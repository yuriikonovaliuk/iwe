// iwe_check: a scoped, fast alternative to a whole-store `iwe schema
// validate` pass for checking exactly the documents just written — see
// the tool's own description in lib.rs for the motivating cost trace.

use std::collections::HashMap;
use std::fs::{create_dir_all, write};

use diwe::config::{Configuration, Patterns, SchemaBinding};
use serde_json::json;
use tempfile::TempDir;

use crate::fixture::Fixture;

fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    // A document under notes/** may not exceed 5 body tokens.
    write(base.join(".iwe/schemas/note.yaml"), "maxTokens: 5\n").unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/short.md"), "# Short\n\nfine.\n").unwrap();
    write(
        base.join("notes/long.md"),
        "# Long\n\nthis body has clearly more than five tokens in it.\n",
    )
    .unwrap();
    dir
}

fn config() -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        "note".to_string(),
        SchemaBinding {
            r#match: Patterns::One("notes/**".to_string()),
        },
    );
    Configuration {
        schemas,
        ..Default::default()
    }
}

async fn fixture(dir: &TempDir) -> Fixture {
    let base = dir.path().canonicalize().unwrap();
    Fixture::with_path(base.to_str().unwrap(), config()).await
}

#[tokio::test]
async fn clean_document_reports_ok() {
    let dir = store();
    let f = fixture(&dir).await;

    let result = f.call_tool("iwe_check", json!({"keys": ["notes/short"]})).await;
    let output = Fixture::result_json(&result);

    let arr = output.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["key"], "notes/short");
    assert_eq!(arr[0]["ok"], true);
    assert!(arr[0].get("violations").is_none());
}

#[tokio::test]
async fn violating_document_reports_violations() {
    let dir = store();
    let f = fixture(&dir).await;

    let result = f.call_tool("iwe_check", json!({"keys": ["notes/long"]})).await;
    let output = Fixture::result_json(&result);

    let arr = output.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["key"], "notes/long");
    assert_eq!(arr[0]["ok"], false);
    let violations = arr[0]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["keyword"], "maxTokens");
}

#[tokio::test]
async fn unknown_key_reports_ok_not_existence() {
    let dir = store();
    let f = fixture(&dir).await;

    let result = f
        .call_tool("iwe_check", json!({"keys": ["notes/does-not-exist"]}))
        .await;
    let output = Fixture::result_json(&result);

    let arr = output.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["key"], "notes/does-not-exist");
    assert_eq!(arr[0]["ok"], true);
}

#[tokio::test]
async fn multiple_keys_scoped_to_exactly_those_requested() {
    let dir = store();
    let f = fixture(&dir).await;

    let result = f
        .call_tool(
            "iwe_check",
            json!({"keys": ["notes/short", "notes/long"]}),
        )
        .await;
    let output = Fixture::result_json(&result);

    let arr = output.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["key"], "notes/short");
    assert_eq!(arr[0]["ok"], true);
    assert_eq!(arr[1]["key"], "notes/long");
    assert_eq!(arr[1]["ok"], false);
}
