//! Live probe for the browser tool (system tools S3): list Chrome's tabs,
//! read the front tab, switch to it (a no-op switch), go back/forward is
//! skipped (it would change the user's page), and report whether page
//! scripting is enabled. cargo run --example browser_probe

use third_eye_lib::llm::tools::browser::{chrome_js_status, BrowserBackend, ChromeBackend};

#[tokio::main]
async fn main() {
    let b = ChromeBackend;
    let tabs = b.tabs().await;
    eprintln!("tabs: {:?}", tabs.as_ref().map(|t| t.len()));
    let front = b.front().await;
    eprintln!("front: {front:?}");
    let switched = match &front {
        Ok(f) => b.switch(f.id).await,
        Err(e) => Err(e.clone()),
    };
    eprintln!("switch(front): {switched:?}");
    let status = chrome_js_status().await;
    eprintln!("js status: {status:?}");
    let text = b.page_text().await;
    eprintln!(
        "page_text: {:?}",
        text.as_ref()
            .map(|t| t.chars().take(80).collect::<String>())
    );
    let ok = tabs.is_ok() && front.is_ok() && switched.is_ok() && status.running;
    eprintln!("VERDICT: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
