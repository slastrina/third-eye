//! `wait_for_text` (S1): block until the focused app shows some text — a
//! structural replacement for the model's guessed `wait 500ms` + re-query
//! retries after an app launch or page load. Polls the window-scoped
//! screen read; on a hit the matching elements come back with grounded
//! click coordinates (and the seen-boxes gate is satisfied), on timeout a
//! typed `not-found` says what IS on screen instead.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;

use crate::llm::toolloop::{FocusedApp, ScreenSeen, SeenBox, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};
use crate::screenquery::ScreenQuery;

pub const WAIT_FOR_TEXT_TOOL: &str = "wait_for_text";
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_TIMEOUT_MS: u64 = 20_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    text: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct WaitForTextTool {
    backend: Arc<dyn ScreenQuery>,
    screen_seen: Arc<ScreenSeen>,
    focused_app: Arc<FocusedApp>,
    poll: Duration,
}

impl WaitForTextTool {
    pub fn new(
        backend: Arc<dyn ScreenQuery>,
        screen_seen: Arc<ScreenSeen>,
        focused_app: Arc<FocusedApp>,
    ) -> Self {
        Self {
            backend,
            screen_seen,
            focused_app,
            poll: Duration::from_millis(500),
        }
    }

    /// Test seam: how long between screen reads.
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: WAIT_FOR_TEXT_TOOL.into(),
            description: "Wait until the focused app shows some text (a page finished loading, a \
                          dialog appeared, a command printed its result), then return the \
                          matching on-screen elements with click coordinates. Use this instead \
                          of guessing wait times. Fails typed (not-found) when the text never \
                          appears within timeoutMs (default 5000, max 20000)."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The text to wait for (case-insensitive, substring)." },
                    "timeoutMs": { "type": "integer", "description": "How long to wait, in ms (default 5000, max 20000)." }
                },
                "required": ["text"]
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for WaitForTextTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == WAIT_FOR_TEXT_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {WAIT_FOR_TEXT_TOOL} arguments: {e}"),
                )
            }
        };
        let needle = args.text.trim().to_lowercase();
        if needle.is_empty() {
            return ToolOutcome::failure("invalid-arguments", "text must not be empty");
        }
        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        let started = Instant::now();
        let focused = self.focused_app.current();
        let mut last_count;
        loop {
            let elements = match self.backend.query_scoped(focused.as_deref()).await {
                Ok(e) => self.focused_app.filter(e),
                Err(e) => return ToolOutcome::failure(e.kind(), e.to_string()),
            };
            last_count = elements.len();
            let hits: Vec<_> = elements
                .iter()
                .filter(|el| el.text.to_lowercase().contains(&needle))
                .cloned()
                .collect();
            if !hits.is_empty() {
                // Coordinates the model may now click — the whole read, not
                // just the hits, so the next click is grounded like screen_query.
                self.screen_seen.mark_seen(
                    elements
                        .iter()
                        .map(|el| SeenBox {
                            x: el.x,
                            y: el.y,
                            width: el.width,
                            height: el.height,
                        })
                        .collect(),
                );
                return ToolOutcome {
                    content: serde_json::json!({
                        "ok": true,
                        "found": true,
                        "waitedMs": started.elapsed().as_millis() as u64,
                        "elements": hits,
                    })
                    .to_string(),
                    ok: true,
                    result_count: Some(hits.len()),
                    mode: None,
                    failure: None,
                    attachment_png: None,
                };
            }
            if started.elapsed() >= timeout {
                break;
            }
            tokio::time::sleep(
                self.poll.min(
                    timeout
                        .saturating_sub(started.elapsed())
                        .max(Duration::from_millis(1)),
                ),
            )
            .await;
        }
        ToolOutcome::failure(
            "not-found",
            format!(
                "{:?} did not appear within {} ms — the {} shows {last_count} other element(s); \
                 screen_query to read what is there, or check the app/page you expected",
                args.text.trim(),
                timeout.as_millis(),
                focused
                    .as_deref()
                    .map_or("screen".to_string(), |a| format!("{a} window")),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screenquery::{ScreenElement, ScreenQueryError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Shows the target text from the Nth read on.
    struct Appears {
        after: usize,
        reads: AtomicUsize,
    }
    #[async_trait]
    impl ScreenQuery for Appears {
        async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
            let n = self.reads.fetch_add(1, Ordering::SeqCst);
            let el = |text: &str| ScreenElement {
                text: text.into(),
                x: 10,
                y: 10,
                width: 100,
                height: 20,
                cx: 60,
                cy: 20,
                app: Some("Google Chrome".into()),
                role: None,
            };
            Ok(if n >= self.after {
                vec![el("Loading…"), el("Welcome back, Alex")]
            } else {
                vec![el("Loading…")]
            })
        }
    }

    fn tool(after: usize) -> (WaitForTextTool, Arc<ScreenSeen>) {
        let seen = Arc::new(ScreenSeen::new());
        let focused = Arc::new(FocusedApp::new());
        focused.set("Google Chrome");
        let t = WaitForTextTool::new(
            Arc::new(Appears {
                after,
                reads: AtomicUsize::new(0),
            }),
            seen.clone(),
            focused,
        )
        .with_poll(Duration::from_millis(5));
        (t, seen)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: WAIT_FOR_TEXT_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn returns_the_matching_elements_once_they_appear_and_grounds_clicks() {
        let (t, seen) = tool(3);
        let out = t
            .execute(&call(
                serde_json::json!({"text":"welcome back","timeoutMs":2000}),
            ))
            .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.result_count, Some(1));
        assert!(out.content.contains("Welcome back, Alex"));
        assert!(seen.seen(), "a hit grounds the next click");
    }

    #[tokio::test]
    async fn times_out_typed_naming_what_is_on_screen() {
        let (t, seen) = tool(usize::MAX);
        let out = t
            .execute(&call(
                serde_json::json!({"text":"welcome back","timeoutMs":30}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("not-found"));
        assert!(
            out.content
                .contains("Google Chrome window shows 1 other element"),
            "{}",
            out.content
        );
        assert!(!seen.seen(), "a miss grounds nothing");
    }

    #[tokio::test]
    async fn empty_text_is_invalid() {
        let (t, _) = tool(0);
        let out = t.execute(&call(serde_json::json!({"text":"  "}))).await;
        assert_eq!(out.failure.as_deref(), Some("invalid-arguments"));
    }
}
