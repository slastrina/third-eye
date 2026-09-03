//! `browser` (S3): talk to Chrome's tabs and DOM instead of OCR'ing pixels.
//! tabs / switch / navigate / back need only Chrome's AppleScript
//! dictionary; page_text / find / click / fill run JavaScript in the front
//! tab — which Chrome only allows after the user flips View → Developer →
//! "Allow JavaScript from Apple Events" (a script cannot flip it: the menu
//! item is disabled under automation). Until then those actions fail typed
//! `javascript-disabled` with the exact instruction, and page_text falls
//! back to the accessibility read. Every action returns the front tab's
//! title + URL afterwards — the verified block. Mutations are HID-class.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::input::commands::{resolve_approval, ApprovalDecision, HidRunMode, SessionWhitelist};
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};
use crate::screenquery::ScreenQuery;

pub const BROWSER_TOOL: &str = "browser";
pub const JS_DISABLED_KIND: &str = "javascript-disabled";
pub const JS_DISABLED_FIX: &str =
    "Chrome is not allowing JavaScript from Apple Events. Ask the user to \
    turn it on ONCE: in Chrome's menu bar, View → Developer → Allow JavaScript from Apple Events. \
    Until then use screen_query / read_page / ui_action instead.";
const PAGE_TEXT_MAX: usize = 14_000;
const FIND_MAX: usize = 25;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub id: i64,
    pub window_id: i64,
    pub title: String,
    pub url: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub id: i64,
    pub tag: String,
    pub text: String,
    #[serde(default)]
    pub href: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    NotRunning,
    NoWindow,
    JsDisabled,
    NotFound(String),
    Failed(String),
}

impl BrowserError {
    pub fn kind(&self) -> &'static str {
        match self {
            BrowserError::NotRunning => "not-running",
            BrowserError::NoWindow => "no-window",
            BrowserError::JsDisabled => JS_DISABLED_KIND,
            BrowserError::NotFound(_) => "not-found",
            BrowserError::Failed(_) => "browser-failed",
        }
    }
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserError::NotRunning => {
                f.write_str("Google Chrome is not running — open {app: \"Google Chrome\"} first")
            }
            BrowserError::NoWindow => f.write_str("Chrome has no window open"),
            BrowserError::JsDisabled => f.write_str(JS_DISABLED_FIX),
            BrowserError::NotFound(d) | BrowserError::Failed(d) => f.write_str(d),
        }
    }
}

/// The Chrome seam: tests script it, production drives Chrome.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError>;
    async fn front(&self) -> Result<TabInfo, BrowserError>;
    async fn switch(&self, tab_id: i64) -> Result<TabInfo, BrowserError>;
    async fn navigate(&self, url: &str) -> Result<TabInfo, BrowserError>;
    async fn back(&self) -> Result<TabInfo, BrowserError>;
    async fn page_text(&self) -> Result<String, BrowserError>;
    async fn find(&self, text: &str) -> Result<Vec<Found>, BrowserError>;
    async fn click(&self, id: i64) -> Result<String, BrowserError>;
    async fn fill(&self, id: i64, value: &str) -> Result<String, BrowserError>;
}

// ---------------------------------------------------------------------------
// Pure builders/parsers (unit-tested; the OS never runs in tests)
// ---------------------------------------------------------------------------

/// A JavaScript string literal for `s` (JSON escaping is valid JS).
pub fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Embed `js` in Chrome's `execute … javascript "…"` AppleScript.
pub fn applescript_execute_js(js: &str) -> String {
    let escaped = js.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "tell application \"Google Chrome\"\n  if (count of windows) is 0 then return \"ERR:no-window\"\n  \
         execute active tab of front window javascript \"{escaped}\"\nend tell"
    )
}

pub fn js_page_text() -> String {
    format!("(function(){{return (document.title+'\\n'+(document.body?document.body.innerText:'')).slice(0,{PAGE_TEXT_MAX})}})()")
}

