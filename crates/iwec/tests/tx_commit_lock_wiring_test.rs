//! Behavioral test suite for wiring `iwec`'s `iwe_tx_commit` onto the
//! store-wide commit lock, built directly from the task contract's
//! acceptance criteria and shared surface
//! (efforts/knowledge-compositor/m6-b-cutover-preconditions/5-iwe-t4-iwec-tx-commit-lock-wiring/contract).
//! Written without reading Developer's implementation of this task's
//! wiring inside `crates/iwec` — every identifier used below comes from
//! the contract's pinned shared surface (`liwe::write_lock`, already
//! landed and committed by a prior task, 95162d1) or from this crate's
//! own pre-existing, already-committed test fixture and MCP-tool surface
//! (`agent_transaction_test.rs`'s style and helpers, mirrored here).
//!
//! ENGINEERING NOTE — how the two required races are driven (delivery
//! requirement 2 for the contract's test (b), and the analogous concern
//! for test (a)):
//!
//!   - Test (a) (`concurrent_lock_holder...`): a background OS thread
//!     acquires the commit lock directly via `liwe::write_lock::
//!     acquire_commit_lock`, the exact same pinned entry point
//!     `iwe_tx_commit` itself must call, aimed at the same repo root. No
//!     config injection is exposed on that function (by design — its
//!     signature is pinned as `acquire_commit_lock(repo_root: &Path)`,
//!     no `LockConfig`), so the real wall-clock duration of the timeout
//!     this test observes is whatever `acquire_commit_lock`'s internal
//!     `acquire_timeout` happens to default to. The test does not assert
//!     a specific duration — only a generous hang-guard (see
//!     `crates/liwe/tests/write_lock_test.rs`'s own
//!     `second_concurrent_acquire_times_out_and_never_hangs` for the same
//!     pattern) — and does not assert the refusal's exact message text,
//!     since the contract pins only `CommitLockError`'s own `Display`
//!     text, not how `iwe_tx_commit` wraps it into its MCP-level error.
//!
//!   - Test (b) (`lock_reclaimed_between_acquire_and_apply...`): forces
//!     the transaction's *own*, already-acquired guard to go stale before
//!     it applies, by racing a second, independent `iwe_lock::FileLock`
//!     directly at the same on-disk path
//!     (`repo_root.join(liwe::write_lock::DEFAULT_LOCK_PATH)`) configured
//!     with a deliberately tiny `stale_after` (1ms). `iwe_lock`'s
//!     `is_free` staleness check is evaluated using the *checking* side's
//!     own `stale_after`, not the holder's — so once any real time has
//!     passed since the transaction's own guard last wrote its heartbeat
//!     (which is true from the moment it's acquired), this second lock's
//!     `.acquire()` call reclaims it immediately, on its first read,
//!     regardless of what heartbeat/stale timing `iwe_tx_commit`'s own
//!     internal call to `acquire_commit_lock` uses. This is the same
//!     mechanism `crates/liwe/tests/write_lock_test.rs`'s
//!     `reclaimed_lock_reports_stale_via_check_fencing` test already uses
//!     against the module in isolation; here it is driven concurrently,
//!     against the real `iwe_tx_commit` tool call, to land the reclaim
//!     specifically inside the pre-apply window rather than before or
//!     after it. Two knobs make that window landing reliable without a
//!     test-only hook into `iwec`'s implementation:
//!       1. A short, fixed head start (50ms) before the racer's first
//!          reclaim attempt: `acquire_commit_lock`'s own start-of-commit
//!          acquire is uncontended here, so it succeeds in well under
//!          that time, comfortably before the racer's first attempt.
//!       2. A large staged batch (150 documents, `BULK_COUNT`) inflates
//!          the real duration of whatever full-scope validation and
//!          conflict-detection work `iwe_tx_commit` does between
//!          acquiring the lock and reaching its pre-apply fencing check,
//!          so that window is comfortably wider than the 50ms head
//!          start plus the racer's own near-instant reclaim.
//!     This remains a genuine race rather than a synchronous, guaranteed
//!     sequencing (no hook into `iwec`'s internals is available or
//!     assumed), but is engineered, via (1) and (2), to land the
//!     intended way with very high reliability. See the delivery report
//!     for this task for the same caveat spelled out explicitly.
//!
//! VERIFICATION STATUS: run once against the pre-wiring `iwec` (before
//! this task's implementation lands) purely to confirm the file compiles,
//! is collectible, and does not hang — not to check correctness, since
//! `iwe_tx_commit` does not yet touch the commit lock at all pre-wiring,
//! so every assertion below that depends on lock contention is *expected*
//! to fail at that point (calls that should be refused instead succeed).
//! Not run against Developer's landed implementation of this task, and
//! Developer's implementation was not read to write this file.

use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use diwe::config::{
    Configuration, JournalOptions, Patterns, SchemaBinding, TransactionOptions, ValidationScope,
};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::fixture::Fixture;

/// How many documents to stage in test (b) purely to widen the real
/// wall-clock window `iwe_tx_commit` spends between acquiring the commit
/// lock and reaching its pre-apply fencing check (see module doc).
const BULK_COUNT: usize = 150;

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

// ---------------------------------------------------------------------------
// Acceptance criterion: "iwe_tx_commit acquires acquire_commit_lock at
// commit-attempt start (inside iwe_tx_commit, never at earlier
// iwe_tx_begin); open-but-uncommitted transaction never holds this lock."
// ---------------------------------------------------------------------------

/// An open, staged-into transaction that never calls `iwe_tx_commit` must
/// not be holding the commit lock — proven by a second, independent
/// `acquire_commit_lock` call (standing in for a wholly separate caller,
/// e.g. the `iwe` CLI or another agent) succeeding against the same repo
/// root while the transaction sits open.
#[tokio::test]
async fn open_uncommitted_transaction_never_holds_the_commit_lock() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;
    let repo_root = dir.path().canonicalize().unwrap();

    let begun = f.call_tool("iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({"key": "notes/new", "content": "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n"}),
    )
    .await;

    // If iwe_tx_begin (or the staging writes since) had acquired the
    // commit lock and never released it, this independent acquire would
    // have to wait out the full acquire_timeout and then fail.
    let guard = liwe::write_lock::acquire_commit_lock(&repo_root).expect(
        "the commit lock must be free while a transaction is open but not yet committing — \
         acquiring it must be iwe_tx_commit's job alone",
    );
    drop(guard);
}

// ---------------------------------------------------------------------------
// Required test (a): concurrent lock holder causes iwe_tx_commit to time
// out and return refusal without applying.
// ---------------------------------------------------------------------------

/// Required test case (a): a concurrent external holder of the commit
/// lock (standing in for another agent's or the CLI's in-flight commit)
/// causes `iwe_tx_commit` to time out and refuse, applying nothing —
/// and the transaction remains open for a subsequent retry once the lock
/// frees up.
#[tokio::test]
async fn concurrent_external_lock_holder_times_out_commit_and_applies_nothing() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;
    let repo_root = dir.path().canonicalize().unwrap();

    // Stage the write first, fully uncontended — the acceptance criterion
    // under test is about iwe_tx_commit's own lock acquisition, not about
    // whatever (separate, pre-existing) validation a staging write goes
    // through inside an open transaction.
    f.call_tool("iwe_tx_begin", json!({})).await;
    let content = "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n";
    let created = f
        .call_tool("iwe_create", json!({"key": "notes/new", "content": content}))
        .await;
    assert!(!created.is_error.unwrap_or(false), "{created:?}");

    // External holder: takes the store-wide commit lock via the exact
    // same public entry point iwe_tx_commit itself must call, and holds
    // it until told to release. Guarded by a bounded `recv_timeout` (not
    // an unconditional `recv`) so that if the test panics below before
    // sending the release signal, this thread still exits on its own
    // instead of leaking a held lock for the rest of the test process.
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder_root = repo_root.clone();
    let holder = thread::spawn(move || {
        let guard = liwe::write_lock::acquire_commit_lock(&holder_root)
            .expect("external holder must acquire the free commit lock");
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
        drop(guard);
    });
    ready_rx.recv().unwrap();

    let start = Instant::now();
    let commit_result = f.try_call_tool("iwe_tx_commit", json!({})).await;
    let elapsed = start.elapsed();

    release_tx.send(()).unwrap();
    holder.join().unwrap();

    assert!(
        commit_result.is_err(),
        "a commit attempted while the commit lock is held elsewhere must be refused, not \
         hang or silently succeed: {commit_result:?}"
    );
    // Hang-guard, not a timing assertion: acquire_commit_lock's internal
    // acquire_timeout is not part of the pinned shared surface, so no
    // specific duration is asserted here — only that it is bounded.
    assert!(
        elapsed < Duration::from_secs(60),
        "commit refusal on an already-held lock took too long: {elapsed:?} — looks like a hang"
    );

    // Nothing applied.
    assert!(!dir.path().join("notes/new.md").exists());
    assert!(journal_records(&dir).is_empty(), "nothing is journaled on a timeout refusal");

    // Still open, not discarded.
    let message = f
        .try_call_tool("iwe_tx_begin", json!({}))
        .await
        .expect_err("a lock-timeout refusal must leave the transaction open, not discard it")
        .to_string();
    assert!(message.contains("already open"), "{message}");

    // Now that the lock is free, retrying the commit succeeds.
    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(dir.path().join("notes/new.md").exists());
    assert_eq!(journal_records(&dir).len(), 1);
}

