// Agent transactions at the MCP surface: `iwe_tx_begin` opens one, every
// write tool then stages into it (the agent's own reads see the staged
// state, disk sees nothing), and `iwe_tx_commit` validates the final
// state as one unit and lands it atomically with a single journal record
// — or refuses it whole. `iwe_tx_abort` discards it. The point: several
// documents that are only valid together (two notes linking each other,
// a hub and its leaves) can be written without ever passing through a
// state the gate would refuse one write at a time.

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

/// `notes/**`: every link must resolve to a note — a rule only the
/// store-level gate can judge, so a note linking to one not yet written
/// is exactly the write a transaction exists for.
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
        transactions: TransactionOptions { validate: scope },
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

fn retrieved_text(f: &Fixture, result: &rmcp::model::CallToolResult) -> String {
    let _ = f;
    result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn two_notes_that_only_resolve_together_land_as_one_commit_with_one_journal_record() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let begun = f.call_tool("iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");

    // Alone, each of these dangles; the gate would refuse either one
    // committed by itself.
    let a = "---\ntype: note\n---\n# A\n\nSee [B](b).\n";
    let b = "---\ntype: note\n---\n# B\n\nSee [A](a).\n";
    let created = f
        .call_tool("iwe_create", json!({"key": "notes/a", "content": a}))
        .await;
    assert!(!created.is_error.unwrap_or(false), "{created:?}");
    let created = f
        .call_tool("iwe_create", json!({"key": "notes/b", "content": b}))
        .await;
    assert!(!created.is_error.unwrap_or(false), "{created:?}");

    // Staged: the agent's own read sees it, disk does not.
    let seen = f.call_tool("iwe_retrieve", json!({"keys": ["notes/a"]})).await;
    assert!(
        retrieved_text(&f, &seen).contains("See [B]"),
        "the staged document is readable inside the transaction, got: {}",
        retrieved_text(&f, &seen)
    );
    assert!(!dir.path().join("notes/a.md").exists());
    assert!(!dir.path().join("notes/b.md").exists());
    assert!(journal_records(&dir).is_empty(), "nothing is journaled before commit");

    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    let report = Fixture::result_json(&committed);
    assert_eq!(report["status"], "committed");
    assert_eq!(report["keys"], json!(["notes/a", "notes/b"]));

    assert_eq!(read_to_string(dir.path().join("notes/a.md")).unwrap(), a);
    assert_eq!(read_to_string(dir.path().join("notes/b.md")).unwrap(), b);

    let records = journal_records(&dir);
    assert_eq!(records.len(), 1, "one record for the whole transaction: {records:?}");
    let effects = records[0]["effects"].as_array().unwrap();
    let keys: Vec<&str> = effects.iter().map(|e| e["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["notes/a", "notes/b"]);
    assert!(effects.iter().all(|e| e["effect"] == "create"), "{effects:?}");
}

#[tokio::test]
async fn a_refused_commit_lands_nothing_and_drops_the_staged_state() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    let clean = "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n";
    let dangling = "---\ntype: note\n---\n# Bad\n\nSee [Nowhere](nowhere).\n";
    f.call_tool("iwe_create", json!({"key": "notes/new", "content": clean}))
        .await;
    f.call_tool("iwe_create", json!({"key": "notes/bad", "content": dangling}))
        .await;

    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("the dangling link refuses the whole transaction")
        .to_string();
    assert!(
        message.contains("notes/bad") && message.contains("nowhere"),
        "the refusal names the fault, got: {message}"
    );
    assert!(
        message.contains("notes/new"),
        "the refusal names every discarded document, got: {message}"
    );

    // The clean write did not land either: all or nothing.
    assert!(!dir.path().join("notes/new.md").exists());
    assert!(!dir.path().join("notes/bad.md").exists());
    assert!(journal_records(&dir).is_empty());

    // The staged state is gone from the graph, and no transaction is open.
    // (A key with no document retrieves as an empty placeholder.)
    let seen = f.call_tool("iwe_retrieve", json!({"keys": ["notes/new"]})).await;
    assert!(
        !retrieved_text(&f, &seen).contains("See [Hub]"),
        "the discarded document is not retrievable, got: {}",
        retrieved_text(&f, &seen)
    );
    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("nothing left to commit")
        .to_string();
    assert!(message.contains("no transaction is open"), "{message}");
}

#[tokio::test]
async fn abort_discards_every_staged_write() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    f.call_tool(
        "iwe_update",
        json!({"key": "notes/leaf", "content": "---\ntype: note\n---\n# Leaf\n\nRewritten [Hub](hub).\n"}),
    )
    .await;
    f.call_tool("iwe_delete", json!({"key": "notes/hub"})).await;

    let aborted = f.call_tool("iwe_tx_abort", json!({})).await;
    let report = Fixture::result_json(&aborted);
    assert_eq!(report["status"], "aborted");
    assert_eq!(report["discarded"], json!(["notes/leaf", "notes/hub"]));

    assert_eq!(read_to_string(dir.path().join("notes/leaf.md")).unwrap(), LEAF);
    assert_eq!(read_to_string(dir.path().join("notes/hub.md")).unwrap(), HUB);
    let seen = f.call_tool("iwe_retrieve", json!({"keys": ["notes/hub"]})).await;
    assert!(
        retrieved_text(&f, &seen).contains("See [Leaf]"),
        "the graph is back to what is on disk, got: {}",
        retrieved_text(&f, &seen)
    );
    assert!(journal_records(&dir).is_empty());
}

