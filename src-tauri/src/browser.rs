//! One browser tab for Third Eye (2026-08-30).
//!
//! Every navigation the assistant makes — `web_search`, a grounded
//! `open <url>` — used to go through `/usr/bin/open`, which hands the URL to
//! the default browser as a NEW tab (or a new window). Ten follow-ups meant
//! ten tabs, and "do more on that page" re-opened the page instead of
//! working in the one already showing. Now the first navigation of the
//! process opens ONE tab and remembers its Chrome ids; every later
//! navigation reuses that tab (re-pointing it, or just raising it when it
//! already shows the URL). Chrome only — it is the one browser with
//! scriptable tab ids; any other default browser keeps the `open` path.

use std::sync::Mutex;
use std::time::Duration;

/// (window id, tab id) of the tab Third Eye opened for itself this process.
static OUR_TAB: Mutex<Option<(i64, i64)>> = Mutex::new(None);

/// How long the AppleScript navigation may take (Chrome may be launching).
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(8);
/// The front-tab readback rides every chat request's system turn: bounded
/// tightly so a wedged Chrome never delays an answer.
const FRONT_TAB_TIMEOUT: Duration = Duration::from_millis(1500);

/// Where a navigation landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenedIn {
    /// Third Eye's own tab, re-pointed (or already there) and raised.
    ReusedTab,
    /// A fresh tab that is now Third Eye's own.
    NewTab,
    /// Handed to the default browser via `open` (not Chrome, or scripting failed).
    SystemOpen,
}

impl OpenedIn {
    pub fn as_str(self) -> &'static str {
        match self {
            OpenedIn::ReusedTab => "reused-tab",
            OpenedIn::NewTab => "new-tab",
            OpenedIn::SystemOpen => "system-open",
        }
    }
}

/// AppleScript string-literal escaping (backslash and double quote).
pub fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The reuse-or-open script: re-point the remembered tab when it still
/// exists (skipping the reload when it already shows `url`), else open one
/// new tab in the front window (or a new window when Chrome has none). The
/// reply is `reused,<wid>,<tid>` or `opened,<wid>,<tid>`. Pure.
///
/// Ids are compared AS TEXT: Chrome's window/tab ids are large integers
/// and an integer literal in the script never matches them (verified live
/// 2026-08-30 — every navigation "opened" until the comparison went textual).
pub fn reuse_or_open_script(remembered: Option<(i64, i64)>, url: &str) -> String {
    let target = escape_applescript(url);
    let reuse = match remembered {
        Some((wid, tid)) => format!(
            r#"  repeat with w in windows
    if (id of w as text) is "{wid}" then
      set n to count of tabs of w
      repeat with i from 1 to n
        set t to tab i of w
        if (id of t as text) is "{tid}" then
          if URL of t is not target then set URL of t to target
          set active tab index of w to i
          set index of w to 1
          activate
          return "reused," & (id of w as text) & "," & (id of t as text)
        end if
      end repeat
    end if
  end repeat
"#
        ),
        None => String::new(),
    };
    format!(
        r#"tell application "Google Chrome"
  set target to "{target}"
{reuse}  if (count of windows) is 0 then make new window
  set w to front window
  set t to make new tab at end of tabs of w with properties {{URL:target}}
  set index of w to 1
  activate
  return "opened," & (id of w as text) & "," & (id of t as text)
end tell"#
    )
}

/// Parse the script reply into (how, window id, tab id). Pure.
pub fn parse_navigation_reply(reply: &str) -> Option<(OpenedIn, i64, i64)> {
    let mut parts = reply.trim().split(',');
    let how = match parts.next()? {
        "reused" => OpenedIn::ReusedTab,
        "opened" => OpenedIn::NewTab,
        _ => return None,
    };
    let wid = parts.next()?.trim().parse().ok()?;
    let tid = parts.next()?.trim().parse().ok()?;
    Some((how, wid, tid))
}

/// The remembered tab, if any (tests and diagnostics).
pub fn remembered_tab() -> Option<(i64, i64)> {
    *OUR_TAB.lock().unwrap()
}

/// Forget the remembered tab (tests).
pub fn forget_tab() {
    *OUR_TAB.lock().unwrap() = None;
}

#[cfg(target_os = "macos")]
fn default_browser_is_chrome() -> bool {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};
    let probe = NSURL::URLWithString(&NSString::from_str("https://example.com/"));
    let Some(probe) = probe else {
        return false;
    };
    NSWorkspace::sharedWorkspace()
        .URLForApplicationToOpenURL(&probe)
        .and_then(|u| u.path().map(|p| p.to_string()))
        .is_some_and(|p| p.ends_with("Google Chrome.app"))
}

#[cfg(target_os = "macos")]
pub(crate) fn chrome_running() -> bool {
    crate::appfocus::macos::pid_for_app_name("Google Chrome").is_some()
}

