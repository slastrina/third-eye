//! Lane health (2026-09-03 review item 5): are the lanes' pinned models
//! actually served, loaded, and tool-capable? A pin can go stale silently —
//! the model was unloaded, deleted, or (the qwen3.5-27b incident) turns out
//! to return nothing — and the only symptom was a failed run. This checks
//! every pin against LM Studio's native model list and reports per lane;
//! the footer pill turns red and Settings names the problem. Never repins.

use serde::Serialize;

use super::lmstudio::LmModelRow;
use super::router::LaneInfo;

/// Where a lane's pin stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneState {
    /// Served and loaded — ready.
    Loaded,
    /// Served but not loaded: the first request loads it (slow) or fails
    /// when JIT loading is off.
    NotLoaded,
    /// Not on the server at all — the pin points at nothing.
    Missing,
    /// The native model list was unavailable (non-loopback endpoint, cloud
    /// provider, LM Studio down): nothing can be said.
    Unknown,
    /// No pin — the endpoint's default model serves this lane.
    Unpinned,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneHealth {
    pub lane: String,
    pub model: Option<String>,
    pub state: LaneState,
    /// `None` when the server did not say.
    pub tool_use: Option<bool>,
    /// A one-line problem statement for the UI, or `None` when healthy.
    pub warning: Option<String>,
}

impl LaneHealth {
    /// Whether the UI should flag this lane.
    pub fn is_unhealthy(&self) -> bool {
        self.warning.is_some()
    }
}

/// Classify one lane against the served rows. Pure — the whole policy.
pub fn classify(lane: &LaneInfo, rows: Option<&[LmModelRow]>) -> LaneHealth {
    let Some(model) = lane.model_id.clone() else {
        return LaneHealth {
            lane: lane.name.clone(),
            model: None,
            state: LaneState::Unpinned,
            tool_use: None,
            warning: None,
        };
    };
    let Some(rows) = rows else {
        return LaneHealth {
            lane: lane.name.clone(),
            model: Some(model),
            state: LaneState::Unknown,
            tool_use: None,
            warning: None,
        };
    };
    let row = rows.iter().find(|r| r.id.eq_ignore_ascii_case(&model));
    let Some(row) = row else {
        return LaneHealth {
            lane: lane.name.clone(),
            warning: Some(format!(
                "{model} is not served by LM Studio — re-pin the {} lane in Settings",
                lane.name
            )),
            model: Some(model),
            state: LaneState::Missing,
            tool_use: None,
        };
    };
    let state = if row.state.eq_ignore_ascii_case("loaded") {
        LaneState::Loaded
    } else {
        LaneState::NotLoaded
    };
    let mut problems = Vec::new();
    if state == LaneState::NotLoaded {
        problems.push(if row.state.eq_ignore_ascii_case("loading") {
            "still loading".to_string()
        } else {
            "not loaded (the first request loads it, slowly, or fails)".to_string()
        });
    }
    if !row.tool_use {
        problems.push("no tool support — it cannot drive the computer".to_string());
    }
    LaneHealth {
        lane: lane.name.clone(),
        warning: (!problems.is_empty()).then(|| format!("{model}: {}", problems.join("; "))),
        model: Some(model),
        state,
        tool_use: Some(row.tool_use),
    }
}

/// Check every lane against the endpoint's served models (bounded: the
/// native probe times out in 1.5s and a miss is `Unknown`, never an error).
pub async fn check(endpoint: &str, lanes: &[LaneInfo]) -> Vec<LaneHealth> {
    let rows = super::lmstudio::model_rows(endpoint).await;
    let health: Vec<LaneHealth> = lanes
        .iter()
        .map(|lane| classify(lane, rows.as_deref()))
        .collect();
    for h in health.iter().filter(|h| h.is_unhealthy()) {
        log::warn!(
            "llm: {} lane unhealthy — {}",
            h.lane,
            h.warning.as_deref().unwrap_or("")
        );
    }
    health
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: &str, tool_use: bool) -> LmModelRow {
        LmModelRow {
            id: id.into(),
            state: state.into(),
            tool_use,
            quantization: None,
            max_context_length: None,
        }
    }

    fn lane(name: &str, model: Option<&str>) -> LaneInfo {
        LaneInfo {
            name: name.into(),
            model_id: model.map(String::from),
        }
    }

    #[test]
    fn loaded_tool_capable_pin_is_healthy() {
        let rows = [row("qwen3-9b", "loaded", true)];
        let h = classify(&lane("thin", Some("qwen3-9b")), Some(&rows));
        assert_eq!(h.state, LaneState::Loaded);
        assert_eq!(h.tool_use, Some(true));
        assert_eq!(h.warning, None);
        assert!(!h.is_unhealthy());
    }

    #[test]
    fn missing_pin_names_the_lane_to_repin() {
        let rows = [row("qwen3-9b", "loaded", true)];
        let h = classify(&lane("heavy", Some("qwen3.5-27b-heretic")), Some(&rows));
        assert_eq!(h.state, LaneState::Missing);
        let w = h.warning.unwrap();
        assert!(
            w.contains("qwen3.5-27b-heretic") && w.contains("heavy lane"),
            "{w}"
        );
    }

    #[test]
    fn not_loaded_and_no_tools_both_surface_in_one_line() {
        let rows = [row("gemma-4-12b", "not-loaded", false)];
        let h = classify(&lane("coder", Some("Gemma-4-12B")), Some(&rows));
        assert_eq!(
            h.state,
            LaneState::NotLoaded,
            "id match is case-insensitive"
        );
        assert_eq!(h.tool_use, Some(false));
        let w = h.warning.unwrap();
        assert!(
            w.contains("not loaded") && w.contains("no tool support"),
            "{w}"
        );
        let loading = [row("m", "loading", true)];
        assert!(classify(&lane("thin", Some("m")), Some(&loading))
            .warning
            .unwrap()
            .contains("still loading"));
    }

    #[test]
    fn unpinned_and_unknown_are_quiet() {
        assert_eq!(
            classify(&lane("thin", None), Some(&[])).state,
            LaneState::Unpinned
        );
        let h = classify(&lane("coder", Some("cloud-model")), None);
        assert_eq!(h.state, LaneState::Unknown);
        assert_eq!(h.warning, None, "no list → no claim");
    }

    #[test]
    fn json_shape_is_camel_case_kebab_state() {
        let h = classify(
            &lane("thin", Some("x")),
            Some(&[row("x", "not-loaded", true)]),
        );
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["state"], "not-loaded");
        assert_eq!(v["toolUse"], true);
        assert!(v["warning"].is_string());
    }
}
