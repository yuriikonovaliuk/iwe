//! Round-tripping a document through the MCP surface: `iwe_retrieve` with
//! `frontmatter: true` leads `content` with the stored frontmatter block,
//! verbatim, so it can go straight back to `iwe_update`; `iwe_update` with
//! `keep_frontmatter: true` keeps the stored frontmatter and replaces only
//! the body. Both default off, leaving today's behaviour unchanged, and a
//! keep_frontmatter update is validated and committed like a full update.

use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};

use diwe::config::{
    Configuration, JournalOptions, Patterns, SchemaBinding, TransactionOptions, ValidationScope,
};
use rmcp::model::ErrorData;
use rmcp::ServiceError;
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::fixture::Fixture;

/// Frontmatter written the way a person writes it -- a comment, flow
/// sequences, uneven spacing, quoting -- which a YAML re-serialization
/// would not reproduce, and two blank lines before the body.
const NOTE: &str = "---\nstatus:  open   # set by triage\ntags: [a, b]\nowner: 'Ada'\n---\n\n\n# Note\n\nBody text with a [link](other).\n";
const NOTE_FRONTMATTER: &str =
    "---\nstatus:  open   # set by triage\ntags: [a, b]\nowner: 'Ada'\n---\n\n\n";
const NOTE_BODY: &str = "# Note\n\nBody text with a [link](other).\n";
const OTHER: &str = "# Other\n\nPlain, no frontmatter.\n";

fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join("docs")).unwrap();
    write(dir.path().join("docs/note.md"), NOTE).unwrap();
    write(dir.path().join("docs/other.md"), OTHER).unwrap();
    dir
}

async fn fixture_at(dir: &TempDir, config: Configuration) -> Fixture {
    let base = dir.path().canonicalize().unwrap();
    Fixture::with_path(base.to_str().unwrap(), config).await
}

fn disk(dir: &TempDir, key: &str) -> String {
    read_to_string(dir.path().join(format!("{key}.md"))).unwrap()
}

async fn retrieved_content(f: &Fixture, key: &str, frontmatter: Option<bool>) -> String {
    let mut args = json!({"keys": [key], "backlinks": false});
    if let Some(flag) = frontmatter {
        args["frontmatter"] = json!(flag);
    }
    let result = f.call_tool("iwe_retrieve", args).await;
    let docs = Fixture::result_json(&result);
    docs[0]["content"].as_str().unwrap().to_string()
}

fn mcp_error(err: ServiceError) -> ErrorData {
    match err {
        ServiceError::McpError(error) => error,
        other => panic!("expected McpError, got: {other:?}"),
    }
}

// --- iwe_retrieve ----------------------------------------------------------

#[tokio::test]
async fn retrieve_defaults_to_the_body_only() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    assert_eq!(retrieved_content(&f, "docs/note", None).await, NOTE_BODY);
    assert_eq!(
        retrieved_content(&f, "docs/note", Some(false)).await,
        NOTE_BODY
    );
}

#[tokio::test]
async fn retrieve_with_frontmatter_returns_the_document_as_stored() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    assert_eq!(retrieved_content(&f, "docs/note", Some(true)).await, NOTE);
    // A document without frontmatter reads the same either way.
    assert_eq!(retrieved_content(&f, "docs/other", Some(true)).await, OTHER);
    assert_eq!(retrieved_content(&f, "docs/other", None).await, OTHER);
}

#[tokio::test]
async fn retrieve_with_frontmatter_applies_to_every_returned_document() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    let result = f
        .call_tool(
            "iwe_retrieve",
            json!({"keys": ["docs/note"], "expand": {"references": 1}, "frontmatter": true}),
        )
        .await;
    let docs = Fixture::result_json(&result);
    let by_key: HashMap<&str, &str> = docs
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["key"].as_str().unwrap(), d["content"].as_str().unwrap()))
        .collect();
    assert_eq!(by_key.get("docs/note"), Some(&NOTE));
    assert_eq!(by_key.get("docs/other"), Some(&OTHER));
}