/// Tag every matching interactive element with data-te-id and return them.
pub fn js_find(query: &str) -> String {
    let q = js_string(query);
    format!(
        "(function(q){{q=q.toLowerCase();var sel='a,button,input,textarea,select,[role=button],[role=link],[role=textbox],[role=checkbox],[role=tab],summary,label,h1,h2,h3';\
         document.querySelectorAll('[data-te-id]').forEach(function(e){{e.removeAttribute('data-te-id')}});\
         var out=[],n=0;var all=document.querySelectorAll(sel);\
         for(var i=0;i<all.length&&out.length<{FIND_MAX};i++){{var e=all[i];\
         var t=((e.innerText||e.value||e.placeholder||e.getAttribute('aria-label')||e.title||'')+'').trim();\
         if(!t||t.toLowerCase().indexOf(q)<0)continue;var r=e.getBoundingClientRect();if(r.width<2||r.height<2)continue;\
         n++;e.setAttribute('data-te-id',String(n));out.push({{id:n,tag:e.tagName.toLowerCase(),text:t.slice(0,120),href:e.href||null}});}}\
         return JSON.stringify(out)}})({q})"
    )
}

pub fn js_click(id: i64) -> String {
    format!(
        "(function(id){{var e=document.querySelector('[data-te-id=\"'+id+'\"]');if(!e)return 'ERR:not-found';\
         e.scrollIntoView({{block:'center'}});e.click();return 'ok:'+((e.innerText||e.value||'')+'').trim().slice(0,80)}})({id})"
    )
}

pub fn js_fill(id: i64, value: &str) -> String {
    let v = js_string(value);
    format!(
        "(function(id,v){{var e=document.querySelector('[data-te-id=\"'+id+'\"]');if(!e)return 'ERR:not-found';e.focus();\
         var d=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(e),'value');if(d&&d.set){{d.set.call(e,v)}}else{{e.value=v}}\
         e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));\
         return 'ok:'+(e.value+'').slice(0,80)}})({id},{v})"
    )
}

/// Field separator inside the tabs listing. NOT the tab character: inside
/// `tell application "Google Chrome"`, `tab` names Chrome's tab CLASS and
/// coerces to the literal text "tab" (live probe 2026-09-03).
pub const TAB_FIELD_SEP: char = '\u{1f}';

/// The tabs listing script: one line per tab,
/// `windowId SEP tabId SEP active SEP url SEP title`.
pub fn applescript_tabs() -> String {
    "set sep to character id 31\ntell application \"Google Chrome\"\n  set out to \"\"\n  repeat with w in windows\n    set wi to id of w\n    set ai to active tab index of w\n    set n to count of tabs of w\n    repeat with i from 1 to n\n      set t to tab i of w\n      set out to out & (wi as text) & sep & (id of t as text) & sep & ((i is ai) as text) & sep & (URL of t) & sep & (title of t) & linefeed\n    end repeat\n  end repeat\n  return out\nend tell".to_string()
}

pub fn parse_tabs(reply: &str) -> Vec<TabInfo> {
    reply
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, TAB_FIELD_SEP);
            let window_id = parts.next()?.trim().parse().ok()?;
            let id = parts.next()?.trim().parse().ok()?;
            let active = parts.next()?.trim().eq_ignore_ascii_case("true");
            let url = parts.next()?.trim().to_string();
            let title = parts.next().unwrap_or("").trim().to_string();
            Some(TabInfo {
                id,
                window_id,
                title,
                url,
                active,
            })
        })
        .collect()
}

/// Map an osascript failure to the typed error.
pub fn classify_error(stderr: &str) -> BrowserError {
    let lower = stderr.to_lowercase();
    if lower.contains("turned off")
        || lower.contains("apple events") && lower.contains("javascript")
    {
        BrowserError::JsDisabled
    } else if lower.contains("not running")
        || lower.contains("-600")
        || lower.contains("connection is invalid")
    {
        BrowserError::NotRunning
    } else {
        BrowserError::Failed(stderr.trim().to_string())
    }
}

/// Which of the found elements `text` means: exact match, else the single
/// loose one; ambiguity/absence typed. Pure.
pub fn pick_found(found: &[Found], text: &str) -> Result<i64, BrowserError> {
    let want = text.trim().to_lowercase();
    if let Some(f) = found.iter().find(|f| f.text.trim().to_lowercase() == want) {
        return Ok(f.id);
    }
    let loose: Vec<&Found> = found
        .iter()
        .filter(|f| f.text.to_lowercase().contains(&want))
        .collect();
    match loose.len() {
        1 => Ok(loose[0].id),
        0 => Err(BrowserError::NotFound(format!(
            "nothing on the page matches {text:?}; the page has: {}",
            found
                .iter()
                .take(8)
                .map(|f| format!("{} {:?}", f.tag, f.text))
                .collect::<Vec<_>>()
                .join(" | ")
        ))),
        _ => Err(BrowserError::NotFound(format!(
            "several elements match {text:?} — pass the id from find: {}",
            loose
                .iter()
                .take(8)
                .map(|f| format!("#{} {} {:?}", f.id, f.tag, f.text))
                .collect::<Vec<_>>()
                .join(" | ")
        ))),
    }
}

