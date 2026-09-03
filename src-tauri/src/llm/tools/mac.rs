//! `mac` (S6): the small system services in ONE discriminated tool —
//! notify, speak, system_info, run_shortcut, calendar_today, reminder_add,
//! note_add. Each is a deterministic osascript / CLI call with a typed
//! result; Shortcuts is the extension point (the user's own automations,
//! no Rust). Mutations (shortcut, reminder, note) gate like an action;
//! the first Calendar/Reminders/Notes use triggers macOS's own Automation
//! consent prompt, and a refusal comes back typed permission-denied.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::input::commands::{resolve_approval, ApprovalDecision, HidRunMode, SessionWhitelist};
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const MAC_TOOL: &str = "mac";
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
const SHORTCUT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub macos_version: Option<String>,
    pub battery_percent: Option<u8>,
    pub charging: Option<bool>,
    pub power_source: Option<String>,
    pub volume_percent: Option<u8>,
    pub dark_mode: Option<bool>,
    pub disk_free: Option<String>,
    pub frontmost_app: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub starts: String,
    pub title: String,
    pub calendar: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacError {
    PermissionDenied(String),
    NotFound(String),
    Failed(String),
}

impl MacError {
    pub fn kind(&self) -> &'static str {
        match self {
            MacError::PermissionDenied(_) => "permission-denied",
            MacError::NotFound(_) => "not-found",
            MacError::Failed(_) => "mac-failed",
        }
    }
}

impl std::fmt::Display for MacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MacError::PermissionDenied(d) | MacError::NotFound(d) | MacError::Failed(d) => {
                f.write_str(d)
            }
        }
    }
}

#[async_trait]
pub trait MacServices: Send + Sync {
    async fn notify(&self, title: &str, body: &str) -> Result<(), MacError>;
    async fn speak(&self, text: &str) -> Result<(), MacError>;
    async fn system_info(&self) -> SystemInfo;
    async fn run_shortcut(&self, name: &str, input: Option<&str>) -> Result<String, MacError>;
    async fn calendar_today(&self) -> Result<Vec<CalendarEvent>, MacError>;
    async fn reminder_add(&self, title: &str, due: Option<Due>) -> Result<(), MacError>;
    async fn note_add(&self, title: &str, body: &str) -> Result<(), MacError>;
}

/// A due date-time the model spelled as `YYYY-MM-DD` or `YYYY-MM-DD HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

/// Parse `YYYY-MM-DD[ HH:MM]` (also `T` between). Pure.
pub fn parse_due(s: &str) -> Option<Due> {
    let s = s.trim().replace('T', " ");
    let (date, time) = match s.split_once(' ') {
        Some((d, t)) => (d, Some(t)),
        None => (s.as_str(), None),
    };
    let mut d = date.split('-');
    let year: i32 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hour, minute) = match time {
        Some(t) => {
            let mut p = t.split(':');
            let h: u32 = p.next()?.parse().ok()?;
            let m: u32 = p.next()?.parse().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            (h, m)
        }
        None => (9, 0),
    };
    Some(Due {
        year,
        month,
        day,
        hour,
        minute,
    })
}

fn q(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn applescript_notify(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        q(body),
        q(title)
    )
}

/// A locale-proof AppleScript date: components set one by one.
fn applescript_date(var: &str, due: Due) -> String {
    format!(
        "set {var} to current date\nset year of {var} to {}\nset month of {var} to {}\nset day of {var} to {}\nset hours of {var} to {}\nset minutes of {var} to {}\nset seconds of {var} to 0\n",
        due.year, due.month, due.day, due.hour, due.minute
    )
}

pub fn applescript_reminder(title: &str, due: Option<Due>) -> String {
    match due {
        Some(d) => format!(
            "{}tell application \"Reminders\"\n  make new reminder with properties {{name:\"{}\", due date:dueDate}}\nend tell",
            applescript_date("dueDate", d),
            q(title)
        ),
        None => format!(
            "tell application \"Reminders\"\n  make new reminder with properties {{name:\"{}\"}}\nend tell",
            q(title)
        ),
    }
}