pub(crate) async fn osascript(script: &str, timeout: Duration) -> Result<String, String> {
    let run = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output();
    let output = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| format!("osascript timed out after {timeout:?}"))?
        .map_err(|e| format!("could not run osascript: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "osascript exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn system_open(url: &str) -> Result<(), String> {
    let status = tokio::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .await
        .map_err(|e| format!("could not run open: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open exited {status}"))
    }
}

/// Navigate to `url` in Third Eye's one tab (see module docs). Falls back
/// to the system `open` when the default browser is not Chrome, or when
/// scripting Chrome fails — the navigation must never be the step that dies.
pub async fn open_url(url: &str) -> Result<OpenedIn, String> {
    #[cfg(target_os = "macos")]
    {
        // Chrome not running: `open` launches it with the URL as its first
        // tab; that tab becomes ours once Chrome is up (lazy adopt below).
        if default_browser_is_chrome() && chrome_running() {
            let remembered = remembered_tab();
            match osascript(&reuse_or_open_script(remembered, url), NAVIGATE_TIMEOUT).await {
                Ok(reply) => {
                    if let Some((how, wid, tid)) = parse_navigation_reply(&reply) {
                        *OUR_TAB.lock().unwrap() = Some((wid, tid));
                        log::info!(
                            "browser: {url} → {} (window {wid}, tab {tid})",
                            how.as_str()
                        );
                        return Ok(how);
                    }
                    log::warn!("browser: unexpected navigation reply {reply:?}; using open");
                }
                Err(e) => log::warn!("browser: scripting Chrome failed ({e}); using open"),
            }
        }
        system_open(url).await?;
        if default_browser_is_chrome() {
            // Adopt the tab `open` produced so the NEXT navigation reuses it.
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let adopt = "tell application \"Google Chrome\"\n  if (count of windows) is 0 then return \"\"\n  return (id of front window as text) & \",\" & (id of active tab of front window as text)\nend tell";
            if let Ok(reply) = osascript(adopt, FRONT_TAB_TIMEOUT).await {
                let mut parts = reply.split(',');
                if let (Some(w), Some(t)) = (
                    parts.next().and_then(|v| v.trim().parse::<i64>().ok()),
                    parts.next().and_then(|v| v.trim().parse::<i64>().ok()),
                ) {
                    *OUR_TAB.lock().unwrap() = Some((w, t));
                    log::info!("browser: adopted Chrome window {w}, tab {t} after open");
                }
            }
        }
        Ok(OpenedIn::SystemOpen)
    }
    #[cfg(not(target_os = "macos"))]
    {
        system_open(url).await?;
        Ok(OpenedIn::SystemOpen)
    }
}

/// The page Chrome's front window currently shows — (title, url) — for the
/// chat request's environment grounding. `None` when Chrome is not running
/// (never launches it), has no window, or does not answer in time.
pub async fn front_tab() -> Option<(String, String)> {
    #[cfg(target_os = "macos")]
    {
        if !chrome_running() {
            return None;
        }
        let script = "tell application \"Google Chrome\"\n  if (count of windows) is 0 then return \"\"\n  return (title of active tab of front window) & linefeed & (URL of active tab of front window)\nend tell";
        let reply = osascript(script, FRONT_TAB_TIMEOUT).await.ok()?;
        let (title, url) = reply.split_once('\n')?;
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        Some((title.trim().to_string(), url.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_escaping_covers_quotes_and_backslashes() {
        assert_eq!(
            escape_applescript(r#"https://x.com/?q="a\b""#),
            r#"https://x.com/?q=\"a\\b\""#
        );
    }

    #[test]
    fn first_navigation_opens_one_new_tab_in_the_front_window() {
        let script = reuse_or_open_script(None, "https://www.ebay.com/sch/i.html?_nkw=shoes");
        assert!(script.contains(r#"set target to "https://www.ebay.com/sch/i.html?_nkw=shoes""#));
        assert!(!script.contains("reused"), "nothing to reuse yet");
        assert!(script.contains("make new tab at end of tabs of w with properties {URL:target}"));
        assert!(
            script.contains("if (count of windows) is 0 then make new window"),
            "a windowless Chrome gets exactly one window"
        );
    }

    #[test]
    fn later_navigations_reuse_the_remembered_tab_without_a_reload_when_already_there() {
        let script = reuse_or_open_script(Some((42, 7)), "https://example.com/");
        assert!(script.contains(r#"if (id of w as text) is "42" then"#));
        assert!(script.contains(r#"if (id of t as text) is "7" then"#));
        assert!(
            script.contains("if URL of t is not target then set URL of t to target"),
            "same URL → raise only, no reload"
        );
        assert!(script.contains("set active tab index of w to i"));
        assert!(
            script.contains("set index of w to 1"),
            "the window comes to the front"
        );
        // The reuse block precedes the fallback open, so a live tab wins.
        assert!(script.find("reused").unwrap() < script.find("make new tab").unwrap());
    }

    #[test]
    fn navigation_replies_parse_and_junk_does_not() {
        assert_eq!(
            parse_navigation_reply("reused,42,7\n"),
            Some((OpenedIn::ReusedTab, 42, 7))
        );
        assert_eq!(
            parse_navigation_reply("opened,1718161745,1718161946"),
            Some((OpenedIn::NewTab, 1718161745, 1718161946))
        );
        assert_eq!(parse_navigation_reply(""), None);
        assert_eq!(parse_navigation_reply("error,1,2"), None);
        assert_eq!(parse_navigation_reply("reused,x,2"), None);
    }
}
