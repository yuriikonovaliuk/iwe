// T11 — EXT-PER-PROPERTY-MUTABILITY: a per-property `mutable:` schema
// keyword, checked at every write path via
// `diwe::permissions::check_write_permission`. These tests exercise the
// construct through the CLI `update` command (WP-04's write path), in both
// ordinary and `--strict` invocation, proving rejection fires identically
// in both — the "mode one" requirement (`m2/design-enforcement-modes`):
// no flag gates this check.
//
// Fixtures are deliberately layer-free: a plain document ("reference"
// schema, a body and an ordinary `archived` field), no origin/package/
// assembly vocabulary anywhere.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const CLEAN: &str = "# Reference\n\noriginal body\n";

#[test]
fn update_ordinary_rejects_a_write_to_an_immutable_body() {
    let temp = setup("mutable:\n  $content: false\n");
    let output = run_update(
        temp.path(),
        &["-k", "notes/one", "--content", "# Reference\n\nchanged body\n"],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains("notes/one"), "{stderr}");
    assert!(stderr.contains("mutable: false"), "{stderr}");
    assert!(stderr.contains("$content"), "{stderr}");
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        CLEAN
    );
}

#[test]
fn update_strict_rejects_the_same_write_identically() {
    let temp = setup("mutable:\n  $content: false\n");
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

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains("notes/one"), "{stderr}");
    assert!(stderr.contains("mutable: false"), "{stderr}");
    assert!(stderr.contains("$content"), "{stderr}");
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        CLEAN
    );
}

/// AB9 / default-mutable, end to end: a schema with no `mutable:` keyword
/// at all imposes no rejection this construct did not already impose
/// before T11 — i.e. none. The write goes through exactly as it would have
/// on `4d39071` (the pre-T11 baseline).
#[test]
fn update_without_a_mutable_keyword_allows_the_body_write() {
    let temp = setup("{}\n");
    let new_content = "# Reference\n\nchanged body\n";
    let output = run_update(temp.path(), &["-k", "notes/one", "--content", new_content]);

    assert!(output.status.success());
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        new_content
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
    write(temp.path().join("notes/one.md"), CLEAN).unwrap();
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
