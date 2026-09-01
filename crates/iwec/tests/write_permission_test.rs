// T10 (EXT-FREEZE): a document carrying `freeze: true` in its own
// frontmatter has every write to it rejected via MCP too — the same shared
// write-permission check CLI paths go through
// (`diwe::permissions::check_write_permission`), unconditionally regardless
// of invocation shape. Layer-free fixtures only: no `origin:`/`mint:`/
// package vocabulary.

use crate::fixture::Fixture;
use diwe::config::Configuration;
use serde_json::json;

const FROZEN_DOC: &str = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen Document\n\nOriginal body.\n";
const UNFROZEN_DOC: &str = "---\nstatus: draft\n---\n\n# Unfrozen Document\n\nOriginal body.\n";

#[tokio::test]
async fn update_of_a_frozen_document_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::write(base.join("doc.md"), FROZEN_DOC).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    // `iwe_update` replaces the document's full content verbatim (unlike
    // the CLI's body-overwrite mode, it does not merge in the existing
    // frontmatter for you — see its tool description), so a well-behaved
    // caller preserves the frontmatter it already read; this is that case:
    // still carrying `freeze: true`, still rejected.
    let result = f
        .try_call_tool(
            "iwe_update",
            json!({"key": "doc", "content": "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen Document\n\nNew body.\n"}),
        )
        .await;

    assert!(result.is_err(), "update of a frozen document must fail");
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "error should name the document and the rule, got: {message}"
    );

    let on_disk = std::fs::read_to_string(base.join("doc.md")).unwrap();
    assert_eq!(on_disk, FROZEN_DOC, "frozen document must be unchanged on disk");
}

#[tokio::test]
async fn create_of_a_frozen_document_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .try_call_tool(
            "iwe_create",
            json!({"key": "brand-new", "content": "---\nfreeze: true\n---\n\n# Brand New\n\nBody.\n"}),
        )
        .await;

    assert!(result.is_err(), "create of a frozen document must fail");
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("brand-new") && message.contains("frozen"),
        "error should name the document and the rule, got: {message}"
    );
    assert!(!base.join("brand-new.md").exists());
}

#[tokio::test]
async fn update_of_an_unfrozen_document_succeeds_unchanged() {
    // AB9: no freeze marker means no new rejection.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::write(base.join("doc.md"), UNFROZEN_DOC).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .call_tool(
            "iwe_update",
            json!({"key": "doc", "content": "# Unfrozen Document\n\nNew body.\n"}),
        )
        .await;
    let output = Fixture::result_json(&result);
    assert_eq!(output["key"], "doc");

    let on_disk = std::fs::read_to_string(base.join("doc.md")).unwrap();
    assert!(on_disk.contains("New body."));
}
