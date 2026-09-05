// The write gate: with `[transactions] validate = "full"` every MCP write
// tool commits through `diwe::validating_transaction::ValidatingTransaction`
// and a write that would leave the store worse than it found it is
// refused — the store-level enforcement the git pre-commit hook used to be
// for a store that is not a git repository (the compositor's materialized
// tree). Without the section, AB9's no-op default is untouched: the same
// writes land, dangling links and all.

use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};

use diwe::config::{Configuration, Patterns, SchemaBinding, TransactionOptions, ValidationScope};
use serde_json::json;
use tempfile::TempDir;

use crate::fixture::Fixture;

const HUB: &str = "---\ntype: note\n---\n# Hub\n\nSee [Leaf](leaf).\n";
const LEAF: &str = "---\ntype: note\n---\n# Leaf\n\nBack to [Hub](hub).\n";

/// `notes/**`: every link must resolve to a note. A rule with a `target`
/// filter is one the pending-documents shape check (`ensure_schema_clean`,
/// which sees only the documents being written) cannot evaluate — it is
/// the store-level gate's alone.
fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    write(
        base.join(".iwe/schemas/note.yaml"),
        "links:\n  - target: { type: note }\n",
    )
    .unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/hub.md"), HUB).unwrap();
    write(base.join("notes/leaf.md"), LEAF).unwrap();
    dir
}

fn config(scope: ValidationScope) -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        "note".to_string(),
        SchemaBinding {
            r#match: Patterns::One("notes/**".to_string()),
        },
    );
    Configuration {
        schemas,
        transactions: TransactionOptions {
            validate: scope,
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn fixture(dir: &TempDir, scope: ValidationScope) -> Fixture {
    let base = dir.path().canonicalize().unwrap();
    Fixture::with_path(base.to_str().unwrap(), config(scope)).await
}

#[tokio::test]
async fn full_scope_refuses_a_create_with_a_dangling_link_and_leaves_disk_untouched() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let result = f
        .try_call_tool(
            "iwe_create",
            json!({"key": "notes/new", "content": "---\ntype: note\n---\n# New\n\nSee [Nowhere](nowhere).\n"}),
        )
        .await;

    let message = result
        .expect_err("the dangling link must be refused")
        .to_string();
    assert!(
        message.contains("notes/new") && message.contains("nowhere"),
        "the refusal names the document and the missing target, got: {message}"
    );
    assert!(!dir.path().join("notes/new.md").exists());

    // The in-memory graph did not take the document either: a retrieve of
    // a key with no document answers with an empty placeholder, never
    // with the refused content.
    let result = f
        .call_tool("iwe_retrieve", json!({"keys": ["notes/new"]}))
        .await;
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        !text.contains("Nowhere"),
        "a refused create is not retrievable, got: {text}"
    );
}

#[tokio::test]
async fn full_scope_refuses_an_update_that_breaks_an_untouched_referrer() {
    // Retyping `leaf` is a clean write on its own; it is `hub`, never
    // written, whose link no longer satisfies the target filter. Only a
    // store-level check sees that.
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let result = f
        .try_call_tool(
            "iwe_update",
            json!({"key": "notes/leaf", "content": "---\ntype: draft\n---\n# Leaf\n\nBack to [Hub](hub).\n"}),
        )
        .await;

    let message = result
        .expect_err("breaking the referrer must be refused")
        .to_string();
    assert!(
        message.contains("notes/hub"),
        "the referrer is named, got: {message}"
    );
    assert_eq!(
        read_to_string(dir.path().join("notes/leaf.md")).unwrap(),
        LEAF
    );
    assert_eq!(
        read_to_string(dir.path().join("notes/hub.md")).unwrap(),
        HUB
    );
}

#[tokio::test]
async fn full_scope_commits_a_rename_as_one_transaction() {
    // A rename removes the old key and rewrites its referrers: judged one
    // write at a time the removal dangles `hub`; judged as one final
    // state it is clean.
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let result = f
        .call_tool(
            "iwe_rename",
            json!({"old_key": "notes/leaf", "new_key": "notes/renamed"}),
        )
        .await;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");

    assert!(!dir.path().join("notes/leaf.md").exists());
    assert!(dir.path().join("notes/renamed.md").exists());
    let hub = read_to_string(dir.path().join("notes/hub.md")).unwrap();
    assert!(
        hub.contains("(renamed)"),
        "the referrer was rewritten: {hub}"
    );
}

#[tokio::test]
async fn full_scope_accepts_a_clean_write() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let result = f
        .call_tool(
            "iwe_create",
            json!({"key": "notes/new", "content": "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n"}),
        )
        .await;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    assert_eq!(
        read_to_string(dir.path().join("notes/new.md")).unwrap(),
        "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n"
    );
}

#[tokio::test]
async fn without_the_section_the_same_dangling_write_lands() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::None).await;

    let result = f
        .call_tool(
            "iwe_create",
            json!({"key": "notes/new", "content": "---\ntype: note\n---\n# New\n\nSee [Nowhere](nowhere).\n"}),
        )
        .await;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    assert!(dir.path().join("notes/new.md").exists());
}
