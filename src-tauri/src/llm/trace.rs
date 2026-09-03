//! Run traces (2026-09-03 review item 1): what one chat run actually did —
//! every tool call with its arguments, outcome kind, timing and `verified`
//! readback, plus lane/model/usage — kept for the last few runs and
//! rendered as a markdown report the user can copy into a bug report.
//! "It kept typing the command" becomes a paste, not a reconstruction.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;

use super::toolloop::ToolEvent;

/// Runs retained in memory (newest first).
pub const RUN_TRACES_CAP: usize = 20;
const ARGS_MAX_CHARS: usize = 500;
const ASK_MAX_CHARS: usize = 300;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceStep {
    pub round: usize,
    pub name: String,
    pub args: String,
    /// `None` when the run ended before the result arrived.
    pub ok: Option<bool>,
    pub failure: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub verified: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTrace {
    pub request_id: u64,
    pub started_at_ms: i64,
    pub ask: String,
    pub lane: String,
    pub model: Option<String>,
    pub teach: bool,
    pub steps: Vec<TraceStep>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_ms: u64,
    /// `done` / `stopped` / an error kind.
    pub end: String,
    pub answer_chars: usize,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}… [{} chars]", s.chars().count())
    }
}

/// Pair every Call with its Result (by call id) into ordered steps. Pure.
pub fn steps_from_events(events: &[ToolEvent]) -> Vec<TraceStep> {
    let mut steps: Vec<(String, TraceStep)> = Vec::new();
    for event in events {
        match event {
            ToolEvent::Call(c) => steps.push((
                c.call.id.clone(),
                TraceStep {
                    round: c.round,
                    name: c.call.name.clone(),
                    args: clip(c.call.arguments.trim(), ARGS_MAX_CHARS),
                    ok: None,
                    failure: None,
                    elapsed_ms: None,
                    verified: None,
                    error: None,
                },
            )),
            ToolEvent::Result(r) => {
                if let Some((_, step)) = steps.iter_mut().rev().find(|(id, _)| *id == r.call_id) {
                    step.ok = Some(r.ok);
                    step.failure = r.failure.clone();
                    step.elapsed_ms = Some(r.elapsed_ms);
                    step.verified = r.verified.clone();
                    step.error = r.error.clone();
                }
            }
        }
    }
    steps.into_iter().map(|(_, s)| s).collect()
}

impl RunTrace {
    pub fn new(
        request_id: u64,
        ask: &str,
        lane: &str,
        model: Option<String>,
        teach: bool,
        events: &[ToolEvent],
    ) -> Self {
        Self {
            request_id,
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            ask: clip(ask, ASK_MAX_CHARS),
            lane: lane.to_string(),
            model,
            teach,
            steps: steps_from_events(events),
            prompt_tokens: None,
            completion_tokens: None,
            total_ms: 0,
            end: "done".into(),
            answer_chars: 0,
        }
    }

    /// The copyable report. Pure.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# Third Eye run #{} — {}\n\n",
            self.request_id,
            match self.end.as_str() {
                "done" => "finished".to_string(),
                "stopped" => "stopped by the user".to_string(),
                kind => format!("failed: {kind}"),
            }
        ));
        out.push_str(&format!("- ask: {}\n", self.ask.replace('\n', " ")));
        out.push_str(&format!(
            "- lane: {}{} · model: {}\n",
            self.lane,
            if self.teach { " (teach mode)" } else { "" },
            self.model.as_deref().unwrap_or("endpoint default")
        ));
        out.push_str(&format!(
            "- {} tool step(s), {} ms total, tokens ↑{} ↓{}, answer {} chars\n\n",
            self.steps.len(),
            self.total_ms,
            self.prompt_tokens
                .map_or("?".to_string(), |t| t.to_string()),
            self.completion_tokens
                .map_or("?".to_string(), |t| t.to_string()),
            self.answer_chars
        ));
        if !self.steps.is_empty() {
            out.push_str("| # | round | tool | args | result | ms |\n|---|---|---|---|---|---|\n");
            for (i, s) in self.steps.iter().enumerate() {
                let result = match (s.ok, &s.failure) {
                    (None, _) => "(no result)".to_string(),
                    (Some(true), _) => "ok".to_string(),
                    (Some(false), Some(kind)) => format!("FAIL {kind}"),
                    (Some(false), None) => "FAIL".to_string(),
                };
                out.push_str(&format!(
                    "| {} | {} | {} | `{}` | {} | {} |\n",
                    i + 1,
                    s.round,
                    s.name,
                    s.args.replace('|', "\\|").replace('\n', " "),
                    result,
                    s.elapsed_ms.map_or("-".to_string(), |ms| ms.to_string())
                ));
            }
            let details: Vec<String> = self
                .steps
                .iter()
                .enumerate()
                .filter(|(_, s)| s.verified.is_some() || s.error.is_some())
                .map(|(i, s)| {
                    let mut d = format!("- step {}: ", i + 1);
                    if let Some(v) = &s.verified {
                        d.push_str(&format!("verified {v}"));
                    }
                    if let Some(e) = &s.error {
                        if s.verified.is_some() {
                            d.push_str(" — ");
                        }
                        d.push_str(&format!("error: {}", e.replace('\n', " ")));
                    }
                    d
                })
                .collect();
            if !details.is_empty() {
                out.push_str("\nDetails:\n");
                out.push_str(&details.join("\n"));
                out.push('\n');
            }
        }
        out
    }
}