// ---------------------------------------------------------------------------
// Required test (b): a lock reclaimed between iwe_tx_commit's acquire and
// its apply step is caught by fencing — staged writes not applied,
// transaction still open, and a subsequent retry succeeds once the lock
// is free.
// ---------------------------------------------------------------------------

/// Required test case (b). See the module doc's ENGINEERING NOTE for
/// exactly how the reclaim is landed inside the pre-apply window.
#[tokio::test]
async fn lock_reclaimed_between_acquire_and_apply_is_caught_by_fencing_transaction_stays_open_for_retry(
) {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;
    let repo_root = dir.path().canonicalize().unwrap();

    f.call_tool("iwe_tx_begin", json!({})).await;
    for i in 0..BULK_COUNT {
        let key = format!("notes/bulk{i}");
        let content = format!("---\ntype: note\n---\n# Bulk {i}\n\nSee [Hub](hub).\n");
        let created = f
            .call_tool("iwe_create", json!({"key": key, "content": content}))
            .await;
        assert!(!created.is_error.unwrap_or(false), "{created:?}");
    }

    let racer_root = repo_root.clone();
    let racer = tokio::task::spawn_blocking(move || {
        // Head start for iwe_tx_commit's own, uncontended start-of-attempt
        // acquire (see module doc, knob 1).
        thread::sleep(Duration::from_millis(50));

        let reclaimer = iwe_lock::FileLock::new(iwe_lock::LockConfig {
            path: racer_root.join(liwe::write_lock::DEFAULT_LOCK_PATH),
            heartbeat_interval: Duration::from_millis(5),
            stale_after: Duration::from_millis(1),
            acquire_timeout: Duration::from_secs(5),
        });
        let guard = reclaimer.acquire().expect(
            "reclaimer must take over the transaction's own held commit lock: iwe_lock's \
             staleness check uses the checking side's own (here, 1ms) stale_after, so any \
             hold at least 1ms old is reclaimable regardless of the holder's own \
             heartbeat/stale configuration",
        );
        // Hold the takeover briefly so the in-flight commit's fencing
        // check observes the supersession, then release so the lock is
        // free again for the retry below.
        thread::sleep(Duration::from_millis(150));
        drop(guard);
    });

    let commit_result = f.try_call_tool("iwe_tx_commit", json!({})).await;
    racer.await.expect("racer thread must not panic");

    assert!(
        commit_result.is_err(),
        "a commit whose lock was reclaimed mid-attempt must be refused by fencing, not \
         silently succeed: {commit_result:?}"
    );

    // Nothing applied: fencing caught the reclaim before apply/journal-append.
    for i in 0..BULK_COUNT {
        assert!(
            !dir.path().join(format!("notes/bulk{i}.md")).exists(),
            "notes/bulk{i} must not have landed once fencing caught the reclaim"
        );
    }
    assert!(journal_records(&dir).is_empty(), "nothing is journaled on a fencing refusal");

    // Still open — not discarded: matching the existing "staged key
    // changed" refusal pattern (agent_transaction_test.rs), a fencing
    // failure aborts only the commit attempt, not the transaction.
    let message = f
        .try_call_tool("iwe_tx_begin", json!({}))
        .await
        .expect_err("a fencing failure must leave the transaction open for retry, not discard it")
        .to_string();
    assert!(message.contains("already open"), "{message}");

    // Retry, now that the lock is free again, succeeds and lands
    // everything as one commit.
    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    for i in 0..BULK_COUNT {
        assert!(dir.path().join(format!("notes/bulk{i}.md")).exists());
    }
    assert_eq!(journal_records(&dir).len(), 1, "one commit, one journal record, on the retry");
}

// ---------------------------------------------------------------------------
// Acceptance criterion: "guard released on all iwe_tx_commit exits:
// success, refusal, fencing failure." (success + refusal legs are also
// exercised by the tests above via their successful retries, which could
// not acquire the lock at all if a prior exit had leaked the guard.)
// ---------------------------------------------------------------------------

/// After a *successful* commit, the guard iwe_tx_commit held for that
/// attempt must have been released — proven by an independent
/// `acquire_commit_lock` succeeding immediately afterward.
#[tokio::test]
async fn guard_is_released_after_a_successful_commit() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;
    let repo_root = dir.path().canonicalize().unwrap();

    f.call_tool("iwe_tx_begin", json!({})).await;
    f.call_tool(
        "iwe_create",
        json!({"key": "notes/new", "content": "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n"}),
    )
    .await;
    let committed = f.call_tool("iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");

    let guard = liwe::write_lock::acquire_commit_lock(&repo_root)
        .expect("the commit lock must be free immediately after a successful commit");
    drop(guard);
}