// ---------------------------------------------------------------------------
// Production backend: Chrome over osascript
// ---------------------------------------------------------------------------

pub struct ChromeBackend;

const SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

impl ChromeBackend {
    async fn run(&self, script: &str) -> Result<String, BrowserError> {
        if !crate::browser::chrome_running() {
            return Err(BrowserError::NotRunning);
        }
        match crate::browser::osascript(script, SCRIPT_TIMEOUT).await {
            Ok(out) if out == "ERR:no-window" => Err(BrowserError::NoWindow),
            Ok(out) => Ok(out),
            Err(e) => Err(classify_error(&e)),
        }
    }

    async fn js(&self, js: &str) -> Result<String, BrowserError> {
        self.run(&applescript_execute_js(js)).await
    }
}

#[async_trait]
impl BrowserBackend for ChromeBackend {
    async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError> {
        Ok(parse_tabs(&self.run(&applescript_tabs()).await?))
    }
    async fn front(&self) -> Result<TabInfo, BrowserError> {
        self.tabs()
            .await?
            .into_iter()
            .find(|t| t.active)
            .ok_or(BrowserError::NoWindow)
    }
    async fn switch(&self, tab_id: i64) -> Result<TabInfo, BrowserError> {
        let script = format!(
            "tell application \"Google Chrome\"\n  repeat with w in windows\n    set n to count of tabs of w\n    repeat with i from 1 to n\n      if (id of tab i of w as text) is \"{tab_id}\" then\n        set active tab index of w to i\n        set index of w to 1\n        activate\n        return \"ok\"\n      end if\n    end repeat\n  end repeat\n  return \"ERR:not-found\"\nend tell"
        );
        match self.run(&script).await?.as_str() {
            "ok" => self.front().await,
            _ => Err(BrowserError::NotFound(format!(
                "no tab with id {tab_id} — browser tabs to list them"
            ))),
        }
    }
    async fn navigate(&self, url: &str) -> Result<TabInfo, BrowserError> {
        crate::browser::open_url(url)
            .await
            .map_err(BrowserError::Failed)?;
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        self.front().await
    }
    async fn back(&self) -> Result<TabInfo, BrowserError> {
        self.run("tell application \"Google Chrome\"\n  if (count of windows) is 0 then return \"ERR:no-window\"\n  go back active tab of front window\n  return \"ok\"\nend tell").await?;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        self.front().await
    }
    async fn page_text(&self) -> Result<String, BrowserError> {
        self.js(&js_page_text()).await
    }
    async fn find(&self, text: &str) -> Result<Vec<Found>, BrowserError> {
        let raw = self.js(&js_find(text)).await?;
        serde_json::from_str(&raw)
            .map_err(|e| BrowserError::Failed(format!("find reply unparsable: {e}")))
    }
    async fn click(&self, id: i64) -> Result<String, BrowserError> {
        let out = self.js(&js_click(id)).await?;
        out.strip_prefix("ok:")
            .map(String::from)
            .ok_or_else(|| BrowserError::NotFound(format!("element #{id} is gone — find again")))
    }
    async fn fill(&self, id: i64, value: &str) -> Result<String, BrowserError> {
        let out = self.js(&js_fill(id, value)).await?;
        out.strip_prefix("ok:")
            .map(String::from)
            .ok_or_else(|| BrowserError::NotFound(format!("element #{id} is gone — find again")))
    }
}

/// Settings → Integrations: can Third Eye script Chrome's pages right now?
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChromeJsStatus {
    pub running: bool,
    /// `None` when Chrome is not running (nothing can be said).
    pub js_enabled: Option<bool>,
    pub detail: String,
}