pub fn applescript_note(title: &str, body: &str) -> String {
    // Notes bodies are HTML; the title becomes the first line.
    let html_body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br>");
    format!(
        "tell application \"Notes\"\n  make new note with properties {{name:\"{}\", body:\"<div><h1>{}</h1><div>{}</div></div>\"}}\nend tell",
        q(title),
        q(&title.replace('&', "&amp;").replace('<', "&lt;")),
        q(&html_body)
    )
}

pub fn applescript_calendar_today() -> &'static str {
    "set sep to character id 31\ntell application \"Calendar\"\n  set out to \"\"\n  set startOfDay to (current date) - (time of (current date))\n  set endOfDay to startOfDay + (1 * days)\n  repeat with c in calendars\n    set evs to (every event of c whose start date ≥ startOfDay and start date < endOfDay)\n    repeat with e in evs\n      set out to out & (start date of e as text) & sep & (summary of e) & sep & (name of c) & linefeed\n    end repeat\n  end repeat\n  return out\nend tell"
}

pub fn parse_calendar(reply: &str) -> Vec<CalendarEvent> {
    reply
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\u{1f}');
            Some(CalendarEvent {
                starts: p.next()?.trim().to_string(),
                title: p.next()?.trim().to_string(),
                calendar: p.next().unwrap_or("").trim().to_string(),
            })
        })
        .collect()
}

/// `pmset -g batt` → (percent, charging, source). Pure.
pub fn parse_battery(output: &str) -> (Option<u8>, Option<bool>, Option<String>) {
    let source = output
        .lines()
        .find(|l| l.contains("drawing from"))
        .and_then(|l| l.split('\'').nth(1))
        .map(String::from);
    let line = output.lines().find(|l| l.contains('%'));
    let percent = line
        .and_then(|l| l.split('%').next())
        .and_then(|s| s.split_whitespace().last())
        .and_then(|s| s.parse().ok());
    let charging = line.map(|l| l.contains("; charging") || l.contains("charged"));
    (percent, charging, source)
}

pub fn classify_error(stderr: &str) -> MacError {
    let lower = stderr.to_lowercase();
    if lower.contains("-1743")
        || lower.contains("not authorized")
        || lower.contains("not allowed assistive")
    {
        MacError::PermissionDenied(
            "macOS blocked the request — allow Third Eye to control that app in System Settings → Privacy & Security → Automation, then retry".into(),
        )
    } else if lower.contains("-1728") || lower.contains("can't get") || lower.contains("not found")
    {
        MacError::NotFound(stderr.trim().to_string())
    } else {
        MacError::Failed(stderr.trim().to_string())
    }
}

pub struct SystemMacServices;

impl SystemMacServices {
    async fn osascript(&self, script: &str, timeout: Duration) -> Result<String, MacError> {
        crate::browser::osascript(script, timeout)
            .await
            .map_err(|e| classify_error(&e))
    }
}

