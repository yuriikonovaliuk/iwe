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
async fn create_of_a_frozen_document_succeeds() {
    // M2 fix-wave (`m2/design-freeze-semantics`): the write-permission
    // predicate was corrected to gate on the document's *prior* on-disk
    // state, not on the outgoing content being written. A brand-new
    // document has no prior state at all, so there is nothing frozen to
    // violate — creating a document that carries `freeze: true` from the
    // moment it is created must succeed ("Freezing is not restricted").
    //
    // This test used to assert the opposite (rejection) — that was the
    // pre-fix, outgoing-content-shaped predicate's behavior, which is what
    // this fix-wave corrected; updated here to match the corrected rule.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .call_tool(
            "iwe_create",
            json!({"key": "brand-new", "content": "---\nfreeze: true\n---\n\n# Brand New\n\nBody.\n"}),
        )
        .await;
    let output = Fixture::result_json(&result);
    assert_eq!(output["key"], "brand-new");
    assert!(base.join("brand-new.md").exists());
}

#[tokio::test]
async fn a_single_write_that_lifts_freeze_and_changes_another_field_is_rejected() {
    // The bypass itself (`m2/design-freeze-semantics`): a single write that
    // both lifts freeze and changes another field used to be evaluated only
    // against its own (now-unfrozen) resulting content, so both changes
    // landed. It must now be rejected as a whole, since its effect is not
    // *solely* lifting freeze.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::write(base.join("doc.md"), FROZEN_DOC).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .try_call_tool(
            "iwe_update",
            json!({"key": "doc", "content": "---\nfreeze: false\nstatus: changed\n---\n\n# Frozen Document\n\nOriginal body.\n"}),
        )
        .await;

    assert!(result.is_err(), "the bypass must still be rejected");
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "error should name the document and the rule, got: {message}"
    );

    let on_disk = std::fs::read_to_string(base.join("doc.md")).unwrap();
    assert_eq!(
        on_disk, FROZEN_DOC,
        "the bypass must not land: neither freeze nor status may change"
    );
}

#[tokio::test]
async fn a_solitary_unfreeze_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::write(base.join("doc.md"), FROZEN_DOC).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .call_tool(
            "iwe_update",
            json!({"key": "doc", "content": "---\nfreeze: false\nstatus: draft\n---\n\n# Frozen Document\n\nOriginal body.\n"}),
        )
        .await;
    let output = Fixture::result_json(&result);
    assert_eq!(output["key"], "doc");

    let on_disk = std::fs::read_to_string(base.join("doc.md")).unwrap();
    assert!(on_disk.contains("freeze: false"), "{on_disk}");
    assert!(on_disk.contains("status: draft"), "{on_disk}");
}

#[tokio::test]
async fn freezing_a_previously_unfrozen_document_plus_other_changes_succeeds() {
    // Detail (b) from the ruling: freezing itself is unrestricted.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::write(base.join("doc.md"), UNFROZEN_DOC).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .call_tool(
            "iwe_update",
            json!({"key": "doc", "content": "---\nfreeze: true\nstatus: reviewed\n---\n\n# Unfrozen Document\n\nNew body.\n"}),
        )
        .await;
    let output = Fixture::result_json(&result);
    assert_eq!(output["key"], "doc");

    let on_disk = std::fs::read_to_string(base.join("doc.md")).unwrap();
    assert!(on_disk.contains("freeze: true"), "{on_disk}");
    assert!(on_disk.contains("status: reviewed"), "{on_disk}");
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
