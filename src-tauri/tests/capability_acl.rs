//! MEM115 regression: real-ACL admission proof for cross-webview event
//! delivery. The settings webview once received no backend broadcasts because
//! it was covered by no capability — `listen()` was denied at the IPC layer
//! while custom-command invokes (not ACL-gated) kept working, so live
//! counters froze at boot-time snapshots and no JS-level harness could see it
//! (they mock `listen()`).
//!
//! These tests exercise the *actual* resolved ACL from
//! `tauri::generate_context!()` (the same codegen the shipping app embeds)
//! against the real `plugin:event|listen` IPC command on a mock runtime:
//! admission for the settings webview, and denial for an uncovered label as
//! the negative control proving the ACL bites in this harness.

use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, Manager, WebviewWindow};

fn mock_app() -> App<tauri::test::MockRuntime> {
    mock_builder()
        .build(tauri::generate_context!())
        .expect("mock app with the real generated context")
}

/// The window may already exist (created from tauri.conf.json at build) or
/// not (mock runtime); either way return a live webview with this label.
fn webview(
    app: &App<tauri::test::MockRuntime>,
    label: &str,
) -> WebviewWindow<tauri::test::MockRuntime> {
    match app.get_webview_window(label) {
        Some(w) => w,
        None => tauri::WebviewWindowBuilder::new(app, label, Default::default())
            .build()
            .expect("webview window"),
    }
}

/// The exact IPC request `@tauri-apps/api/event`'s `listen()` submits.
fn listen_request(event: &str) -> InvokeRequest {
    InvokeRequest {
        cmd: "plugin:event|listen".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("local webview origin"),
        body: InvokeBody::Json(serde_json::json!({
            "event": event,
            "target": { "kind": "Any" },
            "handler": 2,
        })),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

/// MEM115 pin: the settings webview must be admitted to `listen()` for the
/// privacy://state broadcast (and thereby every backend event) under the
/// shipping ACL.
#[test]
fn settings_webview_is_admitted_to_listen() {
    let app = mock_app();
    let settings = webview(&app, "settings");
    let response = get_ipc_response(&settings, listen_request("privacy://state"));
    assert!(
        response.is_ok(),
        "settings webview denied plugin:event|listen under the shipping ACL \
         (MEM115 regressed — check src-tauri/capabilities/): {:?}",
        response.err()
    );
}

/// The overlay's admission must keep holding too — it is the reference
/// surface that always worked.
#[test]
fn overlay_webview_is_admitted_to_listen() {
    let app = mock_app();
    let overlay = webview(&app, "overlay");
    let response = get_ipc_response(&overlay, listen_request("overlay://state-changed"));
    assert!(
        response.is_ok(),
        "overlay webview denied plugin:event|listen under the shipping ACL: {:?}",
        response.err()
    );
}

/// Negative control: a webview whose label appears in no capability must be
/// denied — proving these tests exercise the real ACL rather than passing
/// vacuously.
#[test]
fn uncovered_webview_is_denied_listen() {
    let app = mock_app();
    let rogue = webview(&app, "rogue");
    let response = get_ipc_response(&rogue, listen_request("privacy://state"));
    assert!(
        response.is_err(),
        "a capability-less webview was admitted to plugin:event|listen — the \
         ACL is not being enforced in this harness, so the admission tests \
         above prove nothing"
    );
}
