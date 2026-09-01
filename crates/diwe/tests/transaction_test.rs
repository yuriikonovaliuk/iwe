//! T6 (independent verification): proves that `diwe::fs::apply_changes`
//! and `diwe::fs::write_store_at_path` — the shared mechanism behind
//! WP-06 (CLI delete), WP-07 (CLI rename), WP-08 (CLI extract), WP-09 (CLI
//! inline), WP-11's whole-graph branch (CLI normalize with no `--key`),
//! and (via `write_changes`) half of WP-12 (MCP delete/rename/extract/
//! inline) — actually drive `begin` and `commit` on the injected
//! `Transaction` backend, not just that a write lands on disk.
//!
//! These tests are written against the acceptance criteria for T6 only,
//! without reading how a Developer working the same milestone in parallel
//! implemented WP-02..WP-13 (see `roles/delivery/test-builder`):
//! independence is the point. `diwe::permissions::check_write_permission_
//! for_content` is a placeholder today (always allows), so the
//! "mid-transaction rejection" test below constructs its own hand-built
//! always-rejecting `check` closure rather than relying on real
//! enforcement -- re-run that scenario against the real check once
//! T10/T11 land.

use diwe::config::Format;
use diwe::fs::{apply_changes_with, write_store_at_path_with};
use liwe::model::{Key, State};
use liwe::operations::Changes;
use liwe::transaction::{RecordingTransaction, TransactionLog, TxEvent};

fn read(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

// ---------------------------------------------------------------------
// WP-06 (CLI delete) / WP-12 (MCP delete, via `write_changes`): removal.
// ---------------------------------------------------------------------
#[test]
fn apply_changes_remove_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n").unwrap();

    let log = TransactionLog::new();
    let changes = Changes::new().remove(Key::name("a"));

    apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    assert!(!dir.path().join("a.md").exists());
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(liwe::transaction::Write::Remove(Key::name("a"))),
            TxEvent::Commit,
        ]
    );
}

// ---------------------------------------------------------------------
// WP-08/WP-09 (CLI extract/inline) style: a create alongside the rest of
// `Changes`. WP-07 (rename) and WP-08/09 (extract/inline) all funnel
// through this same `creates` bucket for any document they add, so one
// test against it stands in for all three at the `apply_changes` level
// (see main.rs's `apply_changes` wrapper, which adds nothing but path
// resolution on top of this call).
// ---------------------------------------------------------------------
#[test]
fn apply_changes_create_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    let log = TransactionLog::new();
    let changes = Changes::new().create(Key::name("new-doc"), "# New\n".to_string());

    apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    assert_eq!(read(&dir.path().join("new-doc.md")).unwrap(), "# New\n");
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(liwe::transaction::Write::Put(
                Key::name("new-doc"),
                "# New\n".to_string()
            )),
            TxEvent::Commit,
        ]
    );
}

// ---------------------------------------------------------------------
// WP-07 (CLI rename) style: an update to an existing document.
// ---------------------------------------------------------------------
#[test]
fn apply_changes_update_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# Old\n").unwrap();

    let log = TransactionLog::new();
    let changes = Changes::new().update(Key::name("a"), "# New\n".to_string());

    apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    assert_eq!(read(&dir.path().join("a.md")).unwrap(), "# New\n");
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(liwe::transaction::Write::Put(
                Key::name("a"),
                "# New\n".to_string()
            )),
            TxEvent::Commit,
        ]
    );
}

// ---------------------------------------------------------------------
// WP-11 (CLI normalize, empty-key/whole-graph branch): one implicit
// transaction per rewritten document.
// ---------------------------------------------------------------------
#[test]
fn write_store_at_path_begins_and_commits_once_per_document() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = State::new();
    store.insert("a".to_string(), "# A\n".to_string());
    store.insert("b".to_string(), "# B\n".to_string());

    let log = TransactionLog::new();
    write_store_at_path_with(&store, dir.path(), Format::Markdown, |_, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    let calls = log.events();
    assert_eq!(calls.len(), 6, "begin+write+commit per document: {calls:?}");
    // Both documents were committed exactly once each; order between them
    // is not guaranteed (`State` iteration order), so count rather than
    // match position.
    assert_eq!(calls.iter().filter(|c| **c == TxEvent::Begin).count(), 2);
    assert_eq!(calls.iter().filter(|c| **c == TxEvent::Commit).count(), 2);
    assert_eq!(read(&dir.path().join("a.md")).unwrap(), "# A\n");
    assert_eq!(read(&dir.path().join("b.md")).unwrap(), "# B\n");
}

