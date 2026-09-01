// T13 — Enforcement-mode matrix (independent verification), CLI half.
//
// Independent verification per `m2/design-enforcement-modes` (C13): this
// file drives the *real* `iwe` binary (never reads or infers from
// `crates/diwe/src/permissions.rs`'s own unit tests) to observe, for each
// of the three write-permission constructs — freeze (EXT-FREEZE, T10),
// per-property mutability (EXT-PER-PROPERTY-MUTABILITY, T11), and their
// composition/dominance (R15/LAW-13) — which of C13's three enforcement
// modes actually holds under CLI-ordinary and CLI-`--strict` invocation:
//
//   mode one   — rejected by default, any invocation, no flag
//   mode two   — rejected only under `--strict`
//   mode three — never rejected at write time, only later by
//                `iwe schema validate`
//
// The MCP column of the same matrix lives in
// `crates/iwec/tests/t13_enforcement_mode_matrix_test.rs` (MCP write paths
// are compiled into the `iwec` binary/library, not `iwe`).
//
// ## The matrix (observed, this file's CLI half)
//
// | construct                    | CLI ordinary | CLI --strict |
// |-------------------------------|-------------|--------------|
// | freeze                        | mode one    | mode one     |
// | per-property mutability       | mode one    | mode one     |
// | dominance (freeze > mutable)  | mode one    | mode one     |
//
// Every cell was reached by actually invoking the `iwe` binary against a
// real fixture on disk and observing (a) the process's exit status, (b)
// its stderr, and (c) the fixture file's content afterward — never by
// reading `check_write_permission`'s source and inferring the outcome.
// See the MCP file for the third column.
//
// ## Path (c) — not tested
//
// A raw filesystem edit made outside IWE entirely (e.g. `echo >
// doc.md` from a shell, bypassing both the `iwe` CLI and the `iwec` MCP
// server) is unreachable by any IWE-side mechanism: there is no `iwe`
// process, no MCP tool call, nothing for `check_write_permission` to be
// invoked from. Per C13, that path is covered only by compositor checks
// at sync/materialization time, which is out of this task's (and this
// milestone's write-permission constructs') scope. It is recorded here,
// deliberately, as unreachable and untested — not silently omitted.
//
// ## Sub-mode-one finding: none
//
// Every cell below was produced by an *ordinary* (no `--strict`)
// invocation rejecting the write outright, and the identical-rejection
// test proves `--strict` does not vary that rejection. No construct or
// invocation path was observed sliding into mode two (rejected only under
// `--strict`) or mode three (rejected only later by `schema validate`) —
// verified empirically, not inferred.
//
// Compilability of the fixtures/binary is never cited as enforcement
// evidence anywhere in this file — every assertion below is about a
// process's exit status, stderr, and the file's content on disk after the
// process ran.
//
// Layer-free fixtures only: no `origin:`/`mint:`/package vocabulary
// anywhere in this file.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

// ---------------------------------------------------------------------
// Shared fixture plumbing
// ---------------------------------------------------------------------

