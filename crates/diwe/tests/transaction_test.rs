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
use diwe::fs::{apply_changes, write_store_at_path, TransactionalWriteError};
use liwe::model::{Key, State};
use liwe::operations::Changes;
use liwe::transaction::testing::{Call, CallLog, RecordingTransaction};

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

    let log = CallLog::new();
    let changes = Changes::new().remove(Key::name("a"));

    apply_changes(&changes, dir.path(), Format::Markdown, |_, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    assert!(!dir.path().join("a.md").exists());
    assert_eq!(
        log.calls(),
        vec![
            Call::Begin,
            Call::Write(liwe::transaction::Write::Remove(Key::name("a"))),
            Call::Commit,
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
    let log = CallLog::new();
    let changes = Changes::new().create(Key::name("new-doc"), "# New\n".to_string());

    apply_changes(&changes, dir.path(), Format::Markdown, |_, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    assert_eq!(read(&dir.path().join("new-doc.md")).unwrap(), "# New\n");
    assert_eq!(
        log.calls(),
        vec![
            Call::Begin,
            Call::Write(liwe::transaction::Write::Put(
                Key::name("new-doc"),
                "# New\n".to_string()
            )),
            Call::Commit,
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

    let log = CallLog::new();
    let changes = Changes::new().update(Key::name("a"), "# New\n".to_string());

    apply_changes(&changes, dir.path(), Format::Markdown, |_, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    assert_eq!(read(&dir.path().join("a.md")).unwrap(), "# New\n");
    assert_eq!(
        log.calls(),
        vec![
            Call::Begin,
            Call::Write(liwe::transaction::Write::Put(
                Key::name("a"),
                "# New\n".to_string()
            )),
            Call::Commit,
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

    let log = CallLog::new();
    write_store_at_path(&store, dir.path(), Format::Markdown, |_, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    })
    .unwrap();

    let calls = log.calls();
    assert_eq!(calls.len(), 6, "begin+write+commit per document: {calls:?}");
    // Both documents were committed exactly once each; order between them
    // is not guaranteed (`State` iteration order), so count rather than
    // match position.
    assert_eq!(calls.iter().filter(|c| **c == Call::Begin).count(), 2);
    assert_eq!(calls.iter().filter(|c| **c == Call::Commit).count(), 2);
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
    let log = CallLog::new();
    let changes = Changes::new().create(Key::name("blocked"), "# Blocked\n".to_string());

    let always_rejecting = |_key: &Key, _content: &str| {
        Err(diwe::permissions::WritePermissionError::Placeholder)
    };

    let result = apply_changes(&changes, dir.path(), Format::Markdown, always_rejecting, {
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
        log.calls(),
        vec![
            Call::Begin,
            Call::Write(liwe::transaction::Write::Put(
                Key::name("blocked"),
                "# Blocked\n".to_string()
            )),
            Call::Abort,
        ]
    );

    // The contract itself -- independent of any call site -- is that once
    // a write is rejected for lack of permission, `commit` must refuse
    // (`CommitError::Failed`) and only `abort` remains available. Proven
    // directly against the hand-built always-rejecting stub, matching
    // `RecordingTransaction::always_rejecting`'s own unit test in
    // `liwe::transaction::testing`.
    use liwe::transaction::{CommitError, Transaction, WriteRejected};
    let mut tx = RecordingTransaction::always_rejecting(CallLog::new());
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
// The "write doesn't land" half does **not** hold today, and this test
// documents that rather than asserting something false: `persist` (the
// real `std::fs::write`) runs *before* `tx.commit()` in every WP-02..WP-13
// call site (see `run_transactional_write`'s ordering, which mirrors the
// original hand-written brackets it replaced), and is never gated by
// `tx`'s own outcome. So when a backend's `commit` refuses for a reason
// unrelated to the failed-state rule (`CommitError::Other`, as opposed to
// `CommitError::Failed`, which -- per the `Transaction::write` contract --
// only ever follows a write already rejected and hence never persisted),
// the file has already been written to disk by the time that refusal is
// discovered. This is the central interface-sufficiency finding T6
// surfaces: `Transaction` today runs alongside the real storage effect as
// a parallel, currently-inert bookkeeping trail, not (yet) the mechanism
// that performs or gates it. See the T6 report's interface-sufficiency
// statement for the full argument.
// ---------------------------------------------------------------------
#[test]
fn backend_commit_refusal_surfaces_as_an_error_but_the_write_already_landed() {
    let dir = tempfile::tempdir().unwrap();
    let log = CallLog::new();
    let changes = Changes::new().create(Key::name("doomed"), "# Doomed\n".to_string());

    let result = apply_changes(&changes, dir.path(), Format::Markdown, |_, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::refusing_commit(log.clone())
    });

    assert!(
        result.is_err(),
        "a backend commit refusal must surface as Err, not be swallowed"
    );
    assert!(
        dir.path().join("doomed.md").exists(),
        "documents today's actual (insufficient) behavior: persist runs before \
         commit and is not gated by it, so the write lands on disk even though \
         the transaction backend went on to refuse the commit -- see this \
         test's doc comment"
    );
    assert_eq!(
        log.calls(),
        vec![
            Call::Begin,
            Call::Write(liwe::transaction::Write::Put(
                Key::name("doomed"),
                "# Doomed\n".to_string()
            )),
            Call::Commit,
        ],
        "persist ran (the file write already happened) before commit refused"
    );
}

// ---------------------------------------------------------------------
// Sanity check that the error variant produced by a backend commit
// refusal is in fact the `Commit` variant of `TransactionalWriteError` --
// not, say, silently downgraded to a generic I/O error indistinguishable
// from an unrelated failure. `run_transactional_write` is exercised
// directly here (not through `apply_changes`'s I/O-error conversion) so
// the error's shape, not just its presence, is checked.
// ---------------------------------------------------------------------
#[test]
fn run_transactional_write_reports_commit_refusal_distinctly() {
    let log = CallLog::new();
    let mut tx = RecordingTransaction::refusing_commit(log);

    let result = diwe::fs::run_transactional_write(
        &mut tx,
        liwe::transaction::Write::Put(Key::name("x"), "content".to_string()),
        || Ok(()),
        || Ok(()),
    );

    assert!(matches!(
        result,
        Err(TransactionalWriteError::Commit(liwe::transaction::CommitError::Other(_)))
    ));
}