// ---------------------------------------------------------------------
// "A test where a write-permission rejection occurs mid-transaction:
// commit refuses, abort succeeds, no partial state persists" — using a
// hand-built always-rejecting `check` hook, since T10/T11 haven't landed
// (the real `check_write_permission_for_content` always allows today).
// ---------------------------------------------------------------------
#[test]
fn always_rejecting_check_hook_aborts_and_leaves_no_partial_state() {
    let dir = tempfile::tempdir().unwrap();
    let log = TransactionLog::new();
    let changes = Changes::new().create(Key::name("blocked"), "# Blocked\n".to_string());

    let always_rejecting = |key: &Key, _content: &str, _prior: Option<&str>| {
        Err(diwe::permissions::WritePermissionError::Frozen { key: key.clone() })
    };

    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, always_rejecting, {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_err(), "the rejection must surface, not be swallowed");
    assert!(
        !dir.path().join("blocked.md").exists(),
        "no partial state may persist once the write-permission check rejects"
    );
    // `apply_changes` aborts (not commits) once `check` rejects, without
    // ever calling `commit` for this write -- the transaction never
    // reaches a state where a partial write could be made durable.
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(liwe::transaction::Write::Put(
                Key::name("blocked"),
                "# Blocked\n".to_string()
            )),
            TxEvent::Abort,
        ]
    );

    // The contract itself -- independent of any call site -- is that once
    // a write is rejected for lack of permission, `commit` must refuse
    // (`CommitError::Failed`) and only `abort` remains available. Proven
    // directly against the hand-built always-rejecting stub, matching
    // `RecordingTransaction::rejecting_next_write`'s own unit tests in
    // `liwe::transaction`. (Adapted at merge time: the real constructor
    // rejects the next write, which for this single-write scenario is
    // the always-rejecting stub's exact behaviour.)
    use liwe::transaction::{CommitError, Transaction, WriteRejected};
    let mut tx = RecordingTransaction::rejecting_next_write(TransactionLog::new());
    tx.begin().unwrap();
    let rejected = tx.write(liwe::transaction::Write::Remove(Key::name("secret")));
    assert!(matches!(rejected, Err(WriteRejected::PermissionDenied)));
    assert!(matches!(tx.commit(), Err(CommitError::Failed)));
    assert!(tx.abort().is_ok());
}

// ---------------------------------------------------------------------
// "A test where the stub refuses a commit: write doesn't land, refusal
// surfaces as an error, not swallowed."
//
// The refusal half of this holds cleanly: `apply_changes` returns `Err`,
// not `Ok`, and the error is `TransactionalWriteError::Commit` (see
// `run_transactional_write_reports_commit_refusal_distinctly` below) --
// nothing discards it with `let _ = ...` the way the pre-T6 call sites'
// `.expect("no-op transaction backend never fails")` would have panicked
// past it instead of returning a clean `Err`.
//
// The "write doesn't land" half holds in the merged implementation:
// `apply_changes_with` attempts `tx.commit()` *before* the real
// filesystem operation, so a commit refusal prevents the write from
// landing (T6's original interface-sufficiency finding -- persist ran
// ungated before commit -- was resolved by the t21 fix wave; this test
// was adapted at merge time from documenting the insufficiency to
// asserting the fixed contract).
// ---------------------------------------------------------------------
#[test]
fn backend_commit_refusal_surfaces_as_an_error_and_the_write_does_not_land() {
    let dir = tempfile::tempdir().unwrap();
    let log = TransactionLog::new();
    let changes = Changes::new().create(Key::name("doomed"), "# Doomed\n".to_string());

    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::refusing_commit(log.clone())
    });

    assert!(
        result.is_err(),
        "a backend commit refusal must surface as Err, not be swallowed"
    );
    assert!(
        !dir.path().join("doomed.md").exists(),
        "a refused commit must prevent the write from landing (commit is \
         attempted before the filesystem operation and gates it)"
    );
    assert_eq!(
        &log.events()[..3],
        &[
            TxEvent::Begin,
            TxEvent::Write(liwe::transaction::Write::Put(
                Key::name("doomed"),
                "# Doomed\n".to_string()
            )),
            TxEvent::Commit,
        ],
        "begin and write were driven, and commit was attempted (and refused) \
         before any persist"
    );
}

// ---------------------------------------------------------------------
// Sanity check that a backend commit refusal surfaces distinctly -- not
// silently downgraded to an error indistinguishable from an unrelated
// failure. The merged implementation reports it as an I/O error whose
// message names the transaction backend (`transaction_backend_failed`),
// distinct from the permission-denied shape. (Adapted at merge time: the
// Test-builder's stub `run_transactional_write`/`TransactionalWriteError`
// surface never shipped; the assertion intent is preserved against the
// real reporting path.)
// ---------------------------------------------------------------------
#[test]
fn backend_commit_refusal_is_distinguishable_from_permission_denial() {
    let dir = tempfile::tempdir().unwrap();
    let log = TransactionLog::new();
    let changes = Changes::new().create(Key::name("x"), "content".to_string());

    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::refusing_commit(log.clone())
    });

    let err = result.expect_err("commit refusal must surface");
    assert!(
        err.to_string().contains("transaction backend"),
        "refusal names the transaction backend, distinguishing it from a \
         write-permission denial; got: {err}"
    );
}