#[tokio::test]
async fn a_staged_key_changed_on_disk_underneath_the_transaction_refuses_the_commit() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    f.call_tool(
        "iwe_update",
        json!({"key": "notes/leaf", "content": "---\ntype: note\n---\n# Leaf\n\nMine [Hub](hub).\n"}),
    )
    .await;
    // Another agent lands its own write to the same document.
    let theirs = "---\ntype: note\n---\n# Leaf\n\nTheirs [Hub](hub).\n";
    write(dir.path().join("notes/leaf.md"), theirs).unwrap();

    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("the conflict refuses the commit")
        .to_string();
    assert!(
        message.contains("write conflict") && message.contains("notes/leaf"),
        "{message}"
    );
    assert_eq!(
        read_to_string(dir.path().join("notes/leaf.md")).unwrap(),
        theirs,
        "the other agent's write is not clobbered"
    );
}

#[tokio::test]
async fn a_concurrent_external_holder_of_the_commit_lock_times_out_the_commit_and_applies_nothing()
{
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    let clean = "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n";
    f.call_tool("iwe_create", json!({"key": "notes/new", "content": clean}))
        .await;

    // Another process holds the store-wide commit lock for the whole
    // window this commit attempt will try to acquire it in.
    let root = dir.path().canonicalize().unwrap();
    let holder_root = root.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let guard =
            liwe::write_lock::acquire_commit_lock(&holder_root).expect("external holder acquires");
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(guard);
    });
    ready_rx.recv().unwrap();

    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("the lock is held elsewhere; the commit must refuse rather than block")
        .to_string();
    assert!(message.contains("timed out"), "{message}");
    assert!(message.contains("remains open"), "{message}");

    release_tx.send(()).unwrap();
    holder.join().unwrap();

    // Nothing landed while refused.
    assert!(!dir.path().join("notes/new.md").exists());
    assert!(journal_records(&dir).is_empty());

    // The transaction is still open: once the lock frees up, a retry
    // commits it.
    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(dir.path().join("notes/new.md").exists());
}