/// The last few runs, newest first.
#[derive(Default)]
pub struct RunTraces(Mutex<VecDeque<RunTrace>>);

impl RunTraces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, trace: RunTrace) {
        if let Ok(mut runs) = self.0.lock() {
            runs.push_front(trace);
            runs.truncate(RUN_TRACES_CAP);
        }
    }

    pub fn get(&self, request_id: u64) -> Option<RunTrace> {
        self.0
            .lock()
            .ok()?
            .iter()
            .find(|t| t.request_id == request_id)
            .cloned()
    }

    pub fn latest(&self) -> Option<RunTrace> {
        self.0.lock().ok()?.front().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::toolloop::{ToolCallEvent, ToolResultEvent};
    use crate::llm::ToolCall;

    fn call(id: &str, name: &str, args: &str, round: usize) -> ToolEvent {
        ToolEvent::Call(ToolCallEvent {
            request_id: 7,
            round,
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            },
        })
    }

    fn result(id: &str, name: &str, ok: bool, failure: Option<&str>, ms: u64) -> ToolEvent {
        ToolEvent::Result(ToolResultEvent {
            request_id: 7,
            round: 0,
            call_id: id.into(),
            name: name.into(),
            ok,
            result_count: None,
            mode: None,
            failure: failure.map(String::from),
            preview: None,
            elapsed_ms: ms,
            verified: (name == "input_action").then(|| serde_json::json!({"textEntered": false})),
            error: (!ok).then(|| "boom".to_string()),
        })
    }

    #[test]
    fn steps_pair_calls_with_results_by_id() {
        let events = vec![
            call("a", "focus_app", r#"{"app":"Terminal"}"#, 0),
            result("a", "focus_app", true, None, 120),
            call(
                "b",
                "input_action",
                r#"{"action":"type-text","text":"ls\n"}"#,
                1,
            ),
            result("b", "input_action", false, Some("verification-failed"), 800),
            call("c", "screen_query", "{}", 2), // never answered (run stopped)
        ];
        let steps = steps_from_events(&events);
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].ok, Some(true));
        assert_eq!(steps[0].elapsed_ms, Some(120));
        assert_eq!(steps[1].failure.as_deref(), Some("verification-failed"));
        assert_eq!(
            steps[1].verified,
            Some(serde_json::json!({"textEntered": false}))
        );
        assert_eq!(steps[1].error.as_deref(), Some("boom"));
        assert_eq!(steps[2].ok, None, "unanswered call stays open");
    }

    #[test]
    fn report_reads_like_a_bug_report() {
        let events = vec![
            call("a", "focus_app", r#"{"app":"Terminal"}"#, 0),
            result("a", "focus_app", true, None, 120),
            call(
                "b",
                "input_action",
                r#"{"action":"type-text","text":"ls | wc"}"#,
                1,
            ),
            result("b", "input_action", false, Some("verification-failed"), 800),
        ];
        let mut t = RunTrace::new(
            7,
            "run ls in terminal",
            "heavy",
            Some("qwen-9b".into()),
            true,
            &events,
        );
        t.total_ms = 4321;
        t.prompt_tokens = Some(6000);
        t.completion_tokens = Some(90);
        t.end = "stopped".into();
        let md = t.render_markdown();
        assert!(md.starts_with("# Third Eye run #7 — stopped by the user"));
        assert!(md.contains("- lane: heavy (teach mode) · model: qwen-9b"));
        assert!(md.contains("2 tool step(s), 4321 ms total, tokens ↑6000 ↓90"));
        assert!(md.contains("| 1 | 0 | focus_app | `{\"app\":\"Terminal\"}` | ok | 120 |"));
        assert!(md.contains("FAIL verification-failed | 800 |"));
        assert!(
            md.contains("ls \\| wc"),
            "pipes are escaped inside the table"
        );
        assert!(md.contains("step 2: verified {\"textEntered\":false} — error: boom"));
    }

    #[test]
    fn long_args_and_asks_are_clipped_with_the_true_length() {
        let big = "x".repeat(2000);
        let events = vec![call("a", "write_file", &big, 0)];
        let t = RunTrace::new(1, &big, "thin", None, false, &events);
        assert!(t.steps[0].args.ends_with("… [2000 chars]"));
        assert!(t.ask.ends_with("… [2000 chars]"));
        assert_eq!(
            t.ask.chars().count(),
            ASK_MAX_CHARS + "… [2000 chars]".chars().count()
        );
    }

    #[test]
    fn traces_keep_the_newest_cap_and_find_by_id() {
        let traces = RunTraces::new();
        for i in 0..(RUN_TRACES_CAP as u64 + 5) {
            traces.push(RunTrace::new(i, "a", "thin", None, false, &[]));
        }
        assert_eq!(
            traces.latest().unwrap().request_id,
            RUN_TRACES_CAP as u64 + 4
        );
        assert!(traces.get(0).is_none(), "oldest evicted");
        assert!(traces.get(RUN_TRACES_CAP as u64).is_some());
    }
}