pub async fn chrome_js_status() -> ChromeJsStatus {
    if !crate::browser::chrome_running() {
        return ChromeJsStatus {
            running: false,
            js_enabled: None,
            detail: "Google Chrome is not running".into(),
        };
    }
    match ChromeBackend.js("'te-ok'").await {
        Ok(v) if v == "te-ok" => ChromeJsStatus {
            running: true,
            js_enabled: Some(true),
            detail: "Chrome allows JavaScript from Apple Events — find / click / fill work on pages".into(),
        },
        Ok(other) => ChromeJsStatus {
            running: true,
            js_enabled: Some(false),
            detail: format!("unexpected reply from Chrome: {other:?}"),
        },
        Err(BrowserError::JsDisabled) => ChromeJsStatus {
            running: true,
            js_enabled: Some(false),
            detail: "Off. Turn it on once in Chrome: View → Developer → Allow JavaScript from Apple Events. (Chrome does not let scripts flip this.)".into(),
        },
        Err(BrowserError::NoWindow) => ChromeJsStatus {
            running: true,
            js_enabled: None,
            detail: "Chrome has no window open — open one and recheck".into(),
        },
        Err(e) => ChromeJsStatus {
            running: true,
            js_enabled: Some(false),
            detail: e.to_string(),
        },
    }
}

#[tauri::command]
pub async fn chrome_js_status_cmd() -> ChromeJsStatus {
    chrome_js_status().await
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    action: String,
    #[serde(default)]
    tab: Option<i64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    value: Option<String>,
}

pub struct BrowserTool {
    backend: Arc<dyn BrowserBackend>,
    /// The accessibility read page_text falls back to when JS is off.
    screen: Arc<dyn ScreenQuery>,
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
    /// Teach mode: reading only — the human way is visible clicks.
    read_only: bool,
}