#[tokio::test]
async fn a_commit_lock_reclaimed_before_apply_is_caught_by_fencing_and_the_transaction_stays_open()
{
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    let clean = "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n";
    f.call_tool("iwe_create", json!({"key": "notes/new", "content": clean}))
        .await;

    let root = dir.path().canonicalize().unwrap();
    let lock_path = root.join(liwe::write_lock::DEFAULT_LOCK_PATH);

    // A real commit's gap between acquiring the store commit lock and
    // checking its fencing is otherwise microseconds wide — too narrow
    // for an external test to land a reclaim inside deterministically.
    // `ValidatingTransaction::commit` (the backend both the CLI and this
    // server's agent transactions share, 5-iwe-t3) reads
    // `IWE_TEST_LOCK_FENCING_DELAY_MS` for exactly this: widening that
    // gap so a test can land inside it.
    // SAFETY: no other thread reads/writes the process environment while
    // this test runs (removed again before this function returns).
    unsafe { std::env::set_var("IWE_TEST_LOCK_FENCING_DELAY_MS", "150") };

    // An aggressive external reclaimer: waits just long enough for the
    // in-flight commit's own acquire to land (a plain in-process file
    // lock acquire, far faster than the MCP round trip below), then
    // steals the lock — a `stale_after` far tighter than the production
    // heartbeat cadence — bumping its generation and releasing it again,
    // comfortably inside the widened window above, before the commit
    // reaches its own fencing check.
    let reclaimer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let lock = iwe_lock::FileLock::new(iwe_lock::LockConfig {
            path: lock_path,
            heartbeat_interval: std::time::Duration::from_millis(1),
            stale_after: std::time::Duration::from_millis(5),
            acquire_timeout: std::time::Duration::from_secs(2),
        });
        let stolen = lock
            .acquire()
            .expect("the aggressive reclaimer supersedes the live hold");
        stolen.release();
    });

    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("the lock was reclaimed before the irreversible apply step")
        .to_string();
    reclaimer.join().unwrap();
    // SAFETY: see the comment at the matching `set_var` above.
    unsafe { std::env::remove_var("IWE_TEST_LOCK_FENCING_DELAY_MS") };

    assert!(message.contains("superseded"), "{message}");
    assert!(message.contains("remains open"), "{message}");

    // Nothing landed, and the staged write was not discarded either.
    assert!(!dir.path().join("notes/new.md").exists());
    assert!(journal_records(&dir).is_empty());

    // The transaction is still open: a retry, once the lock is free
    // again, commits it.
    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(dir.path().join("notes/new.md").exists());
}

#[tokio::test]
async fn a_rename_inside_a_transaction_stages_its_whole_change_set() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    let renamed = f
        .call_tool(
            "iwe_rename",
            json!({"old_key": "notes/leaf", "new_key": "notes/renamed"}),
        )
        .await;
    assert!(!renamed.is_error.unwrap_or(false), "{renamed:?}");
    assert!(dir.path().join("notes/leaf.md").exists(), "nothing lands before commit");

    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(!dir.path().join("notes/leaf.md").exists());
    assert!(dir.path().join("notes/renamed.md").exists());
    assert!(read_to_string(dir.path().join("notes/hub.md"))
        .unwrap()
        .contains("(renamed)"));

    let records = journal_records(&dir);
    assert_eq!(records.len(), 1, "{records:?}");
    let effects: Vec<(String, String)> = records[0]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["key"].as_str().unwrap().to_string(),
                e["effect"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(effects.contains(&("notes/leaf".to_string(), "delete".to_string())), "{effects:?}");
    assert!(effects.contains(&("notes/renamed".to_string(), "create".to_string())), "{effects:?}");
    assert!(effects.contains(&("notes/hub".to_string(), "update".to_string())), "{effects:?}");
}

#[tokio::test]
async fn a_second_begin_is_refused_while_one_is_open() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    f.call_tool("iwe_tx_begin", json!({})).await;
    f.call_tool(
        "iwe_create",
        json!({"key": "notes/new", "content": "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n"}),
    )
    .await;
    let message = f
        .try_call_tool("iwe_tx_begin", json!({}))
        .await
        .expect_err("one transaction at a time")
        .to_string();
    assert!(
        message.contains("already open") && message.contains("notes/new"),
        "{message}"
    );
}

#[tokio::test]
async fn without_the_transactions_section_begin_is_refused() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::None).await;

    let message = f
        .try_call_tool("iwe_tx_begin", json!({}))
        .await
        .expect_err("no validating backend to stage on")
        .to_string();
    assert!(message.contains("[transactions] validate"), "{message}");
}
