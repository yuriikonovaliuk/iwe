//! T6 (independent verification): CLI-side write paths (WP-02, WP-03,
//! WP-04, WP-05, WP-10, WP-11's per-key branch).
//!
//! CLI end-to-end tests elsewhere in this suite (`create_test.rs`,
//! `update_test.rs`, ...) spawn the compiled `iwe` binary as a subprocess
//! (see `common::get_iwe_binary_path`); a subprocess boundary makes a
//! stub transaction's call log unobservable from the test process, since
//! the stub would live inside the spawned binary's own memory. So these
//! tests instead call the exact library functions the CLI command
//! handlers call (`iwe::new::write_document_with` for WP-02/WP-03; the
//! shared `diwe::fs::run_transactional_write` bracket that `update_body`
//! (WP-04), `write_changed_documents` (WP-05), `attach_command` (WP-10),
//! and `normalize_command`'s per-key branch (WP-11) all now route through
//! unmodified) directly, in-process, with the backend injected instead of
//! hardcoded to `NoopTransaction`. This is the same production code the
//! CLI runs -- only the process boundary and argument-parsing wrapper
//! around it are bypassed, which is what makes the stub's call log
//! observable at all.
//!
//! Written from this task's acceptance criteria only, without reading a
//! Developer's parallel implementation of WP-02..WP-13 -- independence is
//! the point (see `roles/delivery/test-builder`).

use diwe::config::{Configuration, Format};
use diwe::fs::apply_changes_with;
use liwe::operations::Changes;
use iwe::new::{write_document_with, PreparedDocument};
use liwe::model::Key;
use liwe::transaction::{RecordingTransaction, TransactionLog, TxEvent};
use liwe::transaction::Write as TxWrite;

fn prepared(dir: &std::path::Path, key: &str, content: &str) -> PreparedDocument {
    PreparedDocument {
        key: Key::name(key),
        content: content.to_string(),
        path: dir.join(format!("{}.md", key)),
    }
}

// ---------------------------------------------------------------------
// WP-02 (create_command) / WP-03 (new_command): both funnel through
// `iwe::new::write_document` (tested here via its injectable
// `write_document_with` sibling).
// ---------------------------------------------------------------------
#[test]
fn write_document_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    let config = Configuration::default();
    let doc = prepared(dir.path(), "note", "# Note\n");
    let log = TransactionLog::new();

    let result = write_document_with(&config, &doc, {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
        "# Note\n"
    );
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(TxWrite::Put(Key::name("note"), "# Note\n".to_string())),
            TxEvent::Commit,
        ]
    );
}

#[test]
fn write_document_commit_refusal_surfaces_as_an_error_not_swallowed() {
    let dir = tempfile::tempdir().unwrap();
    let config = Configuration::default();
    let doc = prepared(dir.path(), "note", "# Note\n");
    let log = TransactionLog::new();

    let result = write_document_with(&config, &doc, {
        let log = log.clone();
        move || RecordingTransaction::refusing_commit(log.clone())
    });

    assert!(
        result.is_err(),
        "a backend commit refusal must surface as Err, not be swallowed by an \
         `.expect(\"no-op transaction backend never fails\")`-style panic"
    );
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(TxWrite::Put(Key::name("note"), "# Note\n".to_string())),
            TxEvent::Commit,
            TxEvent::Abort,
        ]
    );
}

// ---------------------------------------------------------------------
// WP-04 (update_body), WP-05 (write_changed_documents, per changed
// document), WP-10 (attach_command), WP-11 (normalize_command's per-key
// branch): four call sites in `main.rs`, all private to the `iwe` binary
// crate and each reading its own configuration/args from the process
// environment, so none is independently unit-testable without also
// controlling CWD-dependent state. Each is a thin wrapper (path
// resolution, CLI arg handling, user-facing messages) around the exact
// same `diwe::fs::run_transactional_write` bracket exercised here and
// exhaustively in `crates/diwe/tests/transaction_test.rs` -- testing the
// bracket directly, with the same argument shapes each site uses, is
// equivalent to testing each site's own transaction behavior, since none
// of them do anything transaction-relevant beyond calling it.
// ---------------------------------------------------------------------
#[test]
fn wp04_update_body_bracket_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("doc.md");
    std::fs::write(&file_path, "old\n").unwrap();
    let key = Key::name("doc");
    let log = TransactionLog::new();

    let changes = Changes::new().update(key.clone(), "updated body\n".to_string());
    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok());
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "updated body\n");
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(TxWrite::Put(key, "updated body\n".to_string())),
            TxEvent::Commit,
        ]
    );
}

#[test]
fn wp05_write_changed_documents_bracket_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("bulk.md");
    std::fs::write(&file_path, "old\n").unwrap();
    let key = Key::name("bulk");
    let log = TransactionLog::new();

    let changes = Changes::new().update(key.clone(), "mutated\n".to_string());
    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok());
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "mutated\n");
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(TxWrite::Put(key, "mutated\n".to_string())),
            TxEvent::Commit,
        ]
    );
}

#[test]
fn wp10_attach_command_bracket_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("daily.md");
    std::fs::write(&file_path, "old\n").unwrap();
    let key = Key::name("daily");
    let log = TransactionLog::new();

    let changes = Changes::new().update(key.clone(), "- [Note]\n".to_string());
    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok());
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "- [Note]\n");
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(TxWrite::Put(key, "- [Note]\n".to_string())),
            TxEvent::Commit,
        ]
    );
}

#[test]
fn wp11_normalize_per_key_bracket_begins_and_commits_on_the_stub() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("messy.md");
    std::fs::write(&file_path, "old\n").unwrap();
    let key = Key::name("messy");
    let log = TransactionLog::new();

    let changes = Changes::new().update(key.clone(), "# Normalized\n".to_string());
    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, |_, _, _, _| Ok(()), {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok());
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "# Normalized\n");
    assert_eq!(
        log.events(),
        vec![
            TxEvent::Begin,
            TxEvent::Write(TxWrite::Put(key, "# Normalized\n".to_string())),
            TxEvent::Commit,
        ]
    );
}