impl BrowserTool {
    pub fn new(
        backend: Arc<dyn BrowserBackend>,
        screen: Arc<dyn ScreenQuery>,
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
        read_only: bool,
    ) -> Self {
        Self {
            backend,
            screen,
            mode,
            whitelist,
            approver,
            read_only,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: BROWSER_TOOL.into(),
            description: "Work with Google Chrome directly — tabs and page content, no pixels: \
                          tabs (list), switch {tab}, navigate {url} (a URL you were given or read), \
                          back, page_text (the front tab's text), find {text} (matching links, \
                          buttons, fields with ids), click {id | text}, fill {id | text, value}. \
                          Every result carries the front tab's title and url. Prefer find+click \
                          over mouse-click on web pages."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["tabs", "switch", "navigate", "back", "page_text", "find", "click", "fill"] },
                    "tab": { "type": "integer", "description": "switch: the tab id from tabs." },
                    "url": { "type": "string", "description": "navigate: the address to show." },
                    "text": { "type": "string", "description": "find: text to look for; click/fill: the element's text when you have no id." },
                    "id": { "type": "integer", "description": "click/fill: an element id from find." },
                    "value": { "type": "string", "description": "fill: the text to enter." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn gate(&self, summary: String) -> Result<(), ToolOutcome> {
        if self.read_only {
            return Err(ToolOutcome::failure(
                "teach-mode",
                "Teach Me mode: do this the visible way (click/type it yourself with input_action)",
            ));
        }
        let decision = {
            let wl = self.whitelist.lock().unwrap();
            resolve_approval(self.mode, ActionKind::Browser, &wl)
        };
        match decision {
            ApprovalDecision::Refuse => Err(ToolOutcome::failure(
                "disabled",
                "input control is off — the user can enable it in Settings → Automation",
            )),
            ApprovalDecision::Perform => Ok(()),
            ApprovalDecision::Prompt => match self
                .approver
                .request(ActionKind::Browser, summary.clone())
                .await
            {
                ApprovalVerdict::AllowOnce => Ok(()),
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    if let Ok(mut wl) = self.whitelist.lock() {
                        wl.allow(ActionKind::Browser);
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

    async fn resolve_id(&self, args: &Args) -> Result<i64, ToolOutcome> {
        if let Some(id) = args.id {
            return Ok(id);
        }
        let text = args
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let Some(text) = text else {
            return Err(ToolOutcome::failure(
                "invalid-arguments",
                "pass id (from find) or text",
            ));
        };
        let found = self
            .backend
            .find(text)
            .await
            .map_err(|e| ToolOutcome::failure(e.kind(), e.to_string()))?;
        pick_found(&found, text).map_err(|e| ToolOutcome::failure(e.kind(), e.to_string()))
    }

    fn with_front(
        &self,
        body: serde_json::Value,
        front: Result<TabInfo, BrowserError>,
    ) -> ToolOutcome {
        let mut v = body;
        if let (Ok(tab), Some(obj)) = (front, v.as_object_mut()) {
            obj.insert(
                "frontTab".into(),
                serde_json::json!({ "title": tab.title, "url": tab.url, "id": tab.id }),
            );
        }
        ToolOutcome::success(v.to_string())
    }
}

#[async_trait]
impl ToolExecutor for BrowserTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == BROWSER_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {BROWSER_TOOL} arguments: {e}"),
                )
            }
        };
        let fail = |e: BrowserError| ToolOutcome::failure(e.kind(), e.to_string());
        match args.action.as_str() {
            "tabs" => match self.backend.tabs().await {
                Ok(tabs) => ToolOutcome {
                    result_count: Some(tabs.len()),
                    ..ToolOutcome::success(serde_json::json!({ "ok": true, "tabs": tabs }).to_string())
                },
                Err(e) => fail(e),
            },
            "page_text" => match self.backend.page_text().await {
                Ok(text) => self.with_front(serde_json::json!({ "ok": true, "text": text }), self.backend.front().await),
                Err(BrowserError::JsDisabled) => match self.screen.page_text("Google Chrome").await {
                    Some(text) => self.with_front(
                        serde_json::json!({ "ok": true, "text": text, "note": "read through accessibility (JavaScript from Apple Events is off)" }),
                        self.backend.front().await,
                    ),
                    None => fail(BrowserError::JsDisabled),
                },
                Err(e) => fail(e),
            },
            "find" => {
                let Some(text) = args.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
                    return ToolOutcome::failure("invalid-arguments", "find needs text");
                };
                match self.backend.find(text).await {
                    Ok(found) => ToolOutcome {
                        result_count: Some(found.len()),
                        ..self.with_front(serde_json::json!({ "ok": true, "matches": found }), self.backend.front().await)
                    },
                    Err(e) => fail(e),
                }
            }
            "switch" => {
                let Some(tab) = args.tab else {
                    return ToolOutcome::failure("invalid-arguments", "switch needs tab (an id from tabs)");
                };
                if let Err(r) = self.gate(format!("Switch to Chrome tab {tab}")).await {
                    return r;
                }
                match self.backend.switch(tab).await {
                    Ok(t) => self.with_front(serde_json::json!({ "ok": true }), Ok(t)),
                    Err(e) => fail(e),
                }
            }
            "navigate" => {
                let Some(url) = args.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
                    return ToolOutcome::failure("invalid-arguments", "navigate needs url");
                };
                if let Err(r) = self.gate(format!("Navigate Chrome to {url}")).await {
                    return r;
                }
                match self.backend.navigate(url).await {
                    Ok(t) => self.with_front(serde_json::json!({ "ok": true, "opened": url }), Ok(t)),
                    Err(e) => fail(e),
                }
            }
            "back" => {
                if let Err(r) = self.gate("Chrome: go back".into()).await {
                    return r;
                }
                match self.backend.back().await {
                    Ok(t) => self.with_front(serde_json::json!({ "ok": true }), Ok(t)),
                    Err(e) => fail(e),
                }
            }
            "click" => {
                let id = match self.resolve_id(&args).await {
                    Ok(id) => id,
                    Err(r) => return r,
                };
                if let Err(r) = self.gate(format!("Click element #{id} on the page{}", args.text.as_deref().map(|t| format!(" ({t:?})")).unwrap_or_default())).await {
                    return r;
                }
                match self.backend.click(id).await {
                    Ok(label) => {
                        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                        self.with_front(serde_json::json!({ "ok": true, "clicked": label }), self.backend.front().await)
                    }
                    Err(e) => fail(e),
                }
            }
            "fill" => {
                let Some(value) = args.value.clone() else {
                    return ToolOutcome::failure("invalid-arguments", "fill needs value");
                };
                let id = match self.resolve_id(&args).await {
                    Ok(id) => id,
                    Err(r) => return r,
                };
                if let Err(r) = self.gate(format!("Fill element #{id} with {value:?}")).await {
                    return r;
                }
                match self.backend.fill(id, &value).await {
                    Ok(now) => self.with_front(serde_json::json!({ "ok": true, "value": now }), self.backend.front().await),
                    Err(e) => fail(e),
                }
            }
            other => ToolOutcome::failure(
                "invalid-arguments",
                format!("unknown action {other:?} (tabs | switch | navigate | back | page_text | find | click | fill)"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screenquery::{ScreenElement, ScreenQueryError};

    #[test]
    fn tab_listing_parses_and_titles_keep_their_tabs() {
        let s = TAB_FIELD_SEP;
        let reply = format!(
            "1718161745{s}1718161960{s}true{s}https://a.example/{s}A page\n1718161745{s}1718161964{s}false{s}https://b.example/{s}B\tpage\ngarbage line\n"
        );
        let tabs = parse_tabs(&reply);
        assert_eq!(tabs.len(), 2);
        assert_eq!(
            tabs[0],
            TabInfo {
                id: 1718161960,
                window_id: 1718161745,
                title: "A page".into(),
                url: "https://a.example/".into(),
                active: true
            }
        );
        assert_eq!(tabs[1].title, "B\tpage");
        assert!(!tabs[1].active);
    }

    #[test]
    fn javascript_is_embedded_with_escaping_and_strings_are_json() {
        let js = js_find("say \"hi\" \\ there");
        assert!(js.contains(r#"("say \"hi\" \\ there")"#));
        let script = applescript_execute_js("return \"x\\n\"");
        assert!(script.contains(r#"javascript "return \"x\\n\"""#));
        assert!(script.contains("ERR:no-window"));
        assert!(js_click(7).contains("data-te-id=\"'+id+'\"") && js_click(7).ends_with("(7)"));
        assert!(js_fill(3, "a'b").ends_with("(3,\"a'b\")"));
        assert!(js_page_text().contains("innerText"));
    }

    #[test]
    fn errors_classify_and_matches_pick() {
        assert_eq!(
            classify_error(
                "Executing JavaScript through AppleScript is turned off. To turn it on…"
            ),
            BrowserError::JsDisabled
        );
        assert_eq!(
            classify_error("Google Chrome got an error: Connection is invalid. (-600)"),
            BrowserError::NotRunning
        );
        assert!(matches!(classify_error("weird"), BrowserError::Failed(_)));
        let found = vec![
            Found {
                id: 1,
                tag: "a".into(),
                text: "Add to cart".into(),
                href: None,
            },
            Found {
                id: 2,
                tag: "a".into(),
                text: "Add to wishlist".into(),
                href: None,
            },
        ];
        assert_eq!(pick_found(&found, "add to cart"), Ok(1));
        assert_eq!(pick_found(&found, "wishlist"), Ok(2));
        assert!(
            matches!(pick_found(&found, "add to"), Err(BrowserError::NotFound(m)) if m.contains("#1") && m.contains("#2"))
        );
        assert!(
            matches!(pick_found(&found, "checkout"), Err(BrowserError::NotFound(m)) if m.contains("the page has"))
        );
    }

    struct Fake {
        js_on: bool,
        calls: Mutex<Vec<String>>,
    }
    fn front() -> TabInfo {
        TabInfo {
            id: 9,
            window_id: 1,
            title: "eBay".into(),
            url: "https://www.ebay.com/".into(),
            active: true,
        }
    }
    #[async_trait]
    impl BrowserBackend for Fake {
        async fn tabs(&self) -> Result<Vec<TabInfo>, BrowserError> {
            Ok(vec![front()])
        }
        async fn front(&self) -> Result<TabInfo, BrowserError> {
            Ok(front())
        }
        async fn switch(&self, id: i64) -> Result<TabInfo, BrowserError> {
            self.calls.lock().unwrap().push(format!("switch {id}"));
            Ok(front())
        }
        async fn navigate(&self, url: &str) -> Result<TabInfo, BrowserError> {
            self.calls.lock().unwrap().push(format!("navigate {url}"));
            Ok(front())
        }
        async fn back(&self) -> Result<TabInfo, BrowserError> {
            self.calls.lock().unwrap().push("back".into());
            Ok(front())
        }
        async fn page_text(&self) -> Result<String, BrowserError> {
            if self.js_on {
                Ok("DOM text".into())
            } else {
                Err(BrowserError::JsDisabled)
            }
        }
        async fn find(&self, text: &str) -> Result<Vec<Found>, BrowserError> {
            if !self.js_on {
                return Err(BrowserError::JsDisabled);
            }
            self.calls.lock().unwrap().push(format!("find {text}"));
            Ok(vec![Found {
                id: 1,
                tag: "a".into(),
                text: "Buy It Now".into(),
                href: None,
            }])
        }
        async fn click(&self, id: i64) -> Result<String, BrowserError> {
            self.calls.lock().unwrap().push(format!("click {id}"));
            Ok("Buy It Now".into())
        }
        async fn fill(&self, id: i64, v: &str) -> Result<String, BrowserError> {
            self.calls.lock().unwrap().push(format!("fill {id} {v}"));
            Ok(v.into())
        }
    }
    struct AxRead;
    #[async_trait]
    impl ScreenQuery for AxRead {
        async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
            Ok(vec![])
        }
        async fn page_text(&self, _app: &str) -> Option<String> {
            Some("AX text".into())
        }
    }
    struct Allow;
    #[async_trait]
    impl ApprovalPrompt for Allow {
        async fn request(&self, _k: ActionKind, _s: String) -> ApprovalVerdict {
            ApprovalVerdict::AllowOnce
        }
    }
    fn tool(js_on: bool, mode: HidRunMode, read_only: bool) -> (BrowserTool, Arc<Fake>) {
        let fake = Arc::new(Fake {
            js_on,
            calls: Mutex::new(vec![]),
        });
        let t = BrowserTool::new(
            fake.clone(),
            Arc::new(AxRead),
            mode,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(Allow),
            read_only,
        );
        (t, fake)
    }
    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: BROWSER_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn click_by_text_finds_then_clicks_and_reports_the_front_tab() {
        let (t, fake) = tool(true, HidRunMode::AutoRun, false);
        let out = t
            .execute(&call(
                serde_json::json!({"action":"click","text":"buy it now"}),
            ))
            .await;
        assert!(out.ok, "{out:?}");
        assert!(
            out.content.contains("\"frontTab\":{") && out.content.contains("ebay.com"),
            "{}",
            out.content
        );
        assert_eq!(
            fake.calls.lock().unwrap().as_slice(),
            ["find buy it now", "click 1"]
        );
    }

    #[tokio::test]
    async fn page_text_falls_back_to_accessibility_when_js_is_off_but_find_is_typed() {
        let (t, _) = tool(false, HidRunMode::AutoRun, false);
        let out = t
            .execute(&call(serde_json::json!({"action":"page_text"})))
            .await;
        assert!(
            out.ok
                && out.content.contains("AX text")
                && out.content.contains("Apple Events is off"),
            "{}",
            out.content
        );
        let out = t
            .execute(&call(serde_json::json!({"action":"find","text":"x"})))
            .await;
        assert_eq!(out.failure.as_deref(), Some(JS_DISABLED_KIND));
        assert!(out
            .content
            .contains("View → Developer → Allow JavaScript from Apple Events"));
    }

    #[tokio::test]
    async fn mutations_are_gated_reads_are_free_and_teach_is_read_only() {
        let (t, fake) = tool(true, HidRunMode::Off, false);
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"back"})))
                .await
                .failure
                .as_deref(),
            Some("disabled")
        );
        assert!(
            t.execute(&call(serde_json::json!({"action":"tabs"})))
                .await
                .ok,
            "reads never gate"
        );
        assert!(fake.calls.lock().unwrap().is_empty());
        let (t, fake) = tool(true, HidRunMode::AutoRun, true);
        assert_eq!(
            t.execute(&call(
                serde_json::json!({"action":"navigate","url":"https://x.example/"})
            ))
            .await
            .failure
            .as_deref(),
            Some("teach-mode")
        );
        assert!(
            t.execute(&call(serde_json::json!({"action":"page_text"})))
                .await
                .ok
        );
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn argument_shapes_are_validated_typed() {
        let (t, _) = tool(true, HidRunMode::AutoRun, false);
        for args in [
            serde_json::json!({"action":"switch"}),
            serde_json::json!({"action":"navigate"}),
            serde_json::json!({"action":"find"}),
            serde_json::json!({"action":"fill","id":1}),
            serde_json::json!({"action":"click"}),
            serde_json::json!({"action":"dance"}),
        ] {
            assert_eq!(
                t.execute(&call(args.clone())).await.failure.as_deref(),
                Some("invalid-arguments"),
                "{args}"
            );
        }
    }
}
