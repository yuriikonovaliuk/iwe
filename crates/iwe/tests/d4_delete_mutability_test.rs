// D4 (M4-extension defect): `iwe delete` on a document with an immutable
// body (or any other `mutable: false` / `freeze: true` property) used to
// PANIC (exit 101) — neither the design ruling's "deletion unsupported-
// and-ERRORS" clean rejection, nor a silent success, but a crash.
//
// Two independent bugs combined to produce that panic, and later (once D1
// landed) a different, equally wrong outcome:
//
// - `iwe/src/main.rs`'s local `apply_changes` wrapper (the durable-write
//   path `delete_command`/`rename_command`/`extract_command`/
//   `inline_command` all funnel through) used to `.expect(...)` the
//   `std::io::Result` `diwe::fs::apply_changes` returns, turning any write-
//   permission rejection it reports into a panic instead of a clean error.
// - `diwe::fs::apply_changes`'s removal loop (`crates/diwe/src/fs.rs`) used
//   to pass the document's on-disk content as *both* the outgoing
//   (`content`) and prior-content argument to the write-permission check,
//   on the reasoning "a removal's resulting content is, in effect, its own
//   unchanged prior content" — true only under the write-permission
//   predicate as it existed *before* D1's fix (which checked every write
//   unconditionally against `PropertyRef::Body`, so passing identical
//   content still triggered rejection). Once D1 replaced that with a
//   touched/untouched diff between prior and outgoing content, identical-
//   content input made every property look untouched, and deletion of a
//   document with an immutable body silently succeeded instead of being
//   rejected at all — the panic was gone, but so was the rejection.
//
// These tests exercise `iwe delete` end to end, proving the fix for both.
//
// M5 supersession (knowledge-compositor T12, R16/LAW-16): deletion is now
// governed by `deletable:` alone. The per-property `mutable:` diff runs only
// for `WriteOperation::Write`, so a `mutable: false` rule no longer rejects a
// delete — `deletable: false` does, and `freeze: true` still dominates. The
// tests below keep the original no-panic guarantee and assert the
// superseded outcome: a `mutable: false` document deletes cleanly, a
// `deletable: false` document is rejected cleanly, a frozen document is
// rejected, and an ordinary document is still deleted normally.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const CLEAN: &str = "# Reference\n\noriginal body\n";

/// Superseded fix target (M5): deleting a document whose bound schema marks
/// the body (`$content`) immutable must neither panic (exit 101) nor be
/// rejected — `mutable:` governs writes only, and the document is removed.
#[test]
fn delete_of_a_document_with_an_immutable_body_succeeds() {
    let temp = setup_schema_bound("mutable:\n  $content: false\n");
    let output = run_delete(temp.path(), &["-k", "notes/one"]);

    assert_ne!(
        output.status.code(),
        Some(101),
        "must not panic; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "a mutable: false rule must not reject a delete; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !temp.path().join("notes/one.md").exists(),
        "the document must have been removed"
    );
}

/// The same for a `mutable: false` rule on an ordinary frontmatter property
/// rather than the body — the per-property mutability diff is skipped for a
/// delete as a whole, not body-only.
#[test]
fn delete_of_a_document_with_an_immutable_frontmatter_property_succeeds() {
    let temp = setup_schema_bound("mutable:\n  archived: false\n");
    write(
        temp.path().join("notes/one.md"),
        "---\narchived: false\n---\n\n# Reference\n\noriginal body\n",
    )
    .unwrap();
    let output = run_delete(temp.path(), &["-k", "notes/one"]);

    assert_ne!(output.status.code(), Some(101), "must not panic");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("notes/one.md").exists());
}

/// The rule that does govern deletion: a bound schema declaring
/// `deletable: false` rejects the delete cleanly, naming the document and
/// the rule, and leaves it on disk untouched.
#[test]
fn delete_rejects_a_non_deletable_document() {
    let temp = setup_schema_bound("deletable: false\n");
    let output = run_delete(temp.path(), &["-k", "notes/one"]);

    assert_ne!(output.status.code(), Some(101), "must not panic");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains("notes/one"), "{stderr}");
    assert!(stderr.contains("deletable"), "{stderr}");
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        CLEAN,
        "the document must not have been removed"
    );
}

/// A frozen document (`freeze: true`) must also be rejected cleanly on
/// delete, not panic — the freeze check runs unconditionally (ahead of the
/// per-property mutability diff), so this exercises that this fix's
/// `content = ""` change to the removal check does not disturb freeze's
/// own, already-correct, rejection.
#[test]
fn delete_rejects_a_frozen_document() {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe")).unwrap();
    create_dir_all(temp.path().join("notes")).unwrap();
    write_config(temp.path(), HashMap::new());
    write(
        temp.path().join("notes/frozen.md"),
        "---\nfreeze: true\n---\n\n# Frozen\n\nbody\n",
    )
    .unwrap();

    let output = run_delete(temp.path(), &["-k", "notes/frozen"]);

    assert_ne!(output.status.code(), Some(101), "must not panic");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains("frozen") || stderr.contains("Frozen"), "{stderr}");
    assert!(
        temp.path().join("notes/frozen.md").exists(),
        "the frozen document must not have been removed"
    );
}

/// Non-regression: an ordinary document, with no `mutable:`/`freeze` rule
/// at all, is still deleted normally by this same code path (AB9's
/// default-mutable guarantee, exercised through delete rather than
/// update/create).
#[test]
fn delete_of_an_ordinary_document_still_succeeds() {
    let temp = setup_schema_bound("{}\n");
    let output = run_delete(temp.path(), &["-k", "notes/one"]);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("notes/one.md").exists());
}

fn setup_schema_bound(schema_source: &str) -> TempDir {
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

fn run_delete(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.arg("delete").current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe delete")
}
