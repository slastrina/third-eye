//! `find_files` (S5): Spotlight search — by name or content, kind, folder
//! and recency — with structured results. Replaces the model composing
//! `find`/`grep` shell commands, and it is how a person would look.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::llm::toolloop::{ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const FIND_FILES_TOOL: &str = "find_files";
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHit {
    pub path: String,
    pub name: String,
    pub size_bytes: Option<u64>,
    pub modified_at_ms: Option<i64>,
    pub is_dir: bool,
}

/// The search seam: tests stub it, production runs mdfind.
#[async_trait]
pub trait FileSearch: Send + Sync {
    async fn search(&self, query: &SpotlightQuery) -> Result<Vec<PathBuf>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotlightQuery {
    pub text: String,
    pub kind: Option<String>,
    pub root: Option<PathBuf>,
    pub modified_within_days: Option<u32>,
    pub limit: usize,
}

/// The Spotlight predicate for a query: name OR content match, kind and
/// recency constraints. Pure — the whole query language in one place.
pub fn spotlight_predicate(q: &SpotlightQuery) -> String {
    let text = q.text.replace('\\', "\\\\").replace('"', "\\\"");
    let mut parts = vec![format!(
        "(kMDItemFSName == \"*{text}*\"cd || kMDItemTextContent == \"*{text}*\"cd)"
    )];
    if let Some(kind) = q.kind.as_deref().map(|k| k.trim().to_lowercase()) {
        let clause = match kind.as_str() {
            "image" | "images" | "photo" | "photos" => "kMDItemContentTypeTree == \"public.image\"",
            "pdf" => "kMDItemContentType == \"com.adobe.pdf\"",
            "document" | "documents" | "doc" => "(kMDItemContentTypeTree == \"public.composite-content\" || kMDItemContentTypeTree == \"public.text\")",
            "text" | "txt" => "kMDItemContentTypeTree == \"public.text\"",
            "code" | "source" => "kMDItemContentTypeTree == \"public.source-code\"",
            "folder" | "folders" | "directory" => "kMDItemContentType == \"public.folder\"",
            "audio" | "music" => "kMDItemContentTypeTree == \"public.audio\"",
            "video" | "movie" => "kMDItemContentTypeTree == \"public.movie\"",
            "app" | "apps" | "application" => "kMDItemContentType == \"com.apple.application-bundle\"",
            "spreadsheet" | "sheet" => "kMDItemContentTypeTree == \"public.spreadsheet\"",
            "presentation" | "slides" => "kMDItemContentTypeTree == \"public.presentation\"",
            _ => "",
        };
        if !clause.is_empty() {
            parts.push(clause.to_string());
        }
    }
    if let Some(days) = q.modified_within_days {
        parts.push(format!(
            "kMDItemFSContentChangeDate >= $time.today(-{days})"
        ));
    }
    parts.join(" && ")
}

/// Production: `mdfind [-onlyin root] <predicate>`, bounded.
pub struct Mdfind;

#[async_trait]
impl FileSearch for Mdfind {
    async fn search(&self, q: &SpotlightQuery) -> Result<Vec<PathBuf>, String> {
        let mut cmd = tokio::process::Command::new("/usr/bin/mdfind");
        if let Some(root) = &q.root {
            cmd.arg("-onlyin").arg(root);
        }
        cmd.arg(spotlight_predicate(q));
        let output = tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output())
            .await
            .map_err(|_| "Spotlight did not answer in time".to_string())?
            .map_err(|e| format!("could not run mdfind: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "mdfind failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(q.limit)
            .map(PathBuf::from)
            .collect())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    query: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, rename = "in")]
    in_dir: Option<String>,
    #[serde(default)]
    modified_within_days: Option<u32>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct FindFilesTool {
    search: Arc<dyn FileSearch>,
}

impl FindFilesTool {
    pub fn new(search: Arc<dyn FileSearch>) -> Self {
        Self { search }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: FIND_FILES_TOOL.into(),
            description: "Find files on this Mac with Spotlight — by name or words inside them — \
                          optionally narrowed by kind (image, pdf, document, text, code, folder, \
                          audio, video, app, spreadsheet, presentation), a folder to search in \
                          (absolute path), and how recently modified (days). Returns paths with \
                          size and modified time. Use this instead of find/grep/ls commands."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Name fragment or words the file contains." },
                    "kind": { "type": "string", "description": "Optional kind filter (image, pdf, document, text, code, folder, audio, video, app, spreadsheet, presentation)." },
                    "in": { "type": "string", "description": "Optional absolute folder to search within (default: everywhere Spotlight indexes)." },
                    "modifiedWithinDays": { "type": "integer", "description": "Optional: only files changed in the last N days." },
                    "limit": { "type": "integer", "description": "Max results (default 20, max 50)." }
                },
                "required": ["query"]
            }),
        }
    }
}

