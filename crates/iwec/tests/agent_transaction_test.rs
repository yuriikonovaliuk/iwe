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
        transactions: TransactionOptions {
            validate: scope,
            ..Default::default()
        },
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

#[tokio::test]
async fn a_second_explicit_begin_is_refused_while_a_different_explicit_one_is_open() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Three explicit handles open at once — multiplicity is not refused;
    // only a *second* begin naming an already-occupied key is. `h3`'s own
    // handle string is literally "default", the reserved slot an omitted
    // `handle` resolves to — an explicit handle can occupy that slot too,
    // and once it does, an omitted-handle begin finds it occupied exactly
    // like any other explicit collision.
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "h1"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({
            "handle": "h1",
            "key": "notes/h1",
            "content": "---\ntype: note\n---\n# H1\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "h2"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({
            "handle": "h2",
            "key": "notes/h2",
            "content": "---\ntype: note\n---\n# H2\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "default"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({
            "handle": "default",
            "key": "notes/h3",
            "content": "---\ntype: note\n---\n# H3\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    // A fourth begin naming any of the three occupied keys — explicit
    // `h1`, explicit `h2`, or the default key (occupied by `h3`, above) —
    // is refused, and each refusal names the conflicting transaction's
    // staged keys, same pattern as `a_second_begin_is_refused_while_one_is_open`.
    let message = f
        .try_call_tool("iwe_tx_begin", json!({"handle": "h1"}))
        .await
        .expect_err("h1 is already open")
        .to_string();
    assert!(message.contains("already open") && message.contains("notes/h1"), "{message}");

    let message = f
        .try_call_tool("iwe_tx_begin", json!({"handle": "h2"}))
        .await
        .expect_err("h2 is already open")
        .to_string();
    assert!(message.contains("already open") && message.contains("notes/h2"), "{message}");

    let message = f
        .try_call_tool("iwe_tx_begin", json!({}))
        .await
        .expect_err("the default key is already open, occupied by the explicit h3 begin")
        .to_string();
    assert!(message.contains("already open") && message.contains("notes/h3"), "{message}");
}

// ---------------------------------------------------------------------------
// T3 resolved-handle echo test (design-7). Contract:
//   `iwe_tx_begin` echoes the *resolved* map key, not the caller's raw
//   input: an explicit `handle: "alpha"` echoes "alpha"; an omitted
//   `handle` echoes the reserved default key, `DEFAULT_TX_HANDLE`
//   ("default") — not, say, an empty string or some other placeholder for
//   "no handle given". Two independent transactions, one per case, so a
//   later assertion can't be satisfied by an earlier one's leftover state.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tx_begin_echoes_the_resolved_handle_key_not_the_raw_input() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Explicit handle: the echoed key is exactly the caller's string.
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let text = Fixture::result_text(&begun);
    assert!(
        text.contains("alpha"),
        "iwe_tx_begin with handle: \"alpha\" must echo \"alpha\" back, got: {text}"
    );
    f.call_tool("iwe_tx_abort", json!({"handle": "alpha"})).await;

    // Omitted handle: the echoed key is the resolved map key
    // (DEFAULT_TX_HANDLE), never the caller's absent input — there is no
    // raw string to echo, so what comes back can only be the server's own
    // resolution of "no handle given" to its reserved default slot.
    let begun = f.call_tool("iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let text = Fixture::result_text(&begun);
    assert!(
        text.contains("default"),
        "iwe_tx_begin with no handle must echo the resolved key \"default\", got: {text}"
    );
}

// ---------------------------------------------------------------------------
// T1 isolation + commit-separation tests (design-7). Contract:
//   1. `two_explicit_handles_open_at_once_each_sees_its_own_staged_state`
//      Two explicit transaction handles can be open at once in one iwec
//      server; interleaved staged writes on the two handles stay isolated
//      (each handle's own read sees only its own staged writes); a
//      no-handle or other-handle read sees neither side's staged state;
//      committing one handle leaves the other unaffected, then the second
//      commits; both land as separate journal records.
//   2. `a_key_changed_by_one_open_handle_refuses_the_other_handle_commit`
//      When handle A commits a change to a key, handle B's later commit
//      carrying a stale stage of the same key is refused as a whole and
//      lands no part of B's transaction.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_explicit_handles_open_at_once_each_sees_its_own_staged_state() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Both handles open at once in the same server.
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    assert!(
        Fixture::result_text(&begun).contains("alpha"),
        "iwe_tx_begin must echo the resolved handle key back, got: {:?}",
        Fixture::result_text(&begun)
    );

    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "beta"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    assert!(
        Fixture::result_text(&begun).contains("beta"),
        "iwe_tx_begin must echo the resolved handle key back, got: {:?}",
        Fixture::result_text(&begun)
    );

    // Interleaved staged writes against distinct keys, one tool call per
    // (handle, key) pair, alternating the two handles so neither's stages
    // are batched ahead of the other's.
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

    // Nothing has landed on disk and nothing has been journaled yet —
    // both transactions are still open.
    for key in ["notes/a1", "notes/a2", "notes/b1", "notes/b2"] {
        assert!(
            !dir.path().join(format!("{key}.md")).exists(),
            "{key} landed on disk before any commit"
        );
    }
    assert!(
        journal_records(&dir).is_empty(),
        "nothing is journaled before commit"
    );

    // Each handle's own read sees only its own staged writes: alpha
    // sees alpha's two staged keys, beta sees beta's two. A read issued
    // from the other handle must not see the first handle's staged
    // state, and a no-handle read sees neither side's staged state.
    //
    // The contract names `iwe_retrieve` as the read tool. The MCP tool
    // surface routes handle-aware reads via the `handle` parameter on
    // any tx-participating tool (the only tool that accepts it as an
    // input and reads), exercised here through `iwe_query find` with
    // `handle` set — the read surface whose routing actually follows
    // the handle, mirroring `iwe_retrieve`'s role on the no-handle side.
    // The contract's two negative clauses (no-handle and other-handle
    // reads see neither side's staged state) are then asserted directly
    // against `iwe_retrieve` itself, the canonical read tool.
    let alpha_sees = Fixture::result_json(
        &f.call_tool(
            "iwe_query",
            json!({
                "operation": "find",
                "handle": "alpha",
                "document": "filter: { $key: { $in: ['notes/a1', 'notes/a2', 'notes/b1', 'notes/b2'] } }\n",
            }),
        )
        .await,
    );
    let alpha_keys: Vec<String> = alpha_sees
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["key"].as_str().unwrap().to_string())
        .collect();
    assert!(
        alpha_keys.contains(&"notes/a1".to_string())
            && alpha_keys.contains(&"notes/a2".to_string()),
        "alpha's own read sees alpha's staged keys a1+a2, got: {alpha_keys:?}"
    );
    assert!(
        !alpha_keys.contains(&"notes/b1".to_string())
            && !alpha_keys.contains(&"notes/b2".to_string()),
        "alpha's own read must not see beta's staged keys b1/b2, got: {alpha_keys:?}"
    );

    let beta_sees = Fixture::result_json(
        &f.call_tool(
            "iwe_query",
            json!({
                "operation": "find",
                "handle": "beta",
                "document": "filter: { $key: { $in: ['notes/a1', 'notes/a2', 'notes/b1', 'notes/b2'] } }\n",
            }),
        )
        .await,
    );
    let beta_keys: Vec<String> = beta_sees
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["key"].as_str().unwrap().to_string())
        .collect();
    assert!(
        beta_keys.contains(&"notes/b1".to_string())
            && beta_keys.contains(&"notes/b2".to_string()),
        "beta's own read sees beta's staged keys b1+b2, got: {beta_keys:?}"
    );
    assert!(
        !beta_keys.contains(&"notes/a1".to_string())
            && !beta_keys.contains(&"notes/a2".to_string()),
        "beta's own read must not see alpha's staged keys a1/a2, got: {beta_keys:?}"
    );

    // No-handle `iwe_retrieve` — the shared-graph view — sees neither
    // side's staged state. Each staged key is absent from the shared
    // graph (and not yet on disk).
    let no_handle_sees_a1 = retrieved_text(
        &f,
        &f.call_tool("iwe_retrieve", json!({"keys": ["notes/a1"]})).await,
    );
    assert!(
        !no_handle_sees_a1.contains("# A1"),
        "a no-handle retrieve must not see alpha's staged a1, got: {no_handle_sees_a1}"
    );
    let no_handle_sees_b1 = retrieved_text(
        &f,
        &f.call_tool("iwe_retrieve", json!({"keys": ["notes/b1"]})).await,
    );
    assert!(
        !no_handle_sees_b1.contains("# B1"),
        "a no-handle retrieve must not see beta's staged b1, got: {no_handle_sees_b1}"
    );

    // Commit alpha first; the other handle (beta) is unaffected.
    let committed = f.call_tool("iwe_tx_commit", json!({"handle": "alpha"})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    let report = Fixture::result_json(&committed);
    assert_eq!(report["status"], "committed");
    let mut alpha_keys: Vec<String> = report["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    alpha_keys.sort();
    assert_eq!(alpha_keys, vec!["notes/a1", "notes/a2"]);

    // Alpha's keys are now on disk and on beta's view remains staged.
    assert_eq!(read_to_string(dir.path().join("notes/a1.md")).unwrap(), a1);
    assert_eq!(read_to_string(dir.path().join("notes/a2.md")).unwrap(), a2);
    assert!(
        !dir.path().join("notes/b1.md").exists(),
        "beta's staged b1 leaked into alpha's commit"
    );
    assert!(
        !dir.path().join("notes/b2.md").exists(),
        "beta's staged b2 leaked into alpha's commit"
    );

    // Beta's handle is still open — its own read still sees its staged
    // state, and its commit now succeeds.
    let beta_still_sees = Fixture::result_json(
        &f.call_tool(
            "iwe_query",
            json!({
                "operation": "find",
                "handle": "beta",
                "document": "filter: { $key: { $in: ['notes/b1', 'notes/b2'] } }\n",
            }),
        )
        .await,
    );
    let beta_still_keys: Vec<String> = beta_still_sees
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["key"].as_str().unwrap().to_string())
        .collect();
    assert!(
        beta_still_keys.contains(&"notes/b1".to_string())
            && beta_still_keys.contains(&"notes/b2".to_string()),
        "beta's own read still sees beta's staged b1+b2 after alpha commits, got: {beta_still_keys:?}"
    );

    let committed = f.call_tool("iwe_tx_commit", json!({"handle": "beta"})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    let report = Fixture::result_json(&committed);
    assert_eq!(report["status"], "committed");
    let mut beta_keys: Vec<String> = report["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    beta_keys.sort();
    assert_eq!(beta_keys, vec!["notes/b1", "notes/b2"]);
    assert_eq!(read_to_string(dir.path().join("notes/b1.md")).unwrap(), b1);
    assert_eq!(read_to_string(dir.path().join("notes/b2.md")).unwrap(), b2);

    // Both commits land as separate journal records, each carrying
    // exactly its own handle's keys (write order within a commit is not
    // pinned, so the key set per record is compared sorted).
    let records = journal_records(&dir);
    assert_eq!(
        records.len(),
        2,
        "two commits, two journal records: {records:?}"
    );
    let effects_of = |record: &Value| -> Vec<String> {
        let mut keys: Vec<String> = record["effects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap().to_string())
            .collect();
        keys.sort();
        keys
    };
    let first = effects_of(&records[0]);
    let second = effects_of(&records[1]);
    assert!(
        (first == vec!["notes/a1", "notes/a2"] && second == vec!["notes/b1", "notes/b2"])
            || (first == vec!["notes/b1", "notes/b2"] && second == vec!["notes/a1", "notes/a2"]),
        "each journal record must carry exactly one handle's keys: {first:?} / {second:?}"
    );
    assert!(
        records[0]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["effect"] == "create"),
        "all staged effects are creates: {records:?}"
    );
    assert!(
        records[1]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["effect"] == "create"),
        "all staged effects are creates: {records:?}"
    );
}

#[tokio::test]
async fn a_key_changed_by_one_open_handle_refuses_the_other_handle_commit() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Two explicit handles open at once.
    f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    f.call_tool("iwe_tx_begin", json!({"handle": "beta"})).await;

    let mine = "---\ntype: note\n---\n# Leaf\n\nMine [Hub](hub).\n";
    let theirs = "---\ntype: note\n---\n# Leaf\n\nTheirs [Hub](hub).\n";
    let beta_own = "---\ntype: note\n---\n# Beta own\n\nSee [Hub](hub).\n";

    // Handle A stages its version of the contested key; handle B stages
    // its own version of the contested key plus a second, conflict-free
    // key. Both handle-stages succeed — the conflict is a commit-time
    // fact, not a staging-time one.
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

    // Handle A commits first — its version of notes/leaf lands.
    let committed = f.call_tool("iwe_tx_commit", json!({"handle": "alpha"})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert_eq!(
        read_to_string(dir.path().join("notes/leaf.md")).unwrap(),
        mine,
        "alpha's commit lands its version of the contested key"
    );

    // Handle B's commit is refused as a whole: its staged notes/leaf is
    // stale against on-disk state (alpha's commit landed first), and the
    // refusal names the conflicting key. The contract is whole-unit:
    // handle B's conflict-free key must NOT land either.
    let message = f
        .try_call_tool("iwe_tx_commit", json!({"handle": "beta"}))
        .await
        .expect_err(
            "beta's commit must be refused: its staged notes/leaf was changed on disk by alpha's commit",
        )
        .to_string();
    assert!(
        message.contains("write conflict") && message.contains("notes/leaf"),
        "the refusal names the stale key, got: {message}"
    );

    // Alpha's version stands — not clobbered by B's failed attempt.
    assert_eq!(
        read_to_string(dir.path().join("notes/leaf.md")).unwrap(),
        mine,
        "alpha's version is not clobbered by beta's refused commit"
    );
    // Beta's non-conflicting key did not land either — a refused commit
    // is a whole-unit refusal.
    assert!(
        !dir.path().join("notes/beta_own.md").exists(),
        "a refused commit lands nothing, not even its conflict-free keys"
    );

    // Only alpha's commit is journaled; beta's refused attempt wrote no
    // record at all.
    let records = journal_records(&dir);
    assert_eq!(
        records.len(),
        1,
        "only alpha's commit is journaled: {records:?}"
    );
    let mut keys: Vec<String> = records[0]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap().to_string())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["notes/leaf"]);
}
