//! S02/T03 structural proof (R011/R023): the coordinates `screen_query` hands
//! the model to aim an `input_action` click are transient — they exist in the
//! tool payload and nowhere else. This test pins the two structural walls that
//! make a coordinate-carrying persistence path *impossible*, not merely absent:
//!
//! 1. The memory store's schema has no coordinate column and declares only
//!    INTEGER/TEXT types, so there is no column an `x`/`y` could ever land in.
//! 2. The watcher's [`TextObservation`] — the sole S02 ingestion payload feeding
//!    the store — serializes to exactly `text` / `appContext` / `capturedAt`,
//!    with no `x`/`y`/`width`/`height` key, so nothing coordinate-shaped can even
//!    reach the ingestion boundary.
//!
//! A [`ScreenElement`] with real coordinates is built first to prove the tool
//! payload *does* carry x/y — the contrast is the point: coordinates live on the
//! tool side of the boundary and die there.

use third_eye_lib::memory::{MemorySource, MemoryStore, NewMemory};
use third_eye_lib::screenquery::ScreenElement;
use third_eye_lib::watcher::TextObservation;

/// The screen_query tool payload carries coordinates — the model needs x/y to
/// aim a click. This is the "before" half of the contrast: coordinates are real
/// and present on the tool side of the persistence boundary.
#[test]
fn screen_element_payload_carries_coordinates() {
    let element = ScreenElement {
        text: "Submit".into(),
        x: 100,
        y: 200,
        width: 60,
        height: 24,
        cx: 0,
        cy: 0,
        app: None,
        role: None,
    };
    let v = serde_json::to_value(&element).unwrap();
    // camelCase, and the coordinates the model reads are all present.
    assert_eq!(v["text"], "Submit");
    assert_eq!(v["x"], 100);
    assert_eq!(v["y"], 200);
    assert_eq!(v["width"], 60);
    assert_eq!(v["height"], 24);
}

/// Wall 1: the store schema has no coordinate column, and every declared column
/// type is INTEGER or TEXT. There is structurally nowhere for a coordinate to
/// be written — an `x`/`y` cannot reach the store because no column accepts it.
#[test]
fn memory_store_schema_has_no_coordinate_column() {
    let store = MemoryStore::open_in_memory().expect("open in-memory store");
    let cols = store.column_info().expect("read table_info");

    let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "id",
            "summary",
            "apps",
            "span_start_ms",
            "span_end_ms",
            "embedding",
            "created_at_ms",
            "updated_at_ms",
            "source",
            "category",
            "tags",
            "pinned",
            "expires_at_ms",
        ],
        "the pinned column set must never grow a coordinate column",
    );
    for (name, ty) in &cols {
        assert!(
            matches!(ty.to_uppercase().as_str(), "INTEGER" | "TEXT"),
            "column {name} declared a disallowed type {ty:?} (coordinates would need a numeric \
             blob path)",
        );
    }
    // No column name even hints at a coordinate.
    for coord in ["x", "y", "width", "height", "bbox", "rect", "coord"] {
        assert!(
            !names.iter().any(|n| n.eq_ignore_ascii_case(coord)),
            "store grew a coordinate-shaped column: {coord}",
        );
    }
}

/// Even a memory whose summary was *derived from* screen_query-shaped on-screen
/// text stores only text/metadata — inserting it and reading the row back over
/// IPC yields no coordinate key. The store round-trip cannot smuggle x/y.
#[test]
fn inserting_screen_derived_text_persists_no_coordinates() {
    let store = MemoryStore::open_in_memory().expect("open in-memory store");
    // A summary a distiller might write from a screen_query result — the words
    // came from on-screen elements, but the coordinates are gone by ingestion.
    let record = store
        .insert(NewMemory {
            summary: "Clicked the Submit button and confirmed the export dialog".into(),
            apps: vec!["Safari".into()],
            span_start_ms: 1_000,
            span_end_ms: 2_000,
            embedding: None,
            source: MemorySource::Watcher,
            category: "other".into(),
            tags: Vec::new(),
            pinned: false,
            expires_at_ms: None,
        })
        .expect("insert screen-derived memory");

    let v = serde_json::to_value(&record).unwrap();
    let obj = v.as_object().expect("record serializes to an object");
    for coord in ["x", "y", "width", "height", "bbox", "rect"] {
        assert!(
            !obj.contains_key(coord),
            "stored record leaked coordinate key {coord}: {v}"
        );
    }
}

/// Wall 2: [`TextObservation`], the sole S02 ingestion payload feeding the
/// store, serializes to exactly `text` / `appContext` / `capturedAt`. A
/// screen_query-shaped observation (built from on-screen text) carries no
/// coordinate key across the ingestion boundary.
#[test]
fn text_observation_ingestion_payload_has_no_coordinate_keys() {
    let observation = TextObservation {
        text: "Submit\nExport dialog".into(),
        app_context: Some("Safari".into()),
        captured_at: 1_234_567_890,
    };
    let v = serde_json::to_value(&observation).unwrap();
    let obj = v.as_object().expect("observation serializes to an object");

    // Exact field set — the pinned ingestion shape (matches watcher's own pin).
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["appContext", "capturedAt", "text"]);

    // And explicitly: no coordinate ever crosses into ingestion.
    for coord in ["x", "y", "width", "height", "bbox", "rect", "coord"] {
        assert!(
            !obj.contains_key(coord),
            "ingestion payload leaked coordinate key {coord}: {v}"
        );
    }
}