// --- round trip -------------------------------------------------------------

#[tokio::test]
async fn retrieve_with_frontmatter_then_update_round_trips_byte_identical() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    for key in ["docs/note", "docs/other"] {
        let before = disk(&dir, key);
        let content = retrieved_content(&f, key, Some(true)).await;
        let updated = f
            .call_tool("iwe_update", json!({"key": key, "content": content}))
            .await;
        assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
        assert_eq!(
            disk(&dir, key),
            before,
            "{key} must round-trip byte-identical"
        );
    }
}

#[tokio::test]
async fn retrieve_body_then_keep_frontmatter_update_round_trips_byte_identical() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    let body = retrieved_content(&f, "docs/note", None).await;
    let updated = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/note", "content": body, "keep_frontmatter": true}),
        )
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
    assert_eq!(disk(&dir, "docs/note"), NOTE);
}

// --- iwe_update keep_frontmatter --------------------------------------------

#[tokio::test]
async fn keep_frontmatter_keeps_the_stored_frontmatter_and_replaces_the_body() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    let new_body = "# Note\n\nRewritten body.\n";
    let updated = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/note", "content": new_body, "keep_frontmatter": true}),
        )
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");

    let expected = format!("{NOTE_FRONTMATTER}{new_body}");
    assert_eq!(disk(&dir, "docs/note"), expected);
    // The in-memory graph agrees with disk.
    assert_eq!(
        retrieved_content(&f, "docs/note", Some(true)).await,
        expected
    );
    let found = f
        .call_tool("iwe_find", json!({"project": "$key,status,owner"}))
        .await;
    let found = Fixture::result_json(&found);
    let note = found
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["key"] == "docs/note")
        .unwrap();
    assert_eq!(
        note,
        &json!({"key": "docs/note", "status": "open", "owner": "Ada"}),
        "the kept frontmatter stays queryable"
    );
}

#[tokio::test]
async fn keep_frontmatter_on_a_document_without_frontmatter_writes_the_body() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    let new_body = "# Other\n\nNew.\n";
    let updated = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/other", "content": new_body, "keep_frontmatter": true}),
        )
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
    assert_eq!(disk(&dir, "docs/other"), new_body);
}

#[tokio::test]
async fn keep_frontmatter_with_frontmatter_in_the_content_is_rejected() {
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;

    for content in [
        "---\nstatus: closed\n---\n\n# Note\n\nNew.\n",
        "---\n---\n# Note\n\nNew.\n",
        NOTE,
    ] {
        let err = f
            .try_call_tool(
                "iwe_update",
                json!({"key": "docs/note", "content": content, "keep_frontmatter": true}),
            )
            .await
            .unwrap_err();
        let error = mcp_error(err);
        assert_eq!(
            error.message,
            "keep_frontmatter: content must be the body only, without a frontmatter block (omit keep_frontmatter to replace the frontmatter too)"
        );
        assert_eq!(
            disk(&dir, "docs/note"),
            NOTE,
            "a rejected update writes nothing"
        );
    }
    assert_eq!(retrieved_content(&f, "docs/note", Some(true)).await, NOTE);
}

#[tokio::test]
async fn update_defaults_to_replacing_the_whole_document() {
    let new_body = "# Note\n\nNo frontmatter any more.\n";
    for args in [
        json!({"key": "docs/note", "content": new_body}),
        json!({"key": "docs/note", "content": new_body, "keep_frontmatter": false}),
    ] {
        let dir = store();
        let f = fixture_at(&dir, Configuration::default()).await;
        let updated = f.call_tool("iwe_update", args).await;
        assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
        assert_eq!(
            disk(&dir, "docs/note"),
            new_body,
            "the frontmatter is replaced (dropped)"
        );
    }

    // And a full content with its own frontmatter replaces the stored one.
    let dir = store();
    let f = fixture_at(&dir, Configuration::default()).await;
    let full = "---\nstatus: closed\n---\n\n# Note\n\nClosed.\n";
    let updated = f
        .call_tool("iwe_update", json!({"key": "docs/note", "content": full}))
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
    assert_eq!(disk(&dir, "docs/note"), full);
}

