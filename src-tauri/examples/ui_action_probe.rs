//! Live probe for ui_action (system tools S2): focus Chrome, set the
//! address bar's value by NAME through accessibility (no navigation), then
//! focus it, printing the readback. cargo run --example ui_action_probe

use third_eye_lib::appfocus::macos::{pid_for_app_name, MacosAppFocus};
use third_eye_lib::appfocus::AppFocus;
use third_eye_lib::screenquery::ax::{perform_ui_action_blocking, AxAct};

#[tokio::main]
async fn main() {
    let focus = MacosAppFocus;
    let fronted = focus.focus("Google Chrome").await.expect("focus Chrome");
    eprintln!("focused: {fronted:?}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let pid = pid_for_app_name(&fronted.app).expect("pid");
    let set = tokio::task::spawn_blocking(move || {
        perform_ui_action_blocking(
            pid,
            "Address and search bar",
            Some("textfield"),
            AxAct::SetValue("example.org — typed by ui_action".into()),
        )
    })
    .await
    .unwrap();
    eprintln!("set_value: {set:?}");
    let focus_it = tokio::task::spawn_blocking(move || {
        perform_ui_action_blocking(pid, "Address and search bar", None, AxAct::Focus)
    })
    .await
    .unwrap();
    eprintln!("focus: {focus_it:?}");
    let missing = tokio::task::spawn_blocking(move || {
        perform_ui_action_blocking(pid, "Definitely Not A Control", None, AxAct::Press)
    })
    .await
    .unwrap();
    eprintln!("missing: {missing:?}");
    let ok = set.as_ref().is_ok_and(|r| {
        r.value_after
            .as_deref()
            .is_some_and(|v| v.contains("example.org"))
    }) && matching_not_found(&missing);
    eprintln!("VERDICT: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}

fn matching_not_found<T>(r: &Result<T, third_eye_lib::screenquery::ax::AxActionError>) -> bool {
    matches!(
        r,
        Err(third_eye_lib::screenquery::ax::AxActionError::NotFound { .. })
    )
}
