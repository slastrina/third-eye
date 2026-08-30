//! Live probe for the teach-mode "types the command but never presses
//! Enter" incident (2026-08-30). Opens a FRESH Terminal window, then tries
//! both ways a model can submit a command — a `\n` inside `type-text` and a
//! `key-press "return"` — reading the tab buffer back over AppleScript as
//! ground truth. Runs the keyboard hops on a real main queue (dispatch_main)
//! because keyboard synthesis is main-thread only.
//!
//! cargo run --example terminal_return_probe

use std::process::Command;
use std::sync::Arc;
use third_eye_lib::input::macos::MacosInput;
use third_eye_lib::input::{InputAction, InputControl};

#[link(name = "System", kind = "dylib")]
extern "C" {
    fn dispatch_main() -> !;
}

fn osascript(script: &str) -> String {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .expect("osascript");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn buffer() -> String {
    osascript("tell application \"Terminal\" to get contents of selected tab of front window")
}

async fn probe() -> bool {
    osascript("tell application \"Terminal\"\nactivate\ndo script \"\"\nend tell");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let backend: Arc<dyn InputControl> = Arc::new(MacosInput);

    let r1 = backend
        .perform(InputAction::TypeText {
            text: "echo TE_PROBE_NL\n".into(),
        })
        .await;
    eprintln!("type-text+newline report: {r1:?}");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let b1 = buffer();
    // Clean execution: the echo's OUTPUT line is exactly the probe word (no
    // U+200B garbage), and no stray `a` litters the next prompt.
    let nl_executed = b1.lines().any(|l| l == "TE_PROBE_NL");
    let garbage = b1.contains('\u{200b}') || b1.lines().any(|l| l.trim_end().ends_with("~ a"));
    eprintln!("after type-text+newline: executed={nl_executed} garbage={garbage} textEntered={:?}\n---\n{b1}\n---", r1.as_ref().map(|r| r.text_entered));

    let r2 = backend
        .perform(InputAction::TypeText {
            text: "echo TE_PROBE_RET".into(),
        })
        .await;
    eprintln!("type-text report: {r2:?}");
    let r3 = backend
        .perform(InputAction::KeyPress {
            key: "return".into(),
            modifiers: None,
        })
        .await;
    eprintln!("key-press return report: {r3:?}");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let b2 = buffer();
    let ret_executed = b2.lines().any(|l| l == "TE_PROBE_RET");
    eprintln!("after key-press return: executed={ret_executed}\n---\n{b2}\n---");
    eprintln!("VERDICT: newline-in-type-text executed={nl_executed} garbage={garbage}; key-press return executed={ret_executed}");
    osascript("tell application \"Terminal\" to close front window");
    nl_executed && !garbage && ret_executed
}

fn main() {
    std::thread::spawn(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let ok = rt.block_on(probe());
        std::process::exit(if ok { 0 } else { 1 });
    });
    unsafe { dispatch_main() }
}