fn describe(path: PathBuf) -> FileHit {
    let meta = std::fs::metadata(&path).ok();
    FileHit {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        size_bytes: meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
        modified_at_ms: meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
        is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
        path: path.display().to_string(),
    }
}

#[async_trait]
impl ToolExecutor for FindFilesTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == FIND_FILES_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {FIND_FILES_TOOL} arguments: {e}"),
                )
            }
        };
        let text = args.query.trim().to_string();
        if text.is_empty() {
            return ToolOutcome::failure("invalid-arguments", "query must not be empty");
        }
        let root = match args
            .in_dir
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(d) => {
                let p = PathBuf::from(d);
                if !p.is_absolute() {
                    return ToolOutcome::failure(
                        "invalid-arguments",
                        "in must be an absolute folder path",
                    );
                }
                if !p.is_dir() {
                    return ToolOutcome::failure(
                        "not-found",
                        format!("{d} is not an existing folder"),
                    );
                }
                Some(p)
            }
            None => None,
        };
        let q = SpotlightQuery {
            text,
            kind: args.kind,
            root,
            modified_within_days: args.modified_within_days,
            limit: args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        };
        match self.search.search(&q).await {
            Ok(paths) => {
                let hits: Vec<FileHit> = paths.into_iter().map(describe).collect();
                ToolOutcome {
                    result_count: Some(hits.len()),
                    ..ToolOutcome::success(
                        serde_json::json!({
                            "ok": true,
                            "count": hits.len(),
                            "files": hits,
                            "note": if hits.is_empty() { "nothing matched — try fewer words, drop the kind, or a different folder" } else { "" }
                        })
                        .to_string(),
                    )
                }
            }
            Err(e) => ToolOutcome::failure("search-failed", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn predicate_covers_name_or_content_kind_and_recency() {
        let q = SpotlightQuery {
            text: "tax \"2025\"".into(),
            kind: Some("PDF".into()),
            root: None,
            modified_within_days: Some(30),
            limit: 5,
        };
        assert_eq!(
            spotlight_predicate(&q),
            "(kMDItemFSName == \"*tax \\\"2025\\\"*\"cd || kMDItemTextContent == \"*tax \\\"2025\\\"*\"cd) && kMDItemContentType == \"com.adobe.pdf\" && kMDItemFSContentChangeDate >= $time.today(-30)"
        );
        let plain = SpotlightQuery {
            text: "notes".into(),
            kind: Some("mystery".into()),
            root: None,
            modified_within_days: None,
            limit: 5,
        };
        assert_eq!(
            spotlight_predicate(&plain),
            "(kMDItemFSName == \"*notes*\"cd || kMDItemTextContent == \"*notes*\"cd)",
            "unknown kinds add nothing"
        );
    }

    struct Fake(Mutex<Vec<SpotlightQuery>>, Vec<PathBuf>);
    #[async_trait]
    impl FileSearch for Fake {
        async fn search(&self, q: &SpotlightQuery) -> Result<Vec<PathBuf>, String> {
            self.0.lock().unwrap().push(q.clone());
            Ok(self.1.clone())
        }
    }
    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: FIND_FILES_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn results_carry_metadata_and_the_query_is_bounded() {
        let dir = std::env::temp_dir().join(format!("te-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        let fake = Arc::new(Fake(Mutex::new(vec![]), vec![file.clone(), dir.clone()]));
        let t = FindFilesTool::new(fake.clone());
        let out = t
            .execute(&call(
                serde_json::json!({"query":"notes","in": dir.display().to_string(),"limit": 999}),
            ))
            .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.result_count, Some(2));
        assert!(
            out.content.contains("\"sizeBytes\":5") && out.content.contains("\"isDir\":true"),
            "{}",
            out.content
        );
        let q = &fake.0.lock().unwrap()[0];
        assert_eq!(q.limit, MAX_LIMIT);
        assert_eq!(q.root.as_deref(), Some(dir.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bad_arguments_are_typed() {
        let t = FindFilesTool::new(Arc::new(Fake(Mutex::new(vec![]), vec![])));
        assert_eq!(
            t.execute(&call(serde_json::json!({"query":"  "})))
                .await
                .failure
                .as_deref(),
            Some("invalid-arguments")
        );
        assert_eq!(
            t.execute(&call(serde_json::json!({"query":"x","in":"relative/dir"})))
                .await
                .failure
                .as_deref(),
            Some("invalid-arguments")
        );
        assert_eq!(
            t.execute(&call(
                serde_json::json!({"query":"x","in":"/definitely/not/here"})
            ))
            .await
            .failure
            .as_deref(),
            Some("not-found")
        );
        let empty = t.execute(&call(serde_json::json!({"query":"x"}))).await;
        assert!(empty.ok && empty.content.contains("nothing matched"));
    }
}