fn run_iwe(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe")
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
    .expect("write config");
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

/// A schema-less fixture (no `.iwe/schemas` directory, no `schemas` table
/// in `config.toml`) carrying one document. Used for the freeze cells and
/// the identical-rejection proof, where no schema construct is under
/// test, so nothing else can add noise to `--strict`'s extra schema-
/// validation pass (`gate_pending`) — the identical-rejection proof
/// depends on that pass being a true no-op here.
fn setup_schema_less(doc_key: &str, content: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe")).expect("mkdir .iwe");
    write_config(temp.path(), HashMap::new());
    write(temp.path().join(format!("{}.md", doc_key)), content).expect("write doc");
    temp
}

/// A fixture bound to a schema declaring `mutable:` rules, for the
/// per-property-mutability and dominance cells.
fn setup_with_schema(doc_key: &str, content: &str, schema_source: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe/schemas")).expect("mkdir schemas");
    if let Some(parent) = Path::new(doc_key).parent() {
        if parent != Path::new("") {
            create_dir_all(temp.path().join(parent)).expect("mkdir doc parent");
        }
    }
    write_config(temp.path(), binding("reference", "notes/**"));
    write(
        temp.path().join(".iwe/schemas/reference.yaml"),
        schema_source,
    )
    .expect("write schema");
    write(temp.path().join(format!("{}.md", doc_key)), content).expect("write doc");
    temp
}

// ---------------------------------------------------------------------
// Cell: freeze x CLI ordinary
// ---------------------------------------------------------------------

const FROZEN_DOC: &str = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen\n\nOriginal body.\n";

#[test]
fn matrix_freeze_cli_ordinary_rejects() {
    let temp = setup_schema_less("doc", FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "-c", "# Frozen\n\nNew body.\n"],
    );

    assert!(
        !output.status.success(),
        "freeze under ordinary CLI invocation must reject the write (mode one)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doc") && stderr.contains("frozen"),
        "got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

// ---------------------------------------------------------------------
// Cell: freeze x CLI --strict
// ---------------------------------------------------------------------

#[test]
fn matrix_freeze_cli_strict_rejects() {
    let temp = setup_schema_less("doc", FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "-c",
            "# Frozen\n\nNew body.\n",
            "--strict",
        ],
    );

    assert!(
        !output.status.success(),
        "freeze under --strict must also reject the write"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doc") && stderr.contains("frozen"),
        "got: {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

// ---------------------------------------------------------------------
// Required proof: IDENTICAL rejection, ordinary vs. --strict
// ---------------------------------------------------------------------
//
// Not "both reject" (the two cells above already show that) — this proves
// the rejection is the *same* rejection: same exit code, and stderr
// byte-for-byte identical between the ordinary and `--strict` runs. That
// byte-identity is only meaningful because both runs reach the exact same
// call site: `update_body` in `crates/iwe/src/main.rs` calls
// `write_single_document_with(..., |key, content|
// diwe::permissions::check_write_permission_for_content(&config, key,
// content), ...)` unconditionally (not inside an `if args.strict`
// branch), and on `Err(e)` prints it via the single line `eprintln!("Error:
// {}", e)` — also unconditional. `args.strict` only gates an *earlier*,
// separate `gate_pending` (schema-validation) pass, which is a no-op here
// because this fixture is schema-less; it never touches or varies the
// write-permission call or its message. That is what makes this
// "identical rejection under both ordinary and strict invocation, not
// merely rejection under each" (per this task's own acceptance
// criterion), rather than two rejections that merely happen to look
// similar.
#[test]
fn matrix_freeze_identical_rejection_ordinary_vs_strict_same_code_site() {
    let ordinary_temp = setup_schema_less("doc", FROZEN_DOC);
    let strict_temp = setup_schema_less("doc", FROZEN_DOC);
    let new_content = "# Frozen\n\nNew body.\n";

    let ordinary = run_iwe(ordinary_temp.path(), &["update", "-k", "doc", "-c", new_content]);
    let strict = run_iwe(
        strict_temp.path(),
        &["update", "-k", "doc", "-c", new_content, "--strict"],
    );

    assert!(!ordinary.status.success());
    assert!(!strict.status.success());
    assert_eq!(
        ordinary.status.code(),
        strict.status.code(),
        "ordinary and --strict must exit identically for the same rejected write"
    );

    let ordinary_stderr = String::from_utf8_lossy(&ordinary.stderr);
    let strict_stderr = String::from_utf8_lossy(&strict.stderr);
    assert_eq!(
        ordinary_stderr, strict_stderr,
        "the rejection message must be byte-identical under ordinary and --strict \
         invocation — same rule, same document, same code-site — not merely \
         'both reject'"
    );
    assert_eq!(
        ordinary_stderr,
        "Error: write to 'doc' rejected: document is frozen (unset 'freeze' to allow writes)\n",
        "pinning the exact message guards against a future change that varies \
         wording between ordinary and --strict without this test noticing"
    );
}

// ---------------------------------------------------------------------
// Cell: per-property mutability x CLI ordinary / --strict
// ---------------------------------------------------------------------

const MUTABILITY_DOC: &str = "# Reference\n\noriginal body\n";

#[test]
fn matrix_mutability_cli_ordinary_rejects() {
    let temp = setup_with_schema("notes/one", MUTABILITY_DOC, "mutable:\n  $content: false\n");

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "notes/one", "--content", "# Reference\n\nchanged body\n"],
    );

    assert!(
        !output.status.success(),
        "per-property mutability under ordinary CLI invocation must reject (mode one)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("notes/one"), "{stderr}");
    assert!(stderr.contains("mutable: false"), "{stderr}");
    assert!(stderr.contains("$content"), "{stderr}");
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        MUTABILITY_DOC
    );
}

#[test]
fn matrix_mutability_cli_strict_rejects() {
    let temp = setup_with_schema("notes/one", MUTABILITY_DOC, "mutable:\n  $content: false\n");

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "notes/one",
            "--content",
            "# Reference\n\nchanged body\n",
            "--strict",
        ],
    );

    assert!(!output.status.success(), "must reject under --strict too");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("notes/one"), "{stderr}");
    assert!(stderr.contains("mutable: false"), "{stderr}");
    assert!(stderr.contains("$content"), "{stderr}");
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        MUTABILITY_DOC
    );
}

// ---------------------------------------------------------------------
// Cell: dominance (freeze > mutable) x CLI ordinary / --strict
// ---------------------------------------------------------------------
//
// The schema explicitly marks the body (`$content`) mutable — if
// per-property mutability alone governed this write, it would succeed.
// The document's own `freeze: true` must dominate anyway (R15/LAW-13),
// rejecting the write, and rejecting it *as freeze* (message names
// "frozen", not "mutable: false").

const FROZEN_BUT_NOMINALLY_MUTABLE_DOC: &str = "---\nfreeze: true\n---\n\n# Reference\n\noriginal body\n";

#[test]
fn matrix_dominance_cli_ordinary_rejects() {
    let temp = setup_with_schema(
        "notes/one",
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC,
        "mutable:\n  $content: true\n",
    );

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "notes/one", "--content", "# Reference\n\nchanged body\n"],
    );

    assert!(
        !output.status.success(),
        "dominance under ordinary CLI invocation must still reject the write"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("notes/one") && stderr.contains("frozen"),
        "must be attributed to freeze: got {stderr}"
    );
    assert!(
        !stderr.contains("mutable: false"),
        "must not be misreported as a mutability rejection: got {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC
    );
}

#[test]
fn matrix_dominance_cli_strict_rejects() {
    let temp = setup_with_schema(
        "notes/one",
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC,
        "mutable:\n  $content: true\n",
    );

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "notes/one",
            "--content",
            "# Reference\n\nchanged body\n",
            "--strict",
        ],
    );

    assert!(!output.status.success(), "must reject under --strict too");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("notes/one") && stderr.contains("frozen"),
        "must be attributed to freeze under --strict too: got {stderr}"
    );
    assert!(
        !stderr.contains("mutable: false"),
        "must not be misreported as a mutability rejection under --strict: got {stderr}"
    );
    assert_eq!(
        read_to_string(temp.path().join("notes/one.md")).unwrap(),
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC
    );
}
