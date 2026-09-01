// T13 (M2 enforcement-mode observation record): closes the one matrix cell
// none of T10/T11/T12's own tests already covered — the freeze-dominates-
// mutability composition (R15/LAW-13, EXT-BOTH-JOINTLY) driven through MCP.
//
// `crates/iwe/tests/freeze_dominates_mutability_test.rs` already proves this
// composition through the CLI, ordinary and `--strict`. This is the same
// proof through the MCP `iwe_update` tool, against the same fixture shape:
// a document carrying `freeze: true` whose bound schema explicitly marks the
// body (`$content`) `mutable: true`. If per-property mutability alone
// governed this write, it would succeed (the schema says the body is
// mutable); freeze on the document's own frontmatter must dominate that and
// reject the write anyway — and it must do so via MCP exactly as it does via
// the CLI, since both funnel through the same
// `diwe::permissions::check_write_permission_for_content_in` call
// (`crates/iwec/src/lib.rs`'s `write_file_with`/`write_changes_with`).
//
// Layer-free fixture: no `origin:`/`mint:`/package vocabulary.

use crate::fixture::Fixture;
use diwe::config::{Configuration, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use tempfile::TempDir;

const FROZEN_BUT_NOMINALLY_MUTABLE_DOC: &str = "\
---
freeze: true
---

# Reference

original body
";

#[tokio::test]
async fn mcp_write_to_a_property_the_schema_marks_mutable_is_still_rejected_when_the_document_is_frozen(
) {
    let dir = setup("mutable:\n  $content: true\n");
    let base = dir.path();
    let f = Fixture::with_path(base.to_str().unwrap(), config("reference", "notes/**")).await;

    let result = f
        .try_call_tool(
            "iwe_update",
            serde_json::json!({
                "key": "notes/one",
                "content": "---\nfreeze: true\n---\n\n# Reference\n\nchanged body\n"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "write to a property the schema marks mutable must still be rejected on a frozen document"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("notes/one") && message.contains("frozen"),
        "rejection must be attributed to freeze, not silently allowed because \
         the property is marked mutable: got {message}"
    );
    assert!(
        !message.contains("mutable: false"),
        "must not be misreported as a mutability rejection: got {message}"
    );
    assert_eq!(
        read_to_string(base.join("notes/one.md")).unwrap(),
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC,
        "frozen document must be unchanged on disk even though its schema marks \
         the written property mutable"
    );
}

fn setup(schema_source: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    write(base.join(".iwe/schemas/reference.yaml"), schema_source).unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(
        base.join("notes/one.md"),
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC,
    )
    .unwrap();
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
