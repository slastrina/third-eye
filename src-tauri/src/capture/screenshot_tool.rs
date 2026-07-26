//! `take_screenshot` (HID-extensions follow-on, 2026-07-26): let the model
//! SEE the screen — the self-evaluation primitive. screen_query reads text
//! with coordinates; a screenshot shows layout, images, and state that OCR
//! misses, and is how the model verifies a goal is actually met on screen
//! before claiming success.
//!
//! Rides the existing capture backend (Third Eye's own windows excluded,
//! R008) and the SAME privacy gate as every capture: privacy mode on ⇒
//! typed refusal. The image goes to the model as a transient vision turn
//! (toolloop injection) — never stored (R011), never in the transcript UI.
//!
//! Saving is the one exception to "never stored", and it is opt-in per
//! call: `save: true` writes the PNG to the user's Desktop (or a directory
//! the user named) and the result reports the EXACT path — the model can
//! only claim a save that actually happened, to a path it can quote.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use serde::Deserialize;

use crate::capture::ScreenCapture;
use crate::llm::toolloop::{ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const TAKE_SCREENSHOT_TOOL: &str = "take_screenshot";

/// Optional arguments: absent/empty means the pre-save behavior (transient
/// vision turn only). Unknown fields are ignored — a small model inventing
/// an extra key must not fail the capture.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScreenshotArgs {
    #[serde(default)]
    save: bool,
    #[serde(default)]
    directory: Option<String>,
}

/// Where a saved screenshot goes: the named directory (with `~` expanded),
/// or the user's Desktop when none was named. Relative paths are rejected —
/// a save must land where the user can find it, never somewhere relative to
/// the app's working directory.
fn resolve_save_dir(directory: Option<&str>, home: Option<&Path>) -> Result<PathBuf, String> {
    match directory {
        None => home
            .map(|h| h.join("Desktop"))
            .ok_or_else(|| "no home directory to resolve Desktop against".into()),
        Some(dir) => {
            let dir = dir.trim();
            if dir.is_empty() {
                return resolve_save_dir(None, home);
            }
            if let Some(rest) = dir.strip_prefix("~/").or(match dir {
                "~" => Some(""),
                _ => None,
            }) {
                return home
                    .map(|h| h.join(rest))
                    .ok_or_else(|| "no home directory to expand '~' against".into());
            }
            let p = PathBuf::from(dir);
            if p.is_absolute() {
                Ok(p)
            } else {
                Err(format!(
                    "directory must be absolute or start with ~ (got {dir:?})"
                ))
            }
        }
    }
}

/// macOS-convention filename ("Screenshot 2026-07-26 at 13.45.12.png"
/// shape), colon-free so it is legal on every filesystem the user might
/// point at.
fn screenshot_filename(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("Third Eye Screenshot %Y-%m-%d at %H.%M.%S.png")
        .to_string()
}

pub struct ScreenshotTool {
    backend: Arc<dyn ScreenCapture>,
    /// Live privacy-mode read, injected so the tool stays Tauri-free.
    privacy_enabled: Box<dyn Fn() -> bool + Send + Sync>,
}

impl ScreenshotTool {
    pub fn new(
        backend: Arc<dyn ScreenCapture>,
        privacy_enabled: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            backend,
            privacy_enabled,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: TAKE_SCREENSHOT_TOOL.into(),
            description: "Capture the screen as an image you can look at (Third Eye's own \
                          windows are excluded). Use it to VERIFY outcomes: after finishing a \
                          task, take a screenshot and CHECK the goal is really visible before \
                          telling the user it is done. Also use it when screen_query's text is \
                          not enough — layout, images, whether a window is actually showing. \
                          The screenshot arrives as the next message; coordinates for clicking \
                          must still come from screen_query. The image is NOT saved to disk \
                          unless you pass save: true — never tell the user it was saved \
                          otherwise. With save: true it is written as a PNG to the user's \
                          Desktop, or to `directory` if the user named one, and the result \
                          contains the exact saved path — quote that path to the user."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "save": {
                        "type": "boolean",
                        "description": "Save the screenshot as a PNG file the user can keep. \
                                        Pass true only when the user asked to save it. Default false."
                    },
                    "directory": {
                        "type": "string",
                        "description": "save: where to write the file — an absolute path or ~/… . \
                                        Omit for the user's Desktop."
                    }
                }
            }),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ScreenshotTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != TAKE_SCREENSHOT_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!(
                    "unknown tool: {} (available: {TAKE_SCREENSHOT_TOOL})",
                    call.name
                ),
            );
        }
        // Privacy first — never capture, then refuse.
        if (self.privacy_enabled)() {
            return ToolOutcome::failure(
                "privacy-mode",
                "privacy mode is on — screen capture is blocked until the user turns it off",
            );
        }
        // Absent/malformed arguments fall back to the no-save default — the
        // capture itself must never fail on an argument the model invented.
        let args: ScreenshotArgs = serde_json::from_str(&call.arguments).unwrap_or_default();
        // Follow the frontmost window: verifying an app on a secondary
        // monitor means LOOKING at that monitor (multi-display, 2026-07-27).
        match self.backend.capture_frontmost().await {
            Ok(frame) => {
                let saved = if args.save {
                    match save_png(&frame.base64_png, args.directory.as_deref()) {
                        Ok(path) => Some(path),
                        Err(detail) => {
                            return ToolOutcome::failure(
                                "save-failed",
                                format!(
                                    "the screen WAS captured but saving it failed: {detail} — \
                                     tell the user the save did not happen"
                                ),
                            );
                        }
                    }
                } else {
                    None
                };
                let mut outcome = ToolOutcome::success(match &saved {
                    Some(path) => format!(
                        "screenshot captured ({}x{}) and saved to {} — quote this exact path to \
                         the user; it also arrives as the next message",
                        frame.width,
                        frame.height,
                        path.display()
                    ),
                    None => format!(
                        "screenshot captured ({}x{}) — it arrives as the next message. It was \
                         NOT saved to a file (pass save: true if the user wants it kept)",
                        frame.width, frame.height
                    ),
                });
                outcome.attachment_png = Some(frame.base64_png);
                outcome
            }
            Err(e) => ToolOutcome::failure(e.kind(), format!("screenshot failed: {e}")),
        }
    }
}

/// Decode and write the captured PNG into the resolved directory. Every
/// failure is a `String` detail for the typed `save-failed` outcome; the
/// directory must already exist (creating directories the user did not ask
/// for is worse than a clear failure naming the missing one).
fn save_png(base64_png: &str, directory: Option<&str>) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = resolve_save_dir(directory, home.as_deref())?;
    if !dir.is_dir() {
        return Err(format!("directory {} does not exist", dir.display()));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_png)
        .map_err(|e| format!("png decode failed: {e}"))?;
    let path = dir.join(screenshot_filename(chrono::Local::now()));
    std::fs::write(&path, bytes).map_err(|e| format!("writing {} failed: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_dir_defaults_to_desktop_and_expands_tilde() {
        let home = Path::new("/Users/alex");
        assert_eq!(
            resolve_save_dir(None, Some(home)).unwrap(),
            PathBuf::from("/Users/alex/Desktop")
        );
        assert_eq!(
            resolve_save_dir(Some("~/Documents/shots"), Some(home)).unwrap(),
            PathBuf::from("/Users/alex/Documents/shots")
        );
        assert_eq!(
            resolve_save_dir(Some("  "), Some(home)).unwrap(),
            PathBuf::from("/Users/alex/Desktop"),
            "blank directory falls back to the default"
        );
        assert_eq!(
            resolve_save_dir(Some("/tmp/x"), Some(home)).unwrap(),
            PathBuf::from("/tmp/x")
        );
        // Relative paths never silently land next to the binary.
        assert!(resolve_save_dir(Some("shots"), Some(home)).is_err());
        assert!(resolve_save_dir(None, None).is_err());
    }

    #[test]
    fn filename_is_timestamped_and_colon_free() {
        use chrono::TimeZone;
        let t = chrono::Local
            .with_ymd_and_hms(2026, 7, 26, 13, 45, 12)
            .unwrap();
        let name = screenshot_filename(t);
        assert_eq!(name, "Third Eye Screenshot 2026-07-26 at 13.45.12.png");
        assert!(!name.contains(':'));
    }

    #[test]
    fn save_png_writes_the_decoded_bytes_into_an_existing_directory() {
        let dir = std::env::temp_dir().join(format!("te-shot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // "cGl4ZWxz" is base64 for "pixels" — the fake frame the capture
        // tests use.
        let path = save_png("cGl4ZWxz", Some(dir.to_str().unwrap())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"pixels");
        std::fs::remove_dir_all(&dir).unwrap();
        // A missing directory is a clear failure naming it, not a mkdir.
        let missing = dir.join("nope");
        let err = save_png("cGl4ZWxz", Some(missing.to_str().unwrap())).unwrap_err();
        assert!(err.contains("does not exist"));
    }
}
