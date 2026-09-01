// T21 — MCP-side counterpart to
// `crates/iwe/tests/freeze_multi_property_write_test.rs`. Same seven cases,
// same ruling (`efforts/knowledge-compositor/m2/design-freeze-semantics`),
// same "second axis" (how many properties one write call touches, not just
// which one) T13's matrix was missing. See that file's header comment for
// the ruling text in full and for the derivation of which cases currently
// pass vs. fail against this worktree's pre-fix
// `crates/diwe/src/permissions.rs` — the same derivation applies here
// unchanged, because both binaries funnel through the identical
// `check_write_permission_for_content[_in]` call (T13's code-path-identity
// finding, `enforcement_mode_matrix_test.rs`).
//
// Two MCP write shapes stand in for the CLI's two mechanisms:
//
//   - `iwe_query`'s `update` operation, `$set`/`$unset` operators — the MCP
//     analog of the CLI's `--set`/`--unset` frontmatter-mutation mode.
//     "Always strict": every mutating application must carry an `expect`
//     guard (`crates/iwec/src/lib.rs`'s `iwe_query` tool description), so
//     there is no separate ordinary/`--strict` split to observe here the
//     way the CLI file has one — MCP is uniformly path (b) per T13's
//     enforcement-mode taxonomy. Used for cases 1-6 (frontmatter-only).
//   - `iwe_update`, full-content-verbatim — the MCP analog of the CLI's
//     `-c`/`--content` body-overwrite mode (does not merge in existing
//     frontmatter for the caller, per `write_permission_test.rs`'s own
//     comment). Used for case 7, the only case that needs to touch the
//     body and frontmatter together in one call.
//
// Both funnel through `self.write_file` -> `check_write_permission_for_
// content_in`, the same call site T13 already source-traced as identical
// to the CLI's for write-permission purposes.
//
// Layer-free fixtures only: no `origin:`/`mint:`/package vocabulary.

use crate::fixture::Fixture;
use diwe::config::Configuration;
use diwe::permissions::FREEZE_FIELD;
use indoc::indoc;
use liwe::model::{parse_leading_frontmatter, split_raw_frontmatter};
use serde_json::json;
use serde_yaml::{Mapping, Value};
use std::fs::{read_to_string, write};
use tempfile::TempDir;

const FROZEN_DOC: &str =
    "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen Document\n\nOriginal body.\n";
const UNFROZEN_DOC: &str = "---\nstatus: draft\n---\n\n# Unfrozen Document\n\nOriginal body.\n";

// ---------------------------------------------------------------------
// Case 1 — the exact demonstrated bypass via `iwe_query`'s $set: freeze
// lifted + another property, one call. MUST be rejected.
// ---------------------------------------------------------------------

