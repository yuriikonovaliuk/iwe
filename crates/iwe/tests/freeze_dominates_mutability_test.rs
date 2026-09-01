// M2 reconciliation (T10 + T11 composition, R15/LAW-13): freeze is
// document-level and dominates per-property mutability. A document that
// carries `freeze: true` in its own frontmatter rejects every write to it —
// including a write to a property its bound schema explicitly marks
// `mutable: true`. See `crates/diwe/src/permissions.rs`'s
// `check_write_permission` doc comment ("Composition: freeze dominates
// mutability") for the unit-level version of this same proof; this is the
// integration-level proof through a real schema + document on disk, driven
// through the CLI the way `write_permission_test.rs` (T10) and
// `mutability_test.rs` (T11) each individually already prove their own
// construct through.
//
// Layer-free fixtures only: no `origin:`/`mint:`/package vocabulary
// anywhere.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const FROZEN_BUT_NOMINALLY_MUTABLE_DOC: &str = "\
---
freeze: true
---

# Reference

original body
";

#[test]
fn write_to_a_property_the_schema_marks_mutable_is_still_rejected_when_the_document_is_frozen() {
    // The schema explicitly marks the document body (`$content`) mutable —
    // if per-property mutability alone governed this write, it would
    // succeed. Freeze on the document's own frontmatter must dominate that
    // and reject the write anyway.
    let temp = setup("mutable:\n  $content: true\n");
    let before = read_to_string(temp.path().join("notes/one.md")).unwrap();

    let output = run_update(
        temp.path(),
        &[
            "-k",
            "notes/one",
            "--content",
            "# Reference\n\nchanged body\n",
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("notes/one") && stderr.contains("frozen"),
        "rejection must be attributed to freeze, not silently allowed because \
         the property is marked mutable: got {stderr}"
    );
    assert!(
        !stderr.contains("mutable: false"),
        "must not be misreported as a mutability rejection: got {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk even though its schema marks \
         the written property mutable"
    );
}

#[test]
fn write_to_a_property_the_schema_marks_mutable_is_still_rejected_under_strict_too() {
    let temp = setup("mutable:\n  $content: true\n");
    let before = read_to_string(temp.path().join("notes/one.md")).unwrap();

    let output = run_update(
        temp.path(),
        &[
            "-k",
            "notes/one",
            "--content",
            "# Reference\n\nchanged body\n",
            "--strict",
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("notes/one") && stderr.contains("frozen"),
        "rejection must be attributed to freeze under --strict too: got {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk under --strict too"
    );
}

fn setup(schema_source: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe/schemas")).unwrap();
    create_dir_all(temp.path().join("notes")).unwrap();
    write_config(temp.path(), binding("reference", "notes/**"));
    write(
        temp.path().join(".iwe/schemas/reference.yaml"),
        schema_source,
    )
    .unwrap();
    write(
        temp.path().join("notes/one.md"),
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC,
    )
    .unwrap();
    temp
}

fn binding(name: &str, pattern: &str) -> HashMap<String, SchemaBinding> {
    let mut schemas = HashMap::new();
    schemas.insert(
        name.to_string(),
        SchemaBinding {
            r#match: Patterns::One(pattern.to_string()),
        },
    );
    schemas
}

fn write_config(path: &Path, schemas: HashMap<String, SchemaBinding>) {
    let config = Configuration {
        library: LibraryOptions {
            path: "".to_string(),
            ..Default::default()
        },
        markdown: MarkdownOptions {
            refs_extension: "".to_string(),
            ..Default::default()
        },
        schemas,
        ..Default::default()
    };
    write(
        path.join(".iwe/config.toml"),
        toml::to_string(&config).expect("config"),
    )
    .unwrap();
}

fn run_update(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.arg("update").current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe update")
}
