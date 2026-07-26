//! Markdown skill packs: the pure, Tauri-free parse + discovery core (M007 S06).
//!
//! A *skill pack* is a `SKILL.md` file whose YAML frontmatter mirrors the
//! `.agents/skills/*/SKILL.md` format the project already ships — a `---`
//! delimited block carrying a required `name` and `description`, followed by a
//! Markdown body that becomes the agent's loaded instructions. Users drop
//! `<name>/SKILL.md` packs into the configurable discovery directory
//! ([`crate::config::resolve_skills_dir`]); this module walks it.
//!
//! Two seams, both unit-testable with no `AppHandle` and no live model
//! (mirroring how [`crate::llm::toolloop`] is deliberately runtime-independent):
//!
//! - [`parse_skill`] — split frontmatter from body, deserialize the YAML, and
//!   validate the required fields. Pure string → [`Skill`] / [`SkillError`].
//! - [`discover_skills`] — walk `<dir>/<name>/SKILL.md`, parse each, and
//!   **skip + log** every malformed or unreadable entry rather than aborting.
//!   A missing directory yields an empty result, never an error.
//!
//! Fail-soft is an acceptance criterion, not a nicety (R006/R007, echoed all
//! over `toolloop.rs`/`config.rs`): a malformed or missing skill file is logged
//! at warn with its offending path and skipped; a successful walk logs the
//! count of loaded skills at info.

use std::path::Path;

use serde::Deserialize;

/// The `SKILL.md` filename every skill pack uses, matching the
/// `.agents/skills/*/SKILL.md` convention.
pub const SKILL_FILE_NAME: &str = "SKILL.md";

/// One parsed skill pack. `name` and `description` come from the required
/// frontmatter fields (the `description` is the documented triggering signal);
/// `body` is the Markdown after the frontmatter — the instructions that load
/// into the agent's context. Optional/unknown frontmatter fields (`license`,
/// etc.) are ignored so the format stays additive, matching the
/// `#[serde(default)]` tolerance [`crate::llm::ChatMessage`] uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// The frontmatter shape we deserialize. Every field is optional at the serde
/// layer so a *missing* required field surfaces as a typed
/// [`SkillError::MissingField`] (skipped + logged) rather than a raw YAML error;
/// unknown fields (`license`, `compatibility`, …) are ignored, keeping the
/// format additive.
#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// The typed skill-parse failure taxonomy (R006), modeled like
/// [`crate::input::InputError`]: a stable `kind()` string for logs. Every
/// variant is non-fatal to discovery — [`discover_skills`] logs it with the
/// offending path and moves on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillError {
    /// The file has no `---`-delimited frontmatter block (no opening delimiter,
    /// or no closing one) — e.g. a plain Markdown file with no metadata.
    MissingFrontmatter { detail: String },
    /// The frontmatter block is present but is not valid YAML.
    MalformedYaml { detail: String },
    /// The YAML parsed, but a required field (`name` / `description`) is absent
    /// or blank. `field` names which one so the log is self-explanatory.
    MissingField { field: &'static str },
}

impl SkillError {
    /// Stable machine-readable name, one token per variant — grep for
    /// `missing-frontmatter` / `malformed-yaml` / `missing-field` in logs.
    pub fn kind(&self) -> &'static str {
        match self {
            SkillError::MissingFrontmatter { .. } => "missing-frontmatter",
            SkillError::MalformedYaml { .. } => "malformed-yaml",
            SkillError::MissingField { .. } => "missing-field",
        }
    }
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::MissingFrontmatter { detail } => {
                write!(f, "missing-frontmatter: {detail}")
            }
            SkillError::MalformedYaml { detail } => {
                write!(f, "malformed-yaml: {detail}")
            }
            SkillError::MissingField { field } => {
                write!(
                    f,
                    "missing-field: required frontmatter field '{field}' is absent or blank"
                )
            }
        }
    }
}

impl std::error::Error for SkillError {}

