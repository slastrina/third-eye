//! Live probe for the 2026-08-30 "opens it twice" report: focus_app twice
//! on Terminal and Chrome must not add windows and must read the front
//! window's title; the one-tab opener must add exactly ONE Chrome tab across
//! three navigations. cargo run --example focus_and_tab_probe

use std::process::Command;
use third_eye_lib::appfocus::macos::MacosAppFocus;
use third_eye_lib::appfocus::AppFocus;
use third_eye_lib::browser;

fn osascript(script: &str) -> String {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .expect("osascript");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
fn windows(app: &str) -> String {
    osascript(&format!("tell application \"{app}\" to count windows"))
}
fn chrome_tabs() -> String {
    osascript("tell application \"Google Chrome\"\nset n to 0\nrepeat with w in windows\nset n to n + (count of tabs of w)\nend repeat\nreturn n\nend tell")
}

#[tokio::main]
async fn main() {
    let focus = MacosAppFocus;
    let mut ok = true;
    for app in ["Terminal", "Google Chrome"] {
        let before = windows(app);
        let r1 = focus.focus(app).await;
        let r2 = focus.focus(app).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let after = windows(app);
        eprintln!("focus {app}: windows before={before} after={after}\n  1st={r1:?}\n  2nd={r2:?}");
        if before != after {
            eprintln!("  !! window count changed");
            ok = false;
        }
        match (&r1, &r2) {
            (Ok(a), Ok(b)) => {
                if a.front_window.is_none() || b.front_window.is_none() {
                    eprintln!("  !! frontWindow missing");
                    ok = false;
                }
                if a.visible_windows == Some(0) || b.visible_windows == Some(0) {
                    eprintln!("  !! visibleWindows 0 with windows present");
                    ok = false;
                }
            }
            _ => ok = false,
        }
    }
    let tabs0 = chrome_tabs();
    let a = browser::open_url("https://example.com/").await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let tabs1 = chrome_tabs();
    let b = browser::open_url("https://example.org/").await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let tabs2 = chrome_tabs();
    let c = browser::open_url("https://example.org/").await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let tabs3 = chrome_tabs();
    let front = browser::front_tab().await;
    eprintln!("tabs: start={tabs0} after#1={tabs1} after#2={tabs2} after#3={tabs3}\n  #1={a:?} #2={b:?} #3={c:?}\n  remembered={:?} front_tab={front:?}", browser::remembered_tab());
    let t0: i64 = tabs0.parse().unwrap_or(-1);
    let t3: i64 = tabs3.parse().unwrap_or(-1);
    if t3 != t0 + 1 {
        eprintln!("  !! expected exactly one new tab");
        ok = false;
    }
    if !matches!(b, Ok(browser::OpenedIn::ReusedTab))
        || !matches!(c, Ok(browser::OpenedIn::ReusedTab))
    {
        eprintln!("  !! later navigations must reuse the tab");
        ok = false;
    }
    if front.as_ref().map(|(_, u)| u.as_str()) != Some("https://example.org/") {
        eprintln!("  !! front_tab should read the navigated page");
        ok = false;
    }
    // Clean up: close the tab we opened.
    if let Some((wid, tid)) = browser::remembered_tab() {
        osascript(&format!("tell application \"Google Chrome\"\nrepeat with w in windows\nif (id of w as text) is \"{wid}\" then\nrepeat with t in tabs of w\nif (id of t as text) is \"{tid}\" then close t\nend repeat\nend if\nend repeat\nend tell"));
    }
    eprintln!("VERDICT: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