// --- validation: a keep_frontmatter update is validated like a full update --

const TASK_SCHEMA: &str = "frontmatter:\n  type: object\n  required: [status]\n  properties:\n    status: { enum: [open, closed] }\nsections:\n  - header: { const: Note }\n    sections:\n      - header: { const: Tasks }\n";
const TASK: &str = "---\nstatus: open\n---\n\n# Note\n\n## Tasks\n\n- one\n";

fn schema_store() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join(".iwe/schemas")).unwrap();
    write(dir.path().join(".iwe/schemas/task.yaml"), TASK_SCHEMA).unwrap();
    create_dir_all(dir.path().join("docs")).unwrap();
    write(dir.path().join("docs/task.md"), TASK).unwrap();
    dir
}

fn schema_config() -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        "task".to_string(),
        SchemaBinding {
            r#match: Patterns::One("docs/**".to_string()),
        },
    );
    Configuration {
        schemas,
        ..Default::default()
    }
}

#[tokio::test]
async fn keep_frontmatter_update_is_validated_with_the_kept_frontmatter() {
    let dir = schema_store();
    let f = fixture_at(&dir, schema_config()).await;

    // The body alone lacks the required `status`; with the kept frontmatter
    // it is valid, so the full document is what gets validated.
    let body = "# Note\n\n## Tasks\n\n- one\n- two\n";
    let err = f
        .try_call_tool("iwe_update", json!({"key": "docs/task", "content": body}))
        .await
        .unwrap_err();
    assert!(mcp_error(err)
        .message
        .starts_with("schema validation failed"));
    assert_eq!(disk(&dir, "docs/task"), TASK);

    let updated = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/task", "content": body, "keep_frontmatter": true}),
        )
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
    assert_eq!(
        disk(&dir, "docs/task"),
        format!("---\nstatus: open\n---\n\n{body}")
    );
}

#[tokio::test]
async fn keep_frontmatter_update_is_rejected_exactly_like_the_equivalent_full_update() {
    let dir = schema_store();
    let f = fixture_at(&dir, schema_config()).await;

    let bad_body = "# Note\n\nNo tasks section.\n";
    let kept = mcp_error(
        f.try_call_tool(
            "iwe_update",
            json!({"key": "docs/task", "content": bad_body, "keep_frontmatter": true}),
        )
        .await
        .unwrap_err(),
    );
    let full = mcp_error(
        f.try_call_tool(
            "iwe_update",
            json!({"key": "docs/task", "content": format!("---\nstatus: open\n---\n\n{bad_body}")}),
        )
        .await
        .unwrap_err(),
    );
    assert!(
        kept.message.starts_with("schema validation failed"),
        "{}",
        kept.message
    );
    assert_eq!(kept.message, full.message);
    assert_eq!(kept.data, full.data);
    assert_eq!(disk(&dir, "docs/task"), TASK);
}

#[tokio::test]
async fn keep_frontmatter_cannot_write_a_frozen_document() {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join("docs")).unwrap();
    let frozen = "---\nfreeze: true\n---\n\n# Signed\n\nFinal.\n";
    write(dir.path().join("docs/signed.md"), frozen).unwrap();
    let f = fixture_at(&dir, Configuration::default()).await;

    let err = f
        .try_call_tool(
            "iwe_update",
            json!({"key": "docs/signed", "content": "# Signed\n\nChanged.\n", "keep_frontmatter": true}),
        )
        .await
        .unwrap_err();
    assert!(mcp_error(err).message.contains("document is frozen"));
    assert_eq!(disk(&dir, "docs/signed"), frozen);
}

// --- transactions -------------------------------------------------------------

fn tx_config() -> Configuration {
    Configuration {
        transactions: TransactionOptions {
            validate: ValidationScope::Full,
            ..Default::default()
        },
        journal: JournalOptions {
            path: Some(".iwe/journal.ndjson".to_string()),
        },
        ..Default::default()
    }
}