#[async_trait]
impl MacServices for SystemMacServices {
    async fn notify(&self, title: &str, body: &str) -> Result<(), MacError> {
        self.osascript(&applescript_notify(title, body), SCRIPT_TIMEOUT)
            .await
            .map(|_| ())
    }
    async fn speak(&self, text: &str) -> Result<(), MacError> {
        // Fire and forget: speech runs while the assistant keeps working.
        tokio::process::Command::new("/usr/bin/say")
            .arg("--")
            .arg(text)
            .spawn()
            .map(|_| ())
            .map_err(|e| MacError::Failed(format!("could not run say: {e}")))
    }
    async fn system_info(&self) -> SystemInfo {
        let run = |cmd: &'static str, args: &'static [&'static str]| async move {
            tokio::process::Command::new(cmd)
                .args(args)
                .output()
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        let (battery, volume, dark, disk, version) = tokio::join!(
            run("/usr/bin/pmset", &["-g", "batt"]),
            run(
                "/usr/bin/osascript",
                &["-e", "output volume of (get volume settings)"]
            ),
            run("/usr/bin/defaults", &["read", "-g", "AppleInterfaceStyle"]),
            run("/bin/df", &["-h", "/"]),
            run("/usr/bin/sw_vers", &["-productVersion"]),
        );
        let (battery_percent, charging, power_source) = battery
            .as_deref()
            .map(parse_battery)
            .unwrap_or((None, None, None));
        SystemInfo {
            macos_version: version.filter(|v| !v.is_empty()),
            battery_percent,
            charging,
            power_source,
            volume_percent: volume.and_then(|v| v.parse().ok()),
            dark_mode: Some(dark.is_some_and(|d| d.trim() == "Dark")),
            disk_free: disk.and_then(|d| {
                d.lines()
                    .nth(1)
                    .and_then(|l| l.split_whitespace().nth(3))
                    .map(|s| format!("{s} free"))
            }),
            frontmost_app: crate::capture::macos::frontmost_app_name(),
        }
    }
    async fn run_shortcut(&self, name: &str, input: Option<&str>) -> Result<String, MacError> {
        let mut cmd = tokio::process::Command::new("/usr/bin/shortcuts");
        cmd.arg("run").arg(name);
        if input.is_some() {
            cmd.arg("-i").arg("-").stdin(std::process::Stdio::piped());
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| MacError::Failed(format!("could not run shortcuts: {e}")))?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        let output = tokio::time::timeout(SHORTCUT_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| MacError::Failed("the shortcut did not finish within 60s".into()))?
            .map_err(|e| MacError::Failed(format!("shortcuts failed: {e}")))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if err.to_lowercase().contains("not find") || err.to_lowercase().contains("no shortcut")
            {
                let list = tokio::process::Command::new("/usr/bin/shortcuts")
                    .arg("list")
                    .output()
                    .await
                    .ok()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .take(30)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                Err(MacError::NotFound(format!(
                    "no shortcut named {name:?}; available: {list}"
                )))
            } else {
                Err(MacError::Failed(err))
            }
        }
    }
    async fn calendar_today(&self) -> Result<Vec<CalendarEvent>, MacError> {
        Ok(parse_calendar(
            &self
                .osascript(applescript_calendar_today(), SCRIPT_TIMEOUT)
                .await?,
        ))
    }
    async fn reminder_add(&self, title: &str, due: Option<Due>) -> Result<(), MacError> {
        self.osascript(&applescript_reminder(title, due), SCRIPT_TIMEOUT)
            .await
            .map(|_| ())
    }
    async fn note_add(&self, title: &str, body: &str) -> Result<(), MacError> {
        self.osascript(&applescript_note(title, body), SCRIPT_TIMEOUT)
            .await
            .map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    action: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    due: Option<String>,
}

pub struct MacTool {
    services: Arc<dyn MacServices>,
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl MacTool {
    pub fn new(
        services: Arc<dyn MacServices>,
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
    ) -> Self {
        Self {
            services,
            mode,
            whitelist,
            approver,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: MAC_TOOL.into(),
            description: "Small macOS services: notify {title, body} (a notification banner), \
                          speak {text} (say it aloud), system_info (battery, power, volume, dark \
                          mode, disk, macOS version, frontmost app), run_shortcut {name, input?} \
                          (run one of the user's Shortcuts and return its output), calendar_today \
                          (today's events), reminder_add {title, due?: YYYY-MM-DD HH:MM}, \
                          note_add {title, body}."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["notify", "speak", "system_info", "run_shortcut", "calendar_today", "reminder_add", "note_add"] },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "text": { "type": "string", "description": "speak: what to say." },
                    "name": { "type": "string", "description": "run_shortcut: the shortcut's exact name." },
                    "input": { "type": "string", "description": "run_shortcut: text passed as the shortcut's input." },
                    "due": { "type": "string", "description": "reminder_add: YYYY-MM-DD or YYYY-MM-DD HH:MM." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn gate(&self, summary: String) -> Result<(), ToolOutcome> {
        let decision = {
            let wl = self.whitelist.lock().unwrap();
            resolve_approval(self.mode, ActionKind::MacService, &wl)
        };
        match decision {
            ApprovalDecision::Refuse => Err(ToolOutcome::failure(
                "disabled",
                "input control is off — the user can enable it in Settings → Automation",
            )),
            ApprovalDecision::Perform => Ok(()),
            ApprovalDecision::Prompt => match self
                .approver
                .request(ActionKind::MacService, summary.clone())
                .await
            {
                ApprovalVerdict::AllowOnce => Ok(()),
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    if let Ok(mut wl) = self.whitelist.lock() {
                        wl.allow(ActionKind::MacService);
                    }
                    Ok(())
                }
                ApprovalVerdict::Deny => Err(ToolOutcome::failure(
                    "approval-denied",
                    format!("the user declined: {summary}"),
                )),
            },
        }
    }
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, ToolOutcome> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolOutcome::failure("invalid-arguments", format!("{what} is required")))
}

#[async_trait]
impl ToolExecutor for MacTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == MAC_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {MAC_TOOL} arguments: {e}"),
                )
            }
        };
        let fail = |e: MacError| ToolOutcome::failure(e.kind(), e.to_string());
        match args.action.as_str() {
            "notify" => {
                let title = match need(&args.title, "title") { Ok(t) => t, Err(o) => return o };
                let body = args.body.as_deref().unwrap_or("").trim();
                match self.services.notify(title, body).await {
                    Ok(()) => ToolOutcome::success(serde_json::json!({ "ok": true, "notified": title }).to_string()),
                    Err(e) => fail(e),
                }
            }
            "speak" => {
                let text = match need(&args.text, "text") { Ok(t) => t, Err(o) => return o };
                match self.services.speak(text).await {
                    Ok(()) => ToolOutcome::success(serde_json::json!({ "ok": true, "speaking": text.chars().take(80).collect::<String>() }).to_string()),
                    Err(e) => fail(e),
                }
            }
            "system_info" => ToolOutcome::success(serde_json::json!({ "ok": true, "system": self.services.system_info().await }).to_string()),
            "run_shortcut" => {
                let name = match need(&args.name, "name") { Ok(t) => t, Err(o) => return o };
                if let Err(r) = self.gate(format!("Run the Shortcut {name:?}")).await {
                    return r;
                }
                match self.services.run_shortcut(name, args.input.as_deref()).await {
                    Ok(out) => ToolOutcome::success(serde_json::json!({ "ok": true, "shortcut": name, "output": out.chars().take(8000).collect::<String>() }).to_string()),
                    Err(e) => fail(e),
                }
            }
            "calendar_today" => match self.services.calendar_today().await {
                Ok(events) => ToolOutcome {
                    result_count: Some(events.len()),
                    ..ToolOutcome::success(serde_json::json!({ "ok": true, "events": events }).to_string())
                },
                Err(e) => fail(e),
            },
            "reminder_add" => {
                let title = match need(&args.title, "title") { Ok(t) => t, Err(o) => return o };
                let due = match args.due.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                    Some(d) => match parse_due(d) {
                        Some(due) => Some(due),
                        None => return ToolOutcome::failure("invalid-arguments", format!("due {d:?} must be YYYY-MM-DD or YYYY-MM-DD HH:MM")),
                    },
                    None => None,
                };
                if let Err(r) = self.gate(format!("Add reminder {title:?}{}", args.due.as_deref().map(|d| format!(" due {d}")).unwrap_or_default())).await {
                    return r;
                }
                match self.services.reminder_add(title, due).await {
                    Ok(()) => ToolOutcome::success(serde_json::json!({ "ok": true, "reminder": title, "due": args.due }).to_string()),
                    Err(e) => fail(e),
                }
            }
            "note_add" => {
                let title = match need(&args.title, "title") { Ok(t) => t, Err(o) => return o };
                let body = args.body.as_deref().unwrap_or("").trim();
                if let Err(r) = self.gate(format!("Add note {title:?}")).await {
                    return r;
                }
                match self.services.note_add(title, body).await {
                    Ok(()) => ToolOutcome::success(serde_json::json!({ "ok": true, "note": title }).to_string()),
                    Err(e) => fail(e),
                }
            }
            other => ToolOutcome::failure(
                "invalid-arguments",
                format!("unknown action {other:?} (notify | speak | system_info | run_shortcut | calendar_today | reminder_add | note_add)"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_dates_parse_with_a_9am_default() {
        assert_eq!(
            parse_due("2026-09-04"),
            Some(Due {
                year: 2026,
                month: 9,
                day: 4,
                hour: 9,
                minute: 0
            })
        );
        assert_eq!(
            parse_due("2026-09-04 17:30"),
            Some(Due {
                year: 2026,
                month: 9,
                day: 4,
                hour: 17,
                minute: 30
            })
        );
        assert_eq!(
            parse_due("2026-09-04T07:05"),
            Some(Due {
                year: 2026,
                month: 9,
                day: 4,
                hour: 7,
                minute: 5
            })
        );
        assert_eq!(parse_due("tomorrow"), None);
        assert_eq!(parse_due("2026-13-01"), None);
        assert_eq!(parse_due("2026-09-04 25:00"), None);
    }

    #[test]
    fn scripts_escape_and_use_component_dates() {
        assert_eq!(
            applescript_notify("Build \"done\"", "all green"),
            "display notification \"all green\" with title \"Build \\\"done\\\"\""
        );
        let r = applescript_reminder("Call mum", parse_due("2026-09-04 17:30"));
        assert!(
            r.contains("set year of dueDate to 2026")
                && r.contains("set hours of dueDate to 17")
                && r.contains("due date:dueDate")
        );
        assert!(applescript_reminder("x", None).contains("{name:\"x\"}"));
        let n = applescript_note("Ideas", "a < b\nline 2");
        assert!(n.contains("a &lt; b<br>line 2"), "{n}");
        assert!(applescript_calendar_today().contains("character id 31"));
    }

    #[test]
    fn battery_and_calendar_parse() {
        let out = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=35324003)\t24%; charging; 2:10 remaining present: true";
        assert_eq!(
            parse_battery(out),
            (Some(24), Some(true), Some("AC Power".into()))
        );
        let out = "Now drawing from 'Battery Power'\n -InternalBattery-0\t87%; discharging; 4:00 remaining";
        assert_eq!(
            parse_battery(out),
            (Some(87), Some(false), Some("Battery Power".into()))
        );
        let evs = parse_calendar("Thursday, 4 September 2026 at 10:00:00\u{1f}Standup\u{1f}Work\n");
        assert_eq!(
            evs,
            vec![CalendarEvent {
                starts: "Thursday, 4 September 2026 at 10:00:00".into(),
                title: "Standup".into(),
                calendar: "Work".into()
            }]
        );
        assert!(matches!(
            classify_error(
                "execution error: Not authorized to send Apple events to Calendar. (-1743)"
            ),
            MacError::PermissionDenied(_)
        ));
    }

    struct Fake(Mutex<Vec<String>>);
    #[async_trait]
    impl MacServices for Fake {
        async fn notify(&self, t: &str, b: &str) -> Result<(), MacError> {
            self.0.lock().unwrap().push(format!("notify {t}|{b}"));
            Ok(())
        }
        async fn speak(&self, t: &str) -> Result<(), MacError> {
            self.0.lock().unwrap().push(format!("speak {t}"));
            Ok(())
        }
        async fn system_info(&self) -> SystemInfo {
            SystemInfo {
                battery_percent: Some(50),
                ..SystemInfo::default()
            }
        }
        async fn run_shortcut(&self, n: &str, i: Option<&str>) -> Result<String, MacError> {
            self.0.lock().unwrap().push(format!("shortcut {n} {i:?}"));
            Ok("42".into())
        }
        async fn calendar_today(&self) -> Result<Vec<CalendarEvent>, MacError> {
            Err(MacError::PermissionDenied("blocked".into()))
        }
        async fn reminder_add(&self, t: &str, d: Option<Due>) -> Result<(), MacError> {
            self.0.lock().unwrap().push(format!("reminder {t} {d:?}"));
            Ok(())
        }
        async fn note_add(&self, t: &str, _b: &str) -> Result<(), MacError> {
            self.0.lock().unwrap().push(format!("note {t}"));
            Ok(())
        }
    }
    struct Prompt(ApprovalVerdict, Mutex<Vec<String>>);
    #[async_trait]
    impl ApprovalPrompt for Prompt {
        async fn request(&self, _k: ActionKind, s: String) -> ApprovalVerdict {
            self.1.lock().unwrap().push(s);
            self.0
        }
    }
    fn tool(mode: HidRunMode, verdict: ApprovalVerdict) -> (MacTool, Arc<Fake>, Arc<Prompt>) {
        let fake = Arc::new(Fake(Mutex::new(vec![])));
        let prompt = Arc::new(Prompt(verdict, Mutex::new(vec![])));
        (
            MacTool::new(
                fake.clone(),
                mode,
                Arc::new(Mutex::new(SessionWhitelist::new())),
                prompt.clone(),
            ),
            fake,
            prompt,
        )
    }
    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: MAC_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn reads_and_notifications_are_free_mutations_ask() {
        let (t, fake, prompt) = tool(HidRunMode::Ask, ApprovalVerdict::AllowOnce);
        assert!(
            t.execute(&call(
                serde_json::json!({"action":"notify","title":"Hi","body":"there"})
            ))
            .await
            .ok
        );
        assert!(
            t.execute(&call(serde_json::json!({"action":"speak","text":"hello"})))
                .await
                .ok
        );
        let info = t
            .execute(&call(serde_json::json!({"action":"system_info"})))
            .await;
        assert!(info.ok && info.content.contains("\"batteryPercent\":50"));
        assert!(prompt.1.lock().unwrap().is_empty(), "no prompts so far");
        let out = t.execute(&call(serde_json::json!({"action":"reminder_add","title":"Call mum","due":"2026-09-04 17:30"}))).await;
        assert!(out.ok, "{out:?}");
        assert_eq!(
            prompt.1.lock().unwrap().as_slice(),
            ["Add reminder \"Call mum\" due 2026-09-04 17:30"]
        );
        assert!(fake
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("reminder Call mum Some(")));
        let out = t
            .execute(&call(
                serde_json::json!({"action":"run_shortcut","name":"Log Water","input":"250ml"}),
            ))
            .await;
        assert!(out.ok && out.content.contains("\"output\":\"42\""));
    }

    #[tokio::test]
    async fn typed_failures_bad_due_permission_and_off() {
        let (t, ..) = tool(HidRunMode::AutoRun, ApprovalVerdict::AllowOnce);
        assert_eq!(
            t.execute(&call(
                serde_json::json!({"action":"reminder_add","title":"x","due":"soon"})
            ))
            .await
            .failure
            .as_deref(),
            Some("invalid-arguments")
        );
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"notify"})))
                .await
                .failure
                .as_deref(),
            Some("invalid-arguments")
        );
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"calendar_today"})))
                .await
                .failure
                .as_deref(),
            Some("permission-denied")
        );
        let (t, fake, _) = tool(HidRunMode::Off, ApprovalVerdict::AllowOnce);
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"note_add","title":"x"})))
                .await
                .failure
                .as_deref(),
            Some("disabled")
        );
        assert!(fake.0.lock().unwrap().is_empty());
    }
}
