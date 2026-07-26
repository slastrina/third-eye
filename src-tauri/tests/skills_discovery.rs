//! M007 S06 integration proof: `discover_skills` walks a real on-disk fixtures
//! directory (`tests/fixtures/skills/`) holding both good and malformed
//! `SKILL.md` packs and returns **only** the good ones — the fail-soft
//! acceptance criterion (R006/R007) exercised end-to-end through the public API,
//! not the in-module unit tests' temp-dir fixtures.
//!
//! Structured like `tests/keystore_live.rs`: a directory fixture rooted at
//! `CARGO_MANIFEST_DIR`, asserting observable behavior against committed,
//! git-tracked files (never a gitignored local path).

use std::path::PathBuf;

use third_eye_lib::llm::skills::{discover_skills, Skill};

/// The committed fixtures root: two good packs (`good-skill`,
/// `another-good-skill`) plus two intentionally broken ones (`malformed-skill`
/// with invalid YAML, `no-frontmatter` with no `---` block).
fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")
}

#[test]
fn discovers_only_the_good_packs_from_a_real_fixtures_directory() {
    let root = fixtures_root();
    assert!(
        root.is_dir(),
        "fixtures dir must exist at {}",
        root.display()
    );

    let skills: Vec<Skill> = discover_skills(&root);

    // Only the two well-formed packs survive; both broken packs are skipped —
    // fail-soft, never fatal. Result is sorted by name for a deterministic
    // injection order.
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["another-good-skill", "good-skill"],
        "discovery must load only the good packs, name-sorted; malformed/no-frontmatter skipped"
    );

    // The loaded body is the instruction text after the frontmatter, trimmed —
    // this is what reaches the agent's context.
    let good = skills
        .iter()
        .find(|s| s.name == "good-skill")
        .expect("good-skill loaded");
    assert!(
        good.description.starts_with("A well-formed fixture skill."),
        "description comes from the frontmatter"
    );
    assert!(
        good.body
            .contains("instruction text that loads into the agent's context"),
        "body is the Markdown after the frontmatter"
    );
    assert!(
        !good.body.starts_with("---"),
        "the frontmatter delimiter must not leak into the loaded body"
    );
}

#[test]
fn a_missing_discovery_directory_yields_no_skills_not_an_error() {
    // A path that does not exist under the fixtures root — discovery must return
    // an empty vec, never panic or error (missing-dir is fail-soft).
    let missing = fixtures_root().join("this-subdir-does-not-exist");
    assert!(!missing.exists());
    assert!(
        discover_skills(&missing).is_empty(),
        "a missing discovery directory yields an empty result, not an error"
    );
}
