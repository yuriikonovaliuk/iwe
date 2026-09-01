// T10 (EXT-FREEZE): a document carrying `freeze: true` in its own
// frontmatter has every write to it rejected — body or any single
// frontmatter field — by the shared write-permission check
// (`diwe::permissions::check_write_permission`), regardless of which CLI
// command reaches it or whether `--strict` is passed. Layer-free fixtures
// only: no `origin:`/`mint:`/package vocabulary.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions};
use indoc::indoc;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn setup(docs: Vec<(&str, &str)>) -> TempDir {
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
    for (key, content) in docs {
        write(temp_path.join(format!("{}.md", key)), content).expect("write doc");
    }
    temp_dir
}

fn run_iwe(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe")
}

const FROZEN_DOC: &str = indoc! {"
    ---
    freeze: true
    status: draft
    ---

    # Frozen Document

    Original body.
"};

const UNFROZEN_DOC: &str = indoc! {"
    ---
    status: draft
    ---

    # Unfrozen Document

    Original body.
"};

#[test]
fn body_write_to_a_frozen_document_is_rejected() {
    let temp = setup(vec![("doc", FROZEN_DOC)]);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "-c", "# Frozen Document\n\nNew body.\n"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doc") && stderr.contains("frozen"),
        "stderr should name the document and the rule, got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

#[test]
fn frontmatter_only_write_to_a_frozen_document_is_also_rejected() {
    // Proves freeze rejects even a write that touches only a frontmatter
    // field, not the body — whole-document rejection, not per-property.
    let temp = setup(vec![("doc", FROZEN_DOC)]);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "--set", "status=reviewed"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doc") && stderr.contains("frozen"),
        "stderr should name the document and the rule, got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

#[test]
fn body_write_to_a_frozen_document_is_rejected_under_strict() {
    let temp = setup(vec![("doc", FROZEN_DOC)]);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "-c",
            "# Frozen Document\n\nNew body.\n",
            "--strict",
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doc") && stderr.contains("frozen"),
        "stderr should name the document and the rule, got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

#[test]
fn frontmatter_write_to_a_frozen_document_is_rejected_under_strict() {
    let temp = setup(vec![("doc", FROZEN_DOC)]);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "status=reviewed",
            "--strict",
            "--expect",
            "1",
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doc") && stderr.contains("frozen"),
        "stderr should name the document and the rule, got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

#[test]
fn unfrozen_document_body_write_succeeds_unchanged() {
    // AB9: no freeze marker means no new rejection — behavior identical to
    // before EXT-FREEZE existed.
    let temp = setup(vec![("doc", UNFROZEN_DOC)]);

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "-c", "# Unfrozen Document\n\nNew body.\n"],
    );

    assert!(output.status.success());
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(after.contains("New body."));
}

#[test]
fn unfrozen_document_frontmatter_write_succeeds_unchanged() {
    let temp = setup(vec![("doc", UNFROZEN_DOC)]);

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "--set", "status=reviewed"],
    );

    assert!(output.status.success());
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(after.contains("status: reviewed"));
}

#[test]
fn create_of_a_frozen_document_succeeds() {
    // M2 fix-wave (`m2/design-freeze-semantics`): the write-permission
    // predicate was corrected to gate on the document's *prior* on-disk
    // state, not on the outgoing content being written. A brand-new
    // document has no prior state at all, so there is nothing frozen to
    // violate — creating a document that carries `freeze: true` from the
    // moment it is created must succeed, the same way setting `freeze:
    // true` on a previously-unfrozen document (plus other changes) is
    // unrestricted ("Freezing is not restricted").
    //
    // This test used to assert the opposite (rejection) — that was the
    // pre-fix, outgoing-content-shaped predicate's behavior, which is what
    // this fix-wave corrected; updated here to match the corrected rule.
    let temp = setup(vec![]);

    let output = run_iwe(
        temp.path(),
        &[
            "create",
            "brand-new",
            "--content",
            "---\nfreeze: true\n---\n\n# Brand New\n\nBody.\n",
        ],
    );

    assert!(output.status.success());
    assert!(temp.path().join("brand-new.md").exists());
}

// M2 fix-wave (`m2/design-freeze-semantics`): the freeze-bypass closure.
// These reproduce the ruling's own bypass example and its three named
// exceptions end to end, through the real CLI.

#[test]
fn a_single_write_that_lifts_freeze_and_changes_another_field_is_rejected() {
    // The bypass itself: `--set freeze=false --set status=changed` in one
    // call used to be evaluated only against its own (now-unfrozen)
    // resulting content, so both changes landed. It must now be rejected as
    // a whole, since its effect is not *solely* lifting freeze.
    let temp = setup(vec![("frozen-doc", FROZEN_DOC)]);
    let before = read_to_string(temp.path().join("frozen-doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "frozen-doc",
            "--set",
            "freeze=false",
            "--set",
            "status=changed",
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("frozen-doc") && stderr.contains("frozen"),
        "stderr should name the document and the rule, got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("frozen-doc.md")).unwrap(),
        before,
        "the bypass must not land: neither freeze nor status may change"
    );
}

#[test]
fn a_solitary_unfreeze_via_set_freeze_false_succeeds() {
    let temp = setup(vec![("frozen-doc", FROZEN_DOC)]);

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "frozen-doc", "--set", "freeze=false"],
    );

    assert!(output.status.success());
    let after = read_to_string(temp.path().join("frozen-doc.md")).unwrap();
    assert!(after.contains("freeze: false"), "{after}");
    assert!(after.contains("status: draft"), "{after}");
    assert!(after.contains("Original body."), "{after}");
}

#[test]
fn a_solitary_unfreeze_via_unset_freeze_succeeds() {
    let temp = setup(vec![("frozen-doc", FROZEN_DOC)]);

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "frozen-doc", "--unset", "freeze"],
    );

    assert!(output.status.success());
    let after = read_to_string(temp.path().join("frozen-doc.md")).unwrap();
    assert!(
        !after.contains("freeze:"),
        "the marker must be removed outright, not merely falsified: {after}"
    );
    assert!(after.contains("status: draft"), "{after}");
    assert!(after.contains("Original body."), "{after}");
}

#[test]
fn freezing_a_previously_unfrozen_document_plus_other_changes_succeeds() {
    // Detail (b) from the ruling: freezing itself is unrestricted — a write
    // that sets `freeze` on a document that was *not* frozen when the
    // predicate ran may carry other changes in the very same write.
    let temp = setup(vec![("doc", UNFROZEN_DOC)]);

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=true",
            "--set",
            "status=reviewed",
        ],
    );

    assert!(output.status.success());
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(after.contains("freeze: true"), "{after}");
    assert!(after.contains("status: reviewed"), "{after}");
}
