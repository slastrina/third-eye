//! LM Studio native REST probes (`/api/v0`, 2026-08-02): the richer
//! backend surface behind the OpenAI-compatible endpoint — per-model load
//! `state` (`loaded` / `not-loaded` / `loading`), declared `capabilities`
//! (`tool_use`), quantization, and context length.
//!
//! Read-only and LOOPBACK-ONLY: [`native_base`] refuses to derive a native
//! base for any non-local endpoint, so these probes can never reach a
//! cloud provider (R016 posture — cloud lanes simply skip the model-state
//! phase). Every call is bounded and health-as-value: any failure returns
//! `None`/empty and the caller degrades to the generic status.

use serde::{Deserialize, Serialize};

/// One served model as `/api/v0/models` reports it. Extra fields the
/// probe does not understand are ignored (LM Studio adds fields freely).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LmModelRow {
    pub id: String,
    /// "loaded" / "not-loaded" / "loading" (verbatim from LM Studio).
    #[serde(default)]
    pub state: String,
    /// Whether the model declares the `tool_use` capability — pinning a
    /// model without it (the gemma incident) can be warned about BEFORE
    /// every chat 400s.
    #[serde(default)]
    pub tool_use: bool,
    #[serde(default)]
    pub quantization: Option<String>,
    #[serde(default)]
    pub max_context_length: Option<u64>,
}

/// Derive the native API base from the OpenAI-compatible endpoint —
/// loopback only. `http://localhost:1234/v1` → `http://localhost:1234`.
pub fn native_base(endpoint: &str) -> Option<String> {
    let lower = endpoint.to_ascii_lowercase();
    let local = lower.starts_with("http://localhost")
        || lower.starts_with("http://127.")
        || lower.starts_with("http://[::1]");
    if !local {
        return None;
    }
    Some(
        endpoint
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string(),
    )
}

/// Fetch every served model's row. `None` on any failure (endpoint not LM
/// Studio, older version without `/api/v0`, network hiccup) — callers
/// degrade, never error.
pub async fn model_rows(endpoint: &str) -> Option<Vec<LmModelRow>> {
    let base = native_base(endpoint)?;
    let response = reqwest::Client::new()
        .get(format!("{base}/api/v0/models"))
        .timeout(std::time::Duration::from_millis(1500))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: serde_json::Value = response.json().await.ok()?;
    let rows = value.get("data")?.as_array()?;
    Some(rows.iter().filter_map(parse_row).collect())
}

/// Parse one `/api/v0/models` entry (tolerant: only `id` is required).
fn parse_row(row: &serde_json::Value) -> Option<LmModelRow> {
    Some(LmModelRow {
        id: row.get("id")?.as_str()?.to_string(),
        state: row
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        tool_use: row
            .get("capabilities")
            .and_then(|c| c.as_array())
            .map(|caps| caps.iter().any(|c| c.as_str() == Some("tool_use")))
            .unwrap_or(false),
        quantization: row
            .get("quantization")
            .and_then(|q| q.as_str())
            .map(String::from),
        max_context_length: row.get("max_context_length").and_then(|m| m.as_u64()),
    })
}

/// One model's load state, if the native API answers. Used by the phase
/// poller to distinguish "loading the model" from "processing the prompt".
pub async fn model_state(endpoint: &str, model_id: &str) -> Option<String> {
    let rows = model_rows(endpoint).await?;
    rows.into_iter()
        .find(|row| row.id == model_id)
        .map(|row| row.state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_base_is_loopback_only() {
        assert_eq!(
            native_base("http://localhost:1234/v1"),
            Some("http://localhost:1234".into())
        );
        assert_eq!(
            native_base("http://127.0.0.1:1234/v1/"),
            Some("http://127.0.0.1:1234".into())
        );
        // Cloud endpoints never get probed (R016).
        assert_eq!(native_base("https://api.openai.com/v1"), None);
        assert_eq!(native_base("http://192.168.1.10:1234/v1"), None);
    }

    #[test]
    fn rows_parse_state_and_capabilities() {
        let row = serde_json::json!({
            "id": "qwen3-coder", "object": "model", "state": "loaded",
            "capabilities": ["tool_use"], "quantization": "Q4_K_S",
            "max_context_length": 262144, "extra_future_field": true
        });
        let parsed = parse_row(&row).unwrap();
        assert_eq!(parsed.id, "qwen3-coder");
        assert_eq!(parsed.state, "loaded");
        assert!(parsed.tool_use);
        assert_eq!(parsed.quantization.as_deref(), Some("Q4_K_S"));
        assert_eq!(parsed.max_context_length, Some(262144));
        // No capabilities → no tool use claimed (the gemma shape).
        let bare = serde_json::json!({"id": "gemma", "state": "not-loaded"});
        let parsed = parse_row(&bare).unwrap();
        assert!(!parsed.tool_use);
        assert_eq!(parsed.state, "not-loaded");
    }

    #[tokio::test]
    async fn scripted_server_round_trip_and_failure_degrade() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({"data": [
                {"id": "m1", "state": "loading", "capabilities": ["tool_use"]},
            ]})
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let endpoint = format!("http://127.0.0.1:{}/v1", addr.port());
        let state = model_state(&endpoint, "m1").await;
        assert_eq!(state.as_deref(), Some("loading"));
        // Unknown model → None; dead endpoint → None (degrade, no error).
        assert_eq!(model_state("http://127.0.0.1:1/v1", "m1").await, None);
    }
}
