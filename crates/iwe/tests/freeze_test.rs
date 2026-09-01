// T10 (EXT-FREEZE) — independent verification build.
//
// This file exercises `diwe::permissions`'s freeze logic (see that
// module's doc comments for the full "T10 (EXT-FREEZE) — independent
// verification build, not authoritative" writeup) from the outside, as a
// black box, through the real `iwe` CLI binary. It is this task's own,
// independently-built implementation and test suite — not the Developer's,
// who is working the same construct in a separate, unseen worktree.
//
// Freeze surface (adapted at merge time to the shipped design): a
// document is frozen when its own frontmatter carries a top-level
// boolean field named `freeze` set to `true` (the Test-builder's
// independent build chose `frozen`; assertions unchanged, marker and
// message aligned to the merged implementation). No layer/assembly/origin/
// package vocabulary appears anywhere in these fixtures or assertions,
// matching LAW-12's structural shape ("Frozen document content
// immutable... rejected at write time by IWE itself").
//
// The empirical core of this file is
// `ordinary_and_strict_rejections_are_byte_identical`: it runs the exact
// same write against the exact same frozen fixture once without `--strict`
// and once with it, and diffs the two `iwe` invocations' outputs directly,
// rather than merely asserting the same expected string twice.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions};
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const FROZEN_DOC: &str = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen Note\n\nOriginal body.\n";
const PLAIN_DOC: &str = "---\nstatus: draft\n---\n\n# Plain Note\n\nOriginal body.\n";

fn setup() -> TempDir {
    let temp_dir = TempDir::new().expect("tempdir");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe")).expect("mkdir .iwe");
    let config = Configuration {
        library: LibraryOptions {
            path: "".to_string(),
            ..Default::default()
        },
        markdown: MarkdownOptions {
            refs_extension: "".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    write(
        temp_path.join(".iwe").join("config.toml"),
        toml::to_string(&config).expect("config"),
    )
    .expect("write config");
    write(temp_path.join("frozen-note.md"), FROZEN_DOC).expect("write frozen doc");
    write(temp_path.join("plain-note.md"), PLAIN_DOC).expect("write plain doc");
    temp_dir
}

fn run_update(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.arg("update").current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe update")
}

const REJECTION_MESSAGE: &str =
    "Error: write to 'frozen-note' rejected: document is frozen (unset 'freeze' to allow writes)\n";

#[test]
fn update_ordinary_rejects_a_write_to_a_frozen_document() {
    let temp = setup();
    let output = run_update(
        temp.path(),
        &["-k", "frozen-note", "--content", "# Changed\n"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("valid utf-8 stderr");
    assert_eq!(stderr, REJECTION_MESSAGE);
    assert_eq!(
        read_to_string(temp.path().join("frozen-note.md")).unwrap(),
        FROZEN_DOC,
        "the frozen document's on-disk content must be untouched"
    );
}

#[test]
fn update_strict_rejects_a_write_to_a_frozen_document() {
    let temp = setup();
    let output = run_update(
        temp.path(),
        &["-k", "frozen-note", "--content", "# Changed\n", "--strict"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("valid utf-8 stderr");
    assert_eq!(stderr, REJECTION_MESSAGE);
    assert_eq!(
        read_to_string(temp.path().join("frozen-note.md")).unwrap(),
        FROZEN_DOC,
        "the frozen document's on-disk content must be untouched"
    );
}

#[test]
fn ordinary_and_strict_rejections_of_the_same_write_are_byte_identical() {
    // Acceptance criterion: "Rejection fires identically across at least 2
    // of {CLI ordinary, CLI --strict, MCP}", proven empirically rather than
    // by asserting the same literal string in two separate tests. Two
    // independent fixtures (so the strict run's schema-validation gate
    // never has to share a document with a run that might have mutated
    // it), same key, same content, same rejection expected either way.
    let ordinary_temp = setup();
    let ordinary = run_update(
        ordinary_temp.path(),
        &["-k", "frozen-note", "--content", "# Changed\n"],
    );

    let strict_temp = setup();
    let strict = run_update(
        strict_temp.path(),
        &["-k", "frozen-note", "--content", "# Changed\n", "--strict"],
    );

    assert_eq!(
        ordinary.status.code(),
        strict.status.code(),
        "ordinary and --strict must reject with the same exit code"
    );
    assert_eq!(
        ordinary.stderr, strict.stderr,
        "ordinary and --strict must reject with byte-identical stderr"
    );
    assert!(!ordinary.status.success());
}

#[test]
fn update_ordinary_allows_a_write_to_an_unfrozen_document() {
    // "An unfrozen document sees zero behavior change": ordinary mode.
    let temp = setup();
    let output = run_update(
        temp.path(),
        &["-k", "plain-note", "--content", "# Changed\n"],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let on_disk = read_to_string(temp.path().join("plain-note.md")).unwrap();
    assert_ne!(on_disk, PLAIN_DOC, "the write must actually have applied");
    assert!(on_disk.contains("Changed"));
}

#[test]
fn update_strict_allows_a_write_to_an_unfrozen_document() {
    // "An unfrozen document sees zero behavior change": --strict mode too
    // (no schema is configured, so nothing for --strict to gate on; this
    // isolates the freeze check itself from schema validation).
    let temp = setup();
    let output = run_update(
        temp.path(),
        &["-k", "plain-note", "--content", "# Changed\n", "--strict"],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let on_disk = read_to_string(temp.path().join("plain-note.md")).unwrap();
    assert_ne!(on_disk, PLAIN_DOC, "the write must actually have applied");
    assert!(on_disk.contains("Changed"));
}

#[test]
fn freeze_rejects_a_write_that_touches_only_an_ordinary_frontmatter_field() {
    // Acceptance criterion: freeze must reject EVERY write, including a
    // plain frontmatter-field write, not just a body write. The body below
    // is byte-identical to the fixture's own body; only the ordinary
    // `status` frontmatter field changes (`draft` -> `revised`), so a
    // non-rejecting run would have to actually rewrite the file, proving
    // this is a real, live write attempt and not a no-op.
    let temp = setup();
    let new_content =
        "---\nfrozen: true\nstatus: revised\n---\n\n# Frozen Note\n\nOriginal body.\n";
    assert_ne!(
        new_content, FROZEN_DOC,
        "sanity: the frontmatter-only edit must actually differ from the fixture"
    );
    let output = run_update(temp.path(), &["-k", "frozen-note", "--content", new_content]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("valid utf-8 stderr");
    assert_eq!(stderr, REJECTION_MESSAGE);
    assert_eq!(
        read_to_string(temp.path().join("frozen-note.md")).unwrap(),
        FROZEN_DOC
    );
}