fn journal_records(dir: &TempDir) -> Vec<Value> {
    read_to_string(dir.path().join(".iwe/journal.ndjson"))
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn keep_frontmatter_inside_a_transaction_keeps_the_staged_frontmatter() {
    let dir = store();
    create_dir_all(dir.path().join(".iwe")).unwrap();
    let f = fixture_at(&dir, tx_config()).await;

    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "h1"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");

    // Stage a frontmatter change, then a body-only update on top of it.
    let staged = "---\nstatus: closed\n---\n\n# Note\n\nClosed.\n";
    let r = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/note", "content": staged, "handle": "h1"}),
        )
        .await;
    assert!(!r.is_error.unwrap_or(false), "{r:?}");
    let body = "# Note\n\nClosed, with a reason.\n";
    let r = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/note", "content": body, "keep_frontmatter": true, "handle": "h1"}),
        )
        .await;
    assert!(!r.is_error.unwrap_or(false), "{r:?}");
    // Frontmatter inside the content is refused inside a transaction too.
    let err = f
        .try_call_tool(
            "iwe_update",
            json!({"key": "docs/note", "content": staged, "keep_frontmatter": true, "handle": "h1"}),
        )
        .await
        .unwrap_err();
    assert!(mcp_error(err).message.starts_with("keep_frontmatter:"));

    assert_eq!(disk(&dir, "docs/note"), NOTE, "nothing lands before commit");
    assert!(journal_records(&dir).is_empty());

    let committed = f.call_tool("iwe_tx_commit", json!({"handle": "h1"})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert_eq!(Fixture::result_json(&committed)["status"], "committed");

    assert_eq!(
        disk(&dir, "docs/note"),
        format!("---\nstatus: closed\n---\n\n{body}")
    );
    let records = journal_records(&dir);
    assert_eq!(records.len(), 1, "{records:?}");
    let effects = records[0]["effects"].as_array().unwrap();
    assert_eq!(effects.len(), 1, "{effects:?}");
    assert_eq!(effects[0]["key"], "docs/note");
}

#[tokio::test]
async fn round_trip_inside_the_default_transaction_reads_and_writes_the_staged_document() {
    let dir = store();
    create_dir_all(dir.path().join(".iwe")).unwrap();
    let f = fixture_at(&dir, tx_config()).await;

    let begun = f.call_tool("iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let staged = "---\nstatus: closed # staged\n---\n\n# Note\n\nStaged.\n";
    let r = f
        .call_tool("iwe_update", json!({"key": "docs/note", "content": staged}))
        .await;
    assert!(!r.is_error.unwrap_or(false), "{r:?}");

    // The read sees the staged frontmatter, verbatim; writing it back is a no-op.
    let content = retrieved_content(&f, "docs/note", Some(true)).await;
    assert_eq!(content, staged);
    let r = f
        .call_tool(
            "iwe_update",
            json!({"key": "docs/note", "content": content}),
        )
        .await;
    assert!(!r.is_error.unwrap_or(false), "{r:?}");

    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert_eq!(disk(&dir, "docs/note"), staged);
}

// --- tool specs -----------------------------------------------------------------

#[tokio::test]
async fn both_flags_are_listed_as_optional_booleans_with_hints() {
    let f = Fixture::with_documents(vec![]).await;
    let tools = f.list_tools().await;
    for (tool, param, hint) in [
        (
            "iwe_retrieve",
            "frontmatter",
            "content leads with the stored frontmatter, verbatim",
        ),
        (
            "iwe_update",
            "keep_frontmatter",
            "keep stored frontmatter; content = body only",
        ),
    ] {
        let schema = &tools
            .tools
            .iter()
            .find(|t| t.name == tool)
            .unwrap_or_else(|| panic!("{tool} listed"))
            .input_schema;
        assert_eq!(
            schema["properties"][param],
            json!({"type": "boolean", "description": hint}),
            "{tool}.{param}"
        );
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            !required.contains(&param),
            "{tool}.{param} must be optional"
        );
    }
}
