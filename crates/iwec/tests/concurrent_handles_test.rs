//! Concurrent two-open-handles acceptance test for design-7
//! (efforts/knowledge-compositor/m6-b-cutover-preconditions/design-7,
//! "transaction handles in the MCP server", revision confirmed 2026-09-05
//! at crates/iwec @ 087aa1a). Built from the acceptance criterion alone —
//! written without reading the handle plumbing's implementation: every
//! identifier used below comes from the design revision's pinned shared
//! surface (caller-supplied optional `handle: String` on every
//! transaction-participating tool, omitted resolves to the reserved
//! `"default"` slot, `iwe_tx_begin` echoes the resolved key back) plus
//! this crate's pre-existing, already-committed test fixture and MCP-tool
//! conventions (agent_transaction_test.rs, tx_commit_lock_wiring_test.rs).
//!
//! Criterion (design-7, confirmed 2026-09-05): two explicit transaction
//! handles can be open at once in one iwec server; interleaved staged
//! writes on the two handles stay isolated; a per-key conflict (both
//! handles write the same key) is detected at commit of the second; and
//! non-conflicting keys commit on both.
//!
//! Per-key conflict detection is the design's existing mechanism, not new
//! logic: each transaction's backend already re-checks its staged keys
//! against on-disk state at that transaction's own commit, so a staged
//! key changed underneath it by another handle's earlier commit surfaces
//! as the same "write conflict" refusal the pre-existing suite observes
//! for external writers (agent_transaction_test.rs's
//! `a_staged_key_changed_on_disk_underneath_the_transaction_refuses_the_commit`).
//!
//! Two tests, one per scenario the criterion separates:
//!
//!   - `two_explicit_handles_open_at_once...`: disjoint write sets,
//!     interleaved — proves both begins succeed together (multiplicity),
//!     each commit lands exactly its own handle's keys and nothing of the
//!     other's (isolation), and both commits land with one journal record
//!     each (the "non-conflicting keys commit on both" half).
//!
//!   - `a_per_key_conflict_between_handles...`: both handles write the
//!     same key — proves the conflict is detected at commit of the
//!     second (final commit refused, naming the key) while the first's
//!     version stands unclobbered and the second's refusal lands nothing
//!     at all (commit is whole-unit, the design pins "no new logic").
//!
//! VERIFICATION STATUS: run against crates/iwec at HEAD; see the delivery
//! report in the invocation for pass/fail counts.

use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};

use diwe::config::{
    Configuration, JournalOptions, Patterns, SchemaBinding, TransactionOptions, ValidationScope,
};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::fixture::Fixture;

const HUB: &str = "---\ntype: note\n---\n# Hub\n\nSee [Leaf](leaf).\n";
const LEAF: &str = "---\ntype: note\n---\n# Leaf\n\nBack to [Hub](hub).\n";

/// Same store shape as `agent_transaction_test.rs`: `notes/**` under a
/// schema that requires every link to resolve to a note.
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
        transactions: TransactionOptions { validate: scope, ..Default::default() },
        journal: JournalOptions {
            path: Some(".iwe/journal.ndjson".to_string()),
        },
        ..Default::default()
    }
}

async fn fixture(dir: &TempDir, scope: ValidationScope) -> Fixture {
    let base = dir.path().canonicalize().unwrap();
    Fixture::with_path(base.to_str().unwrap(), config(scope)).await
}

fn journal_records(dir: &TempDir) -> Vec<Value> {
    read_to_string(dir.path().join(".iwe/journal.ndjson"))
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("journal record parses"))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Criterion halves 1, 2 and 4: two explicit handles open at once,
// interleaved staged writes stay isolated, non-conflicting keys commit on
// both handles.
// ---------------------------------------------------------------------------

