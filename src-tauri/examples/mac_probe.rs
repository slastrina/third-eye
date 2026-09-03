//! Live probe for the mac tool (S6): system_info and a notification. The
//! Calendar/Reminders/Notes actions are NOT exercised here — their first
//! use raises macOS's Automation consent prompt, which belongs to the user.
//! cargo run --example mac_probe

use third_eye_lib::llm::tools::mac::{MacServices, SystemMacServices};

#[tokio::main]
async fn main() {
    let s = SystemMacServices;
    let info = s.system_info().await;
    eprintln!("system_info: {info:?}");
    let n = s
        .notify("Third Eye", "mac tool probe — notifications work")
        .await;
    eprintln!("notify: {n:?}");
    let ok = info.macos_version.is_some() && info.battery_percent.is_some() && n.is_ok();
    eprintln!("VERDICT: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