#[tokio::test]
async fn case1_mcp_freeze_false_plus_other_property_is_rejected() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    let result = f
        .try_call_tool(
            "iwe_query",
            json!({
                "operation": "update",
                "document": indoc! {"
                    filter: { $key: doc }
                    expect: 1
                    update:
                      $set: { freeze: false, status: reviewed }
                "},
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "a single write that lifts freeze AND changes another property \
         must be rejected wholesale via MCP too"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "rejection must name the document and attribute it to freeze: got {message}"
    );
    assert_eq!(
        read_to_string(base.join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
}

// ---------------------------------------------------------------------
// Case 2 — marker-removal variant: `$unset: { freeze }` + another
// property, one call. MUST be rejected.
// ---------------------------------------------------------------------

#[tokio::test]
async fn case2_mcp_freeze_unset_plus_other_property_is_rejected() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    let result = f
        .try_call_tool(
            "iwe_query",
            json!({
                "operation": "update",
                "document": indoc! {"
                    filter: { $key: doc }
                    expect: 1
                    update:
                      $set: { status: reviewed }
                      $unset: { freeze: '' }
                "},
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "removing the freeze key entirely is the same bypass shape as \
         setting it false; bundling another property change must still be \
         rejected"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "got {message}"
    );
    assert_eq!(read_to_string(base.join("doc.md")).unwrap(), before);
}

// ---------------------------------------------------------------------
// Case 3 — solitary unfreeze via `$set: { freeze: false }` alone. MUST
// succeed, result identical to prior except no longer frozen.
// ---------------------------------------------------------------------

#[tokio::test]
async fn case3_mcp_solitary_unfreeze_via_set_false_succeeds() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    f.call_tool(
        "iwe_query",
        json!({
            "operation": "update",
            "document": indoc! {"
                filter: { $key: doc }
                expect: 1
                update:
                  $set: { freeze: false }
            "},
        }),
    )
    .await;

    let after = read_to_string(base.join("doc.md")).unwrap();
    assert!(
        is_solitary_freeze_lift(&before, &after),
        "before={before:?} after={after:?}"
    );
}

// ---------------------------------------------------------------------
// Case 4 — solitary unfreeze via `$unset: { freeze }` alone. MUST succeed,
// same "sole effect" comparison as case 3.
// ---------------------------------------------------------------------

#[tokio::test]
async fn case4_mcp_solitary_unfreeze_via_unset_succeeds() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    f.call_tool(
        "iwe_query",
        json!({
            "operation": "update",
            "document": indoc! {"
                filter: { $key: doc }
                expect: 1
                update:
                  $unset: { freeze: '' }
            "},
        }),
    )
    .await;

    let after = read_to_string(base.join("doc.md")).unwrap();
    assert!(
        is_solitary_freeze_lift(&before, &after),
        "before={before:?} after={after:?}"
    );
    assert!(
        !frontmatter(&after).contains_key(Value::String(FREEZE_FIELD.to_string())),
        "marker removal means the key is gone, not merely false: {after:?}"
    );
}

// ---------------------------------------------------------------------
// Case 5 — freezing an *unfrozen* document plus another property, one
// call. MUST succeed: "freezing is not restricted" per the ruling.
// ---------------------------------------------------------------------

#[tokio::test]
async fn case5_mcp_freezing_unfrozen_document_plus_other_property_succeeds() {
    let (_dir, base, f) = setup(UNFROZEN_DOC).await;

    let result = f
        .try_call_tool(
            "iwe_query",
            json!({
                "operation": "update",
                "document": indoc! {"
                    filter: { $key: doc }
                    expect: 1
                    update:
                      $set: { freeze: true, status: reviewed }
                "},
            }),
        )
        .await;

    assert!(
        result.is_ok(),
        "freezing an unfrozen document may carry other changes in the same \
         call via MCP too — do not assume this must be rejected the way \
         cases 1/2 are: {:?}",
        result.err()
    );
    let after = read_to_string(base.join("doc.md")).unwrap();
    let fm = frontmatter(&after);
    assert_eq!(
        fm.get(Value::String(FREEZE_FIELD.to_string())),
        Some(&Value::Bool(true)),
        "{after:?}"
    );
    assert_eq!(
        fm.get(Value::String("status".to_string())),
        Some(&Value::String("reviewed".to_string())),
        "{after:?}"
    );
    assert_eq!(body(&after), body(UNFROZEN_DOC));
}

// ---------------------------------------------------------------------
// Case 6 — two separate `iwe_query` calls: unfreeze alone (succeeds), then
// a distinct write changing another property (succeeds too, against what
// is now an ordinary unfrozen document).
// ---------------------------------------------------------------------

#[tokio::test]
async fn case6_mcp_two_step_unfreeze_then_separate_write_succeeds() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    let unfreeze = f
        .try_call_tool(
            "iwe_query",
            json!({
                "operation": "update",
                "document": indoc! {"
                    filter: { $key: doc }
                    expect: 1
                    update:
                      $set: { freeze: false }
                "},
            }),
        )
        .await;
    assert!(
        unfreeze.is_ok(),
        "step 1 (solitary unfreeze) must succeed: {:?}",
        unfreeze.err()
    );
    let after_unfreeze = read_to_string(base.join("doc.md")).unwrap();
    assert!(is_solitary_freeze_lift(&before, &after_unfreeze));

    let second = f
        .try_call_tool(
            "iwe_query",
            json!({
                "operation": "update",
                "document": indoc! {"
                    filter: { $key: doc }
                    expect: 1
                    update:
                      $set: { status: reviewed }
                "},
            }),
        )
        .await;
    assert!(
        second.is_ok(),
        "step 2, a separate call against the now-unfrozen document, must \
         succeed: {:?}",
        second.err()
    );
    let after_second = read_to_string(base.join("doc.md")).unwrap();
    let fm = frontmatter(&after_second);
    assert_eq!(
        fm.get(Value::String("status".to_string())),
        Some(&Value::String("reviewed".to_string()))
    );
    assert!(!effectively_frozen(&after_second));
}

// ---------------------------------------------------------------------
// Case 7 — body-vs-frontmatter interaction via `iwe_update` (full-content
// verbatim): a frozen document, one call that clears freeze AND changes
// the body. MUST be rejected — the body counts as part of "every
// frontmatter property and the body" the sole-effect comparison covers.
// ---------------------------------------------------------------------

#[tokio::test]
async fn case7_mcp_body_change_plus_freeze_false_is_rejected() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    let result = f
        .try_call_tool(
            "iwe_update",
            json!({
                "key": "doc",
                "content": "---\nfreeze: false\nstatus: draft\n---\n\n# Frozen Document\n\nNew body.\n"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "clearing freeze (set false) and changing the body in the same \
         call must be rejected — the body is not exempt from the sole- \
         effect comparison"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "got {message}"
    );
    assert_eq!(read_to_string(base.join("doc.md")).unwrap(), before);
}

#[tokio::test]
async fn case7_mcp_body_change_plus_freeze_removed_is_rejected() {
    let (_dir, base, f) = setup(FROZEN_DOC).await;
    let before = read_to_string(base.join("doc.md")).unwrap();

    let result = f
        .try_call_tool(
            "iwe_update",
            json!({
                "key": "doc",
                "content": "---\nstatus: draft\n---\n\n# Frozen Document\n\nNew body.\n"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "removing the freeze key entirely and changing the body in the \
         same call must be rejected too, same as the set-false variant"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "got {message}"
    );
    assert_eq!(read_to_string(base.join("doc.md")).unwrap(), before);
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

async fn setup(doc_content: &str) -> (TempDir, std::path::PathBuf, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    write(base.join("doc.md"), doc_content).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;
    (dir, base, f)
}

fn frontmatter(content: &str) -> Mapping {
    parse_leading_frontmatter(content).unwrap_or_default()
}

fn body(content: &str) -> String {
    split_raw_frontmatter(content).1.to_string()
}

fn effectively_frozen(content: &str) -> bool {
    frontmatter(content)
        .get(Value::String(FREEZE_FIELD.to_string()))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The ruling's own definition of "sole effect": `after` is identical to
/// `before` in every frontmatter property and the body, except that
/// `before` was effectively frozen and `after` is not.
fn is_solitary_freeze_lift(before: &str, after: &str) -> bool {
    if !effectively_frozen(before) || effectively_frozen(after) {
        return false;
    }
    let mut before_fm = frontmatter(before);
    let mut after_fm = frontmatter(after);
    before_fm.remove(Value::String(FREEZE_FIELD.to_string()));
    after_fm.remove(Value::String(FREEZE_FIELD.to_string()));
    before_fm == after_fm && body(before) == body(after)
}
