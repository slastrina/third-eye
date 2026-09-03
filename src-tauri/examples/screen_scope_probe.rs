//! Live timing probe for window-scoped screen_query (2026-08-30): OCR the
//! whole display, then only the named app's front window, and print the
//! element counts and milliseconds side by side.
//!
//! cargo run --example screen_scope_probe [App name]   (default: Terminal)

use std::time::Instant;
use third_eye_lib::ocr::macos::extract_elements_scoped_blocking;
use third_eye_lib::screenquery::macos::SCREEN_QUERY_MAX_DIMENSION;

fn main() {
    let app = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Terminal".to_string());
    for (label, scope) in [("screen", None), ("window", Some(app.as_str()))] {
        for round in 1..=2 {
            let start = Instant::now();
            match extract_elements_scoped_blocking(SCREEN_QUERY_MAX_DIMENSION, scope) {
                Ok((elements, used)) => eprintln!(
                    "{label:>6} #{round}: {:>4} element(s) in {:>5} ms (used {used:?})",
                    elements.len(),
                    start.elapsed().as_millis()
                ),
                Err(e) => eprintln!("{label:>6} #{round}: error {e}"),
            }
        }
    }
}