/// Split a `SKILL.md` string into `(yaml_frontmatter, markdown_body)`. The
/// contract mirrors `.agents/skills/*/SKILL.md`: the file opens with a `---`
/// line, the frontmatter YAML runs until the next `---` line, and everything
/// after is the body. A leading UTF-8 BOM is tolerated. `.lines()` normalizes
/// `\n` and `\r\n`, so both line endings parse. A file with no opening or no
/// closing delimiter is a [`SkillError::MissingFrontmatter`].
fn split_frontmatter(markdown: &str) -> Result<(String, String), SkillError> {
    let normalized = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let mut lines = normalized.lines();

    match lines.next() {
        Some(first) if first.trim_end() == "---" => {}
        _ => {
            return Err(SkillError::MissingFrontmatter {
                detail: "file does not open with a '---' frontmatter delimiter".to_string(),
            })
        }
    }

    let mut yaml = String::new();
    let mut body = String::new();
    let mut closed = false;

    for line in lines {
        if !closed {
            if line.trim_end() == "---" {
                closed = true;
                continue;
            }
            yaml.push_str(line);
            yaml.push('\n');
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }

    if !closed {
        return Err(SkillError::MissingFrontmatter {
            detail: "frontmatter is not closed by a second '---' delimiter".to_string(),
        });
    }

    Ok((yaml, body))
}

/// Parse one `SKILL.md` string into a [`Skill`]. Pure — no I/O. Splits the
/// frontmatter, deserializes the YAML into [`RawFrontmatter`], then validates
/// that both required fields are present and non-blank. Any failure is a typed
/// [`SkillError`] the caller ([`discover_skills`]) logs and skips.
pub fn parse_skill(markdown: &str) -> Result<Skill, SkillError> {
    let (yaml, body) = split_frontmatter(markdown)?;

    let raw: RawFrontmatter =
        serde_yml::from_str(&yaml).map_err(|e| SkillError::MalformedYaml {
            detail: e.to_string(),
        })?;

    let name = non_blank(raw.name).ok_or(SkillError::MissingField { field: "name" })?;
    let description = non_blank(raw.description).ok_or(SkillError::MissingField {
        field: "description",
    })?;

    Ok(Skill {
        name,
        description,
        body: body.trim().to_string(),
    })
}

/// A required string field is usable only when present and non-blank; a blank
/// value is treated as absent (the same "trimmed non-blank" rule
/// [`crate::config`]'s `stored_*` interpreters use).
fn non_blank(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Walk `<dir>/<name>/SKILL.md`, parsing each pack. This is the fail-soft
/// discovery seam: a missing/unreadable directory yields an empty result (not
/// an error), and every malformed or unreadable `SKILL.md` is logged at warn
/// with its offending path and skipped so one bad pack never blocks the rest
/// (R006/R007). A successful walk logs the count of loaded skills at info.
///
/// Skills are returned sorted by `name` for a deterministic injection order.
pub fn discover_skills(dir: &Path) -> Vec<Skill> {
    if !dir.exists() {
        log::info!(
            "skills: discovery dir {} does not exist; no skills loaded",
            dir.display()
        );
        return Vec::new();
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!(
                "skills: cannot read discovery dir {}: {e}; no skills loaded",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut skills = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::warn!(
                    "skills: skipping unreadable entry in {}: {e}",
                    dir.display()
                );
                continue;
            }
        };

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_file = path.join(SKILL_FILE_NAME);
        if !skill_file.exists() {
            // A pack directory with no SKILL.md is not an error — it just isn't
            // a skill. Debug-level so it never noises the warn stream.
            log::debug!(
                "skills: {} has no {SKILL_FILE_NAME}; skipping",
                path.display()
            );
            continue;
        }

        let markdown = match std::fs::read_to_string(&skill_file) {
            Ok(text) => text,
            Err(e) => {
                log::warn!(
                    "skills: cannot read {}: {e}; skipping",
                    skill_file.display()
                );
                continue;
            }
        };

        match parse_skill(&markdown) {
            Ok(skill) => skills.push(skill),
            Err(err) => {
                log::warn!(
                    "skills: skipping {} ({}): {err}",
                    skill_file.display(),
                    err.kind()
                );
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    log::info!(
        "skills: loaded {} skill(s) from {}",
        skills.len(),
        dir.display()
    );
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    const WELL_FORMED: &str = "---\n\
name: error-handling\n\
description: Master error handling patterns. Use when implementing error handling.\n\
---\n\
\n\
# Error Handling\n\
\n\
Build resilient applications.\n";

    #[test]
    fn parses_a_well_formed_skill_into_name_description_body() {
        let skill = parse_skill(WELL_FORMED).expect("well-formed skill must parse");
        assert_eq!(skill.name, "error-handling");
        assert_eq!(
            skill.description,
            "Master error handling patterns. Use when implementing error handling."
        );
        // Body is the Markdown after the frontmatter, trimmed.
        assert_eq!(
            skill.body,
            "# Error Handling\n\nBuild resilient applications."
        );
    }

    #[test]
    fn tolerates_optional_and_unknown_frontmatter_fields() {
        // `license` mirrors the pdf/docx/pptx/xlsx packs; an unknown field must
        // not fail the parse (additive format).
        let md = "---\n\
name: pdf\n\
description: Work with PDF files.\n\
license: Proprietary. LICENSE.txt has complete terms\n\
something_new: 42\n\
---\n\
# PDF\nbody\n";
        let skill = parse_skill(md).expect("optional/unknown fields must not fail the parse");
        assert_eq!(skill.name, "pdf");
        assert_eq!(skill.description, "Work with PDF files.");
    }

    #[test]
    fn tolerates_crlf_line_endings_and_a_leading_bom() {
        let md = "\u{feff}---\r\nname: crlf\r\ndescription: windows line endings\r\n---\r\n# Body\r\ntext\r\n";
        let skill = parse_skill(md).expect("CRLF + BOM must parse");
        assert_eq!(skill.name, "crlf");
        assert_eq!(skill.description, "windows line endings");
        assert_eq!(skill.body, "# Body\ntext");
    }

    #[test]
    fn malformed_yaml_frontmatter_is_a_typed_error_not_a_panic() {
        // A tab-indented mapping value and a dangling colon: invalid YAML.
        let md = "---\nname: bad\ndescription: [unterminated\n---\nbody\n";
        let err = parse_skill(md).expect_err("malformed YAML must error");
        assert_eq!(err.kind(), "malformed-yaml");
    }

    #[test]
    fn missing_required_name_is_skipped_via_missing_field() {
        let md = "---\ndescription: has a description but no name\n---\nbody\n";
        let err = parse_skill(md).expect_err("missing name must error");
        assert_eq!(err, SkillError::MissingField { field: "name" });
        assert_eq!(err.kind(), "missing-field");
    }

    #[test]
    fn missing_required_description_is_skipped_via_missing_field() {
        let md = "---\nname: has-name-only\n---\nbody\n";
        let err = parse_skill(md).expect_err("missing description must error");
        assert_eq!(
            err,
            SkillError::MissingField {
                field: "description"
            }
        );
    }

    #[test]
    fn blank_required_field_counts_as_missing() {
        let md = "---\nname: \"   \"\ndescription: present\n---\nbody\n";
        let err = parse_skill(md).expect_err("blank name must error");
        assert_eq!(err, SkillError::MissingField { field: "name" });
    }

    #[test]
    fn a_file_with_no_frontmatter_is_missing_frontmatter() {
        let md = "# Just a heading\n\nNo metadata block at all.\n";
        let err = parse_skill(md).expect_err("no frontmatter must error");
        assert_eq!(err.kind(), "missing-frontmatter");
    }

    #[test]
    fn an_unclosed_frontmatter_block_is_missing_frontmatter() {
        let md = "---\nname: never-closed\ndescription: no closing delimiter\n# body\n";
        let err = parse_skill(md).expect_err("unclosed frontmatter must error");
        assert_eq!(err.kind(), "missing-frontmatter");
    }

    #[test]
    fn error_kind_is_stable_for_every_variant() {
        assert_eq!(
            SkillError::MissingFrontmatter {
                detail: String::new()
            }
            .kind(),
            "missing-frontmatter"
        );
        assert_eq!(
            SkillError::MalformedYaml {
                detail: String::new()
            }
            .kind(),
            "malformed-yaml"
        );
        assert_eq!(
            SkillError::MissingField { field: "name" }.kind(),
            "missing-field"
        );
    }

    // ---- discovery seam (directory fixtures built in a temp dir) ----

    /// Build an isolated temp directory unique to this test, returning its path.
    /// Cleaned up by the caller with `std::fs::remove_dir_all`.
    fn temp_skills_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "third_eye_skills_test_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp skills root");
        root
    }

    fn write_pack(root: &Path, name: &str, contents: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("create pack dir");
        std::fs::write(dir.join(SKILL_FILE_NAME), contents).expect("write SKILL.md");
    }

    #[test]
    fn discover_returns_empty_for_a_missing_directory() {
        let missing =
            std::env::temp_dir().join(format!("third_eye_skills_absent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(discover_skills(&missing).is_empty());
    }

    #[test]
    fn discover_loads_good_packs_and_skips_malformed_ones() {
        let root = temp_skills_root("mixed");

        write_pack(
            &root,
            "good-one",
            "---\nname: good-one\ndescription: first good skill\n---\n# One\nbody one\n",
        );
        write_pack(
            &root,
            "good-two",
            "---\nname: good-two\ndescription: second good skill\n---\n# Two\nbody two\n",
        );
        // Malformed frontmatter (bad YAML) — must be skipped, not fatal.
        write_pack(
            &root,
            "malformed",
            "---\ndescription: [unterminated\n---\nbody\n",
        );
        // Missing required name — must be skipped.
        write_pack(&root, "no-name", "---\ndescription: nameless\n---\nbody\n");
        // No frontmatter at all — must be skipped.
        write_pack(&root, "plain", "# Just markdown\nno metadata\n");
        // A directory with no SKILL.md — silently ignored, not a skill.
        std::fs::create_dir_all(root.join("empty-dir")).unwrap();

        let skills = discover_skills(&root);

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["good-one", "good-two"],
            "only the good packs load, sorted by name"
        );
        assert_eq!(skills[0].description, "first good skill");
        assert_eq!(skills[0].body, "# One\nbody one");

        std::fs::remove_dir_all(&root).ok();
    }
}
