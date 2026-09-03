//! System tools (specs/2026-09-03-system-tools.md): deterministic,
//! verifiable replacements for what the model otherwise improvises with
//! pixels and shell idioms. One discriminated tool per surface, typed
//! refusals, a readback where the OS can answer, lane-scoped.

pub mod browser;
pub mod find_files;
pub mod mac;
pub mod open;
pub mod processes;
pub mod text_selection;
pub mod ui_action;
pub mod wait_for_text;
