//! Auto lane selection (coding-agent S1, 2026-08-01): pure heuristics that
//! pick a lane per request when the router mode is AUTO.
//!
//! Deterministic and explainable by design — no triage LLM call (the user
//! chose zero-latency heuristics): code-shaped asks route to the coder
//! lane, computer-task-shaped asks to heavy, everything else to thin.
//! Stickiness is the CALLER's job (an escalated conversation stays
//! escalated until ＋New / resume resets the lock) — this module only
//! scores one request.

use super::router::{CODER_LANE, HEAVY_LANE, THIN_LANE};

/// Signals that the user wants CODE written/changed/understood — the coder
/// lane's work. Substring match on the lowercased ask; multi-word entries
/// keep single common words ("test", "file") from over-triggering.
const CODE_SIGNALS: &[&str] = &[
    "code",
    "coding",
    "function",
    "refactor",
    "compile",
    "debug",
    "implement",
    "script",
    "typescript",
    "javascript",
    "python",
    "rust",
    "swift",
    "kotlin",
    "unit test",
    "write a test",
    // 2026-08-02: "write me a small app in my workspace" routed THIN — the
    // list lacked the app/workspace phrasings real asks use.
    "workspace",
    "an app",
    "small app",
    "the app",
    "my app",
    "app that",
    "app to",
    "a program",
    "the program",
    "algorithm",
    "write code",
    "codebase",
    "cli tool",
    "command-line",
    "fix the bug",
    "fix this bug",
    "stack trace",
    "exception",
    "regex",
    "sql",
    "api endpoint",
    "cargo ",
    "npm ",
    "git ",
    "pull request",
    "repository",
    "repo ",
    "```",
    ".rs",
    ".py",
    ".ts",
    ".tsx",
    ".js",
    ".go",
    ".java",
    ".cpp",
    ".json",
    ".yaml",
    ".toml",
];

/// Signals that the user wants the COMPUTER driven — heavy-lane work (the
/// tool-discipline flows: browse, click, type, find on screen).
const TASK_SIGNALS: &[&str] = &[
    "open ",
    "click",
    "search for",
    "search the",
    "find me",
    "look up",
    "browse",
    "browser",
    "chrome",
    "safari",
    "ebay",
    "google",
    "website",
    "webpage",
    "on screen",
    "screenshot",
    "type in",
    "scroll",
    "install ",
    "launch ",
    "play ",
    "download",
    "on the page",
    "this page",
    "read the page",
];

/// Pick a lane for one request. `locked` is the conversation's sticky
/// escalation (heavy/coder from an earlier request) and always wins —
/// a task's quick follow-up must not drop back to the 9B mid-task.
pub fn select_lane(ask: &str, locked: Option<&str>) -> &'static str {
    if let Some(lane) = locked {
        // Normalize to the static names; unknown locks fall through.
        match lane {
            "coder" => return CODER_LANE,
            "heavy" => return HEAVY_LANE,
            _ => {}
        }
    }
    let ask = ask.to_lowercase();
    // Code beats task when both match ("open the repo and fix the bug" is
    // coding work even though "open" is a task verb).
    if CODE_SIGNALS.iter().any(|s| ask.contains(s)) {
        return CODER_LANE;
    }
    if TASK_SIGNALS.iter().any(|s| ask.contains(s)) {
        return HEAVY_LANE;
    }
    THIN_LANE
}

/// Whether an auto-routed lane should LOCK the conversation there
/// (stickiness): escalations stick, thin never does.
pub fn locks_conversation(lane: &str) -> bool {
    lane == HEAVY_LANE || lane == CODER_LANE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_routes_thin_tasks_heavy_code_coder() {
        assert_eq!(select_lane("what is 2 plus 2", None), THIN_LANE);
        assert_eq!(select_lane("whats my name?", None), THIN_LANE);
        assert_eq!(
            select_lane("can you find me half life 2 on ebay", None),
            HEAVY_LANE
        );
        assert_eq!(
            select_lane("open chrome and search for lasagne", None),
            HEAVY_LANE
        );
        assert_eq!(
            select_lane("write a function that parses ISO dates", None),
            CODER_LANE
        );
        assert_eq!(
            select_lane("Refactor the ingest loop and fix the bug", None),
            CODER_LANE
        );
        // The thin-routed incident phrasings (2026-08-02).
        assert_eq!(
            select_lane(
                "can you write in my workspace a small app to calculate pi to 30 places",
                None
            ),
            CODER_LANE
        );
        assert_eq!(
            select_lane("build an app that tracks my reading list", None),
            CODER_LANE
        );
        assert_eq!(
            select_lane("implement the sorting algorithm", None),
            CODER_LANE
        );
    }

    #[test]
    fn code_beats_task_when_both_match() {
        assert_eq!(
            select_lane("open my repo and implement the parser in rust", None),
            CODER_LANE
        );
    }

    #[test]
    fn sticky_lock_wins_over_the_ask_text() {
        // Mid-task follow-ups are short and unsignalled — the lock carries.
        assert_eq!(select_lane("now the pc version", Some("heavy")), HEAVY_LANE);
        assert_eq!(select_lane("and add docs", Some("coder")), CODER_LANE);
        // An unknown lock is ignored, never trusted.
        assert_eq!(select_lane("hello", Some("bogus")), THIN_LANE);
    }

    #[test]
    fn only_escalations_lock() {
        assert!(locks_conversation(HEAVY_LANE));
        assert!(locks_conversation(CODER_LANE));
        assert!(!locks_conversation(THIN_LANE));
        assert!(!locks_conversation("bogus"));
    }
}
