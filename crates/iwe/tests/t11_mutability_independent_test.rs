//! T11 (independent verification build of `EXT-PER-PROPERTY-MUTABILITY`):
//! end-to-end CLI tests for the `mutable:` schema flag, exercised through
//! the real `iwe` binary the same way `schema_strict_test.rs` exercises
//! `--strict` — a store fixture written to a `TempDir`, a real subprocess
//! invocation, and assertions on stdout/stderr/exit-code/on-disk content.
//!
//! This build's own `mutable:` syntax (see `diwe::permissions` for the
//! rationale): `SchemaBinding.mutable` is a `HashMap<String, bool>` keyed
//! by the same property-selector string `PropertyRef::from_selector`
//! already uses everywhere else — `"$content"` for the document body, a
//! (possibly dotted) frontmatter field path otherwise. A property absent
//! from the map is mutable by default; only an explicit `false` marks it
//! immutable.
//!
//! Fixtures below are deliberately layer-free: no layer/assembly/origin/
//! package vocabulary from the law exemplars. `vault/sealed-record` and a
//! `status` field stand in for LAW-09's "mint-origin document"/"other
//! property" shape.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const SEALED: &str = "---\nstatus: draft\n---\n\n# Sealed\n\nOriginal body.\n";

/// A store with one document (`vault/sealed-record`) bound to a schema
/// whose `mutable:` table marks the body (`$content`) immutable and leaves
/// `status` unmentioned (so it stays mutable by the default rule). An
/// empty-constraint `.iwe/schemas/vault.yaml` is present so `--strict`'s
/// separate schema-validation gate passes cleanly and does not mask the
/// write-permission rejection under test.
fn setup_body_immutable() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe/schemas")).unwrap();
    create_dir_all(temp.path().join("vault")).unwrap();
    let mut mutable = HashMap::new();
    mutable.insert("$content".to_string(), false);
    write_config(temp.path(), binding("vault", "vault/**", mutable));
    write(
        temp.path().join(".iwe/schemas/vault.yaml"),
        "sections: []\n",
    )
    .unwrap();
    write(temp.path().join("vault/sealed-record.md"), SEALED).unwrap();
    temp
}

/// A store with the same one document, bound to a schema with no
/// `mutable:` table at all (T11's "default is mutable" case).
fn setup_default_mutable() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe/schemas")).unwrap();
    create_dir_all(temp.path().join("vault")).unwrap();
    write_config(temp.path(), binding("vault", "vault/**", HashMap::new()));
    write(
        temp.path().join(".iwe/schemas/vault.yaml"),
        "sections: []\n",
    )
    .unwrap();
    write(temp.path().join("vault/sealed-record.md"), SEALED).unwrap();
    temp
}

fn binding(
    name: &str,
    pattern: &str,
    mutable: HashMap<String, bool>,
) -> HashMap<String, SchemaBinding> {
    let mut schemas = HashMap::new();
    schemas.insert(
        name.to_string(),
        SchemaBinding {
            r#match: Patterns::One(pattern.to_string()),
            mutable,
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

/// AC: "Rejection fires identically across at least 2 of {CLI ordinary,
/// CLI `--strict`, MCP}" — ordinary and `--strict` invocation of the exact
/// same body-overwrite both reach `check_write_permission_for_content`
/// unconditionally (per `iwe::main::update_body`'s own comment: the check
/// runs after any `--strict` gating, not inside an `if args.strict`
/// branch), so both must reject identically: same exit code, same stderr
/// message (document/rule/property named), same untouched file on disk.
#[test]
fn body_write_rejected_identically_ordinary_and_strict() {
    let ordinary = setup_body_immutable();
    let ordinary_output = run_update(
        ordinary.path(),
        &[
            "-k",
            "vault/sealed-record",
            "--content",
            "# Sealed\n\nNew body.\n",
        ],
    );

    let strict = setup_body_immutable();
    let strict_output = run_update(
        strict.path(),
        &[
            "-k",
            "vault/sealed-record",
            "--content",
            "# Sealed\n\nNew body.\n",
            "--strict",
        ],
    );

    assert_eq!(ordinary_output.status.code(), Some(1));
    assert_eq!(strict_output.status.code(), Some(1));

    let ordinary_stderr =
        String::from_utf8(ordinary_output.stderr).expect("valid utf-8 stderr");
    let strict_stderr = String::from_utf8(strict_output.stderr).expect("valid utf-8 stderr");
    assert_eq!(
        ordinary_stderr, strict_stderr,
        "ordinary and --strict must reject with the identical message"
    );

    // Names the document, the rule, and the specific property.
    assert!(
        ordinary_stderr.contains("vault/sealed-record"),
        "{ordinary_stderr}"
    );
    assert!(ordinary_stderr.contains("vault"), "{ordinary_stderr}");
    assert!(ordinary_stderr.contains("$content"), "{ordinary_stderr}");
    assert!(
        ordinary_stderr.contains("not mutable"),
        "{ordinary_stderr}"
    );

    assert_eq!(
        read_to_string(ordinary.path().join("vault/sealed-record.md")).unwrap(),
        SEALED,
        "the rejected write must not reach disk (ordinary)"
    );
    assert_eq!(
        read_to_string(strict.path().join("vault/sealed-record.md")).unwrap(),
        SEALED,
        "the rejected write must not reach disk (--strict)"
    );
}

/// AC: "Default is mutable: test a corpus of unmarked documents sees zero
/// rejections" — real end-to-end reinforcement of the unit-level corpus
/// test in `diwe::permissions`'s own test module: a schema with no
/// `mutable:` table at all rejects nothing, for both ordinary and
/// `--strict` invocation of the same body overwrite.
#[test]
fn default_mutable_schema_allows_body_write_ordinary_and_strict() {
    let ordinary = setup_default_mutable();
    let ordinary_output = run_update(
        ordinary.path(),
        &[
            "-k",
            "vault/sealed-record",
            "--content",
            "# Sealed\n\nUpdated body.\n",
        ],
    );
    assert!(
        ordinary_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ordinary_output.stderr)
    );
    // `--content` with no frontmatter of its own keeps the existing
    // frontmatter (`update_body`'s own merge behavior, unrelated to T11);
    // only the body actually changed.
    assert_eq!(
        read_to_string(ordinary.path().join("vault/sealed-record.md")).unwrap(),
        "---\nstatus: draft\n---\n\n# Sealed\n\nUpdated body.\n"
    );

    let strict = setup_default_mutable();
    let strict_output = run_update(
        strict.path(),
        &[
            "-k",
            "vault/sealed-record",
            "--content",
            "# Sealed\n\nUpdated body.\n",
            "--strict",
        ],
    );
    assert!(
        strict_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&strict_output.stderr)
    );
    assert_eq!(
        read_to_string(strict.path().join("vault/sealed-record.md")).unwrap(),
        "---\nstatus: draft\n---\n\n# Sealed\n\nUpdated body.\n"
    );
}