/// Two explicit-handle transactions open at once in one server; their
/// interleaved staged writes stay isolated (each commit carries exactly
/// its own handle's keys); both commits land.
#[tokio::test]
async fn two_explicit_handles_open_at_once_isolate_interleaved_writes_and_both_commit() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Multiplicity: the second explicit begin must not be refused while
    // the first is open — only a second begin on the *same* occupied key
    // is (a second "alpha" is refused; a first "beta" is not).
    let begun = f
        .call_tool("iwe_tx_begin", json!({"handle": "alpha"}))
        .await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    assert!(
        Fixture::result_text(&begun).contains("alpha"),
        "iwe_tx_begin must echo the resolved handle key back, got: {:?}",
        Fixture::result_text(&begun)
    );

    let begun = f
        .call_tool("iwe_tx_begin", json!({"handle": "beta"}))
        .await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    assert!(
        Fixture::result_text(&begun).contains("beta"),
        "iwe_tx_begin must echo the resolved handle key back, got: {:?}",
        Fixture::result_text(&begun)
    );

    let message = f
        .try_call_tool("iwe_tx_begin", json!({"handle": "alpha"}))
        .await
        .expect_err("a second begin on an occupied explicit handle must be refused")
        .to_string();
    assert!(message.contains("already open"), "{message}");

    // Interleaved staged writes across the two handles.
    let a1 = "---\ntype: note\n---\n# A1\n\nSee [Hub](hub).\n";
    let a2 = "---\ntype: note\n---\n# A2\n\nSee [Hub](hub).\n";
    let b1 = "---\ntype: note\n---\n# B1\n\nSee [Hub](hub).\n";
    let b2 = "---\ntype: note\n---\n# B2\n\nSee [Hub](hub).\n";
    for (handle, key, content) in [
        ("alpha", "notes/a1", a1),
        ("beta", "notes/b1", b1),
        ("alpha", "notes/a2", a2),
        ("beta", "notes/b2", b2),
    ] {
        let created = f
            .call_tool(
                "iwe_create",
                json!({"handle": handle, "key": key, "content": content}),
            )
            .await;
        assert!(!created.is_error.unwrap_or(false), "{created:?}");
    }

    // Nothing lands on disk before any commit.
    for key in ["notes/a1", "notes/a2", "notes/b1", "notes/b2"] {
        assert!(!dir.path().join(format!("{key}.md")).exists(), "{key} on disk pre-commit");
    }
    assert!(journal_records(&dir).is_empty(), "nothing is journaled before commit");

    // Isolation, first direction: the first commit carries *only* its own
    // handle's staged writes — beta's keys must not ride along in alpha's
    // commit and must not be on disk afterward.
    let committed = f
        .call_tool("iwe_tx_commit", json!({"handle": "alpha"}))
        .await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(dir.path().join("notes/a1.md").exists());
    assert!(dir.path().join("notes/a2.md").exists());
    assert!(
        !dir.path().join("notes/b1.md").exists(),
        "beta's staged write leaked into alpha's commit"
    );
    assert!(
        !dir.path().join("notes/b2.md").exists(),
        "beta's staged write leaked into alpha's commit"
    );

    // Non-conflicting keys commit on both handles: beta's own commit lands
    // its remaining keys, still only its own.
    let committed = f
        .call_tool("iwe_tx_commit", json!({"handle": "beta"}))
        .await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(dir.path().join("notes/b1.md").exists());
    assert!(dir.path().join("notes/b2.md").exists());

    // One journal record per commit, each carrying exactly its own
    // handle's keys (write order within a commit is not part of the
    // criterion, so key sets are compared sorted).
    let records = journal_records(&dir);
    assert_eq!(records.len(), 2, "two commits, two journal records: {records:?}");
    let effects_of = |record: &Value| {
        let mut keys: Vec<String> = record["effects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap().to_string())
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(effects_of(&records[0]), vec!["notes/a1", "notes/a2"]);
    assert_eq!(effects_of(&records[1]), vec!["notes/b1", "notes/b2"]);
}

// ---------------------------------------------------------------------------
// Criterion half 3: a per-key conflict (both handles write the same key)
// is detected at commit of the second; the first's version stands, the
// second's refusal lands nothing.
// ---------------------------------------------------------------------------

/// Both handles stage a write to the same key; the second commit to reach
/// the finish is refused — its staged key's on-disk state was changed by
/// the first handle's commit — and the refusal lands nothing at all.
#[tokio::test]
async fn a_per_key_conflict_between_handles_is_detected_at_the_second_commit_and_lands_nothing() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    f.call_tool("iwe_tx_begin", json!({"handle": "beta"})).await;

    let mine = "---\ntype: note\n---\n# Leaf\n\nMine [Hub](hub).\n";
    let theirs = "---\ntype: note\n---\n# Leaf\n\nTheirs [Hub](hub).\n";
    let beta_own = "---\ntype: note\n---\n# Beta own\n\nSee [Hub](hub).\n";

    // Both handles write notes/leaf, with different content; beta also
    // stages a second, conflict-free key that nothing else touches. All
    // staging succeeds — each handle stages against its own isolated
    // snapshot; the conflict is a commit-time fact, not a staging-time one.
    let updated = f
        .call_tool(
            "iwe_update",
            json!({"handle": "alpha", "key": "notes/leaf", "content": mine}),
        )
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
    let updated = f
        .call_tool(
            "iwe_update",
            json!({"handle": "beta", "key": "notes/leaf", "content": theirs}),
        )
        .await;
    assert!(!updated.is_error.unwrap_or(false), "{updated:?}");
    let created = f
        .call_tool(
            "iwe_create",
            json!({"handle": "beta", "key": "notes/beta_own", "content": beta_own}),
        )
        .await;
    assert!(!created.is_error.unwrap_or(false), "{created:?}");

    // First commit lands its version of the shared key.
    let committed = f
        .call_tool("iwe_tx_commit", json!({"handle": "alpha"}))
        .await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert_eq!(read_to_string(dir.path().join("notes/leaf.md")).unwrap(), mine);

    // Commit of the second is refused: its staged notes/leaf no longer
    // matches on-disk state (alpha's commit landed first). Same detection
    // mechanism and wording the pre-existing suite observes for external
    // writers changing a staged key underneath a transaction.
    let message = f
        .try_call_tool("iwe_tx_commit", json!({"handle": "beta"}))
        .await
        .expect_err(
            "the second handle's commit must be refused: its staged notes/leaf was changed \
             on disk by alpha's commit",
        )
        .to_string();
    assert!(message.contains("write conflict"), "{message}");
    assert!(message.contains("notes/leaf"), "{message}");

    // The first's version is not clobbered, and the refused commit's
    // non-conflicting key did not land either — a refused commit is a
    // whole-unit refusal.
    assert_eq!(read_to_string(dir.path().join("notes/leaf.md")).unwrap(), mine);
    assert!(
        !dir.path().join("notes/beta_own.md").exists(),
        "a refused commit lands nothing, not even its conflict-free keys"
    );
    let records = journal_records(&dir);
    assert_eq!(records.len(), 1, "only alpha's commit is journaled: {records:?}");
}