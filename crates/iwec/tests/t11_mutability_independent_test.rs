//! T11 (independent verification build of `EXT-PER-PROPERTY-MUTABILITY`):
//! MCP-side companion to `crates/iwe/tests/mutability_test.rs`'s CLI
//! ordinary/`--strict` pair — same schema. (Adapted at merge time: the
//! shipped design carries `mutable:` in the schema file, not on the
//! config binding; fixtures updated, assertions unchanged. `"$content"` for the
//! body), reached through `iwe_update` instead of a CLI subprocess, to
//! demonstrate the rejection fires from the MCP surface too (this test
//! plus the CLI pair together satisfy "rejection fires identically across
//! at least 2 of {CLI ordinary, CLI `--strict`, MCP}" with all three, not
//! just two).
//!
//! Layer-free fixture, matching the CLI test: `vault/sealed-record` /
//! `status`, standing in for LAW-09's "mint-origin document"/"other
//! property" shape — no layer/assembly/origin/package vocabulary.

use crate::fixture::Fixture;
use diwe::config::{Configuration, Patterns, SchemaBinding};
use serde_json::json;
use std::collections::HashMap;
use std::fs;

const SEALED: &str = "---\nstatus: draft\n---\n\n# Sealed\n\nOriginal body.\n";

/// MCP's `ensure_schema_clean` runs unconditionally (unlike the CLI, which
/// only runs its equivalent under `--strict`), so any document bound to a
/// named schema needs a real, loadable (even if empty-constraint)
/// `.iwe/schemas/<name>.yaml` file, or every write is rejected by schema
/// validation before the write-permission check under test ever runs.
fn write_permissive_schema_file(base: &std::path::Path, name: &str) {
    fs::create_dir_all(base.join(".iwe/schemas")).unwrap();
    fs::write(
        base.join(format!(".iwe/schemas/{name}.yaml")),
        "sections: []\n",
    )
    .unwrap();
}

fn body_immutable_config() -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        "vault".to_string(),
        SchemaBinding {
            r#match: Patterns::One("vault/**".to_string()),
        },
    );
    Configuration {
        schemas,
        ..Configuration::default()
    }
}

/// Writes the vault schema with the body marked immutable — the shipped
/// design's home for the `mutable:` mapping.
fn write_body_immutable_schema_file(base: &std::path::Path, name: &str) {
    fs::create_dir_all(base.join(".iwe/schemas")).unwrap();
    fs::write(
        base.join(format!(".iwe/schemas/{name}.yaml")),
        "sections: []\nmutable:\n  $content: false\n",
    )
    .unwrap();
}

#[tokio::test]
async fn update_rejects_body_write_when_schema_marks_content_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    fs::create_dir_all(base.join("vault")).unwrap();
    fs::write(base.join("vault/sealed-record.md"), SEALED).unwrap();
    write_body_immutable_schema_file(&base, "vault");

    let f = Fixture::with_path(base.to_str().unwrap(), body_immutable_config()).await;

    let result = f
        .try_call_tool(
            "iwe_update",
            json!({"key": "vault/sealed-record", "content": "# Sealed\n\nNew body.\n"}),
        )
        .await;

    let message = result.unwrap_err().to_string();
    // Names the document, the rule, and the specific property — same
    // `WritePermissionError::Display` the CLI's stderr uses.
    assert!(message.contains("vault/sealed-record"), "{message}");
    assert!(message.contains("vault"), "{message}");
    assert!(message.contains("$content"), "{message}");
    assert!(message.contains("mutable: false"), "{message}");

    // The rejected write must not reach disk.
    let on_disk = fs::read_to_string(base.join("vault/sealed-record.md")).unwrap();
    assert_eq!(on_disk, SEALED);
}

#[tokio::test]
async fn update_allows_body_write_when_schema_has_no_mutable_table() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    fs::create_dir_all(base.join("vault")).unwrap();
    fs::write(base.join("vault/sealed-record.md"), SEALED).unwrap();
    write_permissive_schema_file(&base, "vault");

    let mut schemas = HashMap::new();
    schemas.insert(
        "vault".to_string(),
        SchemaBinding {
            r#match: Patterns::One("vault/**".to_string()),

        },
    );
    let config = Configuration {
        schemas,
        ..Configuration::default()
    };
    let f = Fixture::with_path(base.to_str().unwrap(), config).await;

    f.call_tool(
        "iwe_update",
        json!({"key": "vault/sealed-record", "content": "# Sealed\n\nUpdated body.\n"}),
    )
    .await;

    let on_disk = fs::read_to_string(base.join("vault/sealed-record.md")).unwrap();
    assert_eq!(on_disk, "# Sealed\n\nUpdated body.\n");
}
