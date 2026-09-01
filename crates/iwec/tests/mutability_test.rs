// T11 — EXT-PER-PROPERTY-MUTABILITY, exercised through the MCP `iwe_update`
// tool: proves the same `mutable:` rejection the CLI observes
// (`crates/iwe/tests/mutability_test.rs`) fires identically through MCP —
// the "one mechanism, not two" requirement (`m2/design-enforcement-modes`).
//
// Layer-free fixture: a plain "reference" schema, a body and no
// origin/package/assembly vocabulary anywhere.

use crate::fixture::Fixture;
use diwe::config::{Configuration, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use tempfile::TempDir;

const CLEAN: &str = "# Reference\n\noriginal body\n";

#[tokio::test]
async fn update_rejects_a_write_to_an_immutable_body_and_leaves_the_file_unchanged() {
    let dir = setup("mutable:\n  $content: false\n");
    let base = dir.path();
    let f = Fixture::with_path(base.to_str().unwrap(), config("reference", "notes/**")).await;

    let result = f
        .try_call_tool(
            "iwe_update",
            serde_json::json!({ "key": "notes/one", "content": "# Reference\n\nchanged body\n" }),
        )
        .await;

    // WP-12's `write_file` silently drops the write on rejection (no MCP
    // error surfaced today — a separate gap this task does not close, see
    // `diwe::permissions::WritePermissionError`'s doc comment); what this
    // construct guarantees, and what this test proves, is that the write
    // itself never reaches disk.
    let _ = result;
    assert_eq!(read_to_string(base.join("notes/one.md")).unwrap(), CLEAN);
}

#[tokio::test]
async fn update_without_a_mutable_keyword_allows_the_body_write() {
    let dir = setup("{}\n");
    let base = dir.path();
    let f = Fixture::with_path(base.to_str().unwrap(), config("reference", "notes/**")).await;

    let new_content = "# Reference\n\nchanged body\n";
    f.call_tool(
        "iwe_update",
        serde_json::json!({ "key": "notes/one", "content": new_content }),
    )
    .await;

    assert_eq!(read_to_string(base.join("notes/one.md")).unwrap(), new_content);
}

fn setup(schema: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    write(base.join(".iwe/schemas/reference.yaml"), schema).unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/one.md"), CLEAN).unwrap();
    dir
}

fn config(name: &str, pattern: &str) -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        name.to_string(),
        SchemaBinding {
            r#match: Patterns::One(pattern.to_string()),
        },
    );
    Configuration {
        schemas,
        ..Default::default()
    }
}
