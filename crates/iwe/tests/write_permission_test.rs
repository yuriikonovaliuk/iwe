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
fn create_of_a_frozen_document_is_rejected() {
    // A document that carries `freeze: true` from the moment it is created
    // is rejected too: freeze is evaluated on the document's own content
    // being written, not only on documents that already exist on disk.
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

    assert!(!output.status.success());
    assert!(!temp.path().join("brand-new.md").exists());
}
