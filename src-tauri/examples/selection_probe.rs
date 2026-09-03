//! Live probe for text_selection (system tools S4): a fresh TextEdit
//! document with known text, select all, read the selection through
//! accessibility, replace it, read the document back, then close without
//! saving. cargo run --example selection_probe

use third_eye_lib::input::macos::{selected_text_blocking, set_selected_text_blocking};

fn osascript(script: &str) -> String {
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .expect("osascript");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn main() {
    osascript("tell application \"TextEdit\"\nactivate\nset d to make new document\nset text of d to \"alpha beta gamma\"\nend tell");
    std::thread::sleep(std::time::Duration::from_millis(900));
    osascript("tell application \"System Events\" to keystroke \"a\" using command down");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let got = selected_text_blocking();
    eprintln!(
        "selected: {:?}",
        got.as_ref()
            .map(|(t, f)| (t.clone(), f.app.clone(), f.role.clone()))
    );
    let set = set_selected_text_blocking("delta");
    eprintln!("set: {set:?}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let text = osascript("tell application \"TextEdit\" to get text of front document");
    eprintln!("document now: {text:?}");
    osascript("tell application \"TextEdit\" to close front document saving no");
    let ok = got
        .as_ref()
        .is_some_and(|(t, _)| t.as_deref() == Some("alpha beta gamma"))
        && set == Ok(true)
        && text.trim() == "delta";
    eprintln!("VERDICT: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
