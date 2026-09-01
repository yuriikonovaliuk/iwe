// M2 fix-wave: `AffectedSetTransaction`'s concrete type staying dormant as
// the production default is correct (AB9: "transactions default to no-op
// passthrough") — but the *interface* it would plug into must be genuinely
// routed through by production paths, reachable by a future caller (the
// compositor, in a later milestone) without rewriting `iwec`'s call sites.
//
// Before this fix, `IweServer::write_file_with`/`write_changes_with` (the
// generic cores parameterized over the transaction backend) were private —
// reachable only from tests living inside `crates/iwec/src/lib.rs` itself,
// with no way for any external crate to install a different backend at
// all. This test lives in `crates/iwec/tests/` — a separate compilation
// unit that can only see `iwec`'s genuinely `pub` surface — and proves the
// interface boundary is now real: an external caller can construct an
// `IweServer` and drive both write paths with its own transaction factory,
// exactly the way a future compositor would install `AffectedSetTransaction`
// in place of `NoopTransaction` without touching this crate's source.

use diwe::config::Configuration;
use iwec::IweServer;
use liwe::model::Key;
use liwe::operations::Changes;
use liwe::transaction::{RecordingTransaction, TransactionLog};

fn server_over(dir: &std::path::Path) -> IweServer {
    IweServer::new(dir.to_str().unwrap(), &Configuration::default())
}

#[test]
fn write_file_with_is_reachable_and_drivable_from_outside_the_crate() {
    let dir = tempfile::tempdir().unwrap();
    let server = server_over(dir.path());
    let log = TransactionLog::new();

    let result = server.write_file_with(&Key::name("note"), "# Note\n", {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(log.begin_count(), 1);
    assert_eq!(log.commit_count(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
        "# Note\n"
    );
}

#[test]
fn write_changes_with_is_reachable_and_drivable_from_outside_the_crate() {
    let dir = tempfile::tempdir().unwrap();
    let server = server_over(dir.path());
    let changes = Changes::new().create(Key::name("note"), "# Note\n".to_string());
    let log = TransactionLog::new();

    server.write_changes_with(&changes, {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert_eq!(log.begin_count(), 1);
    assert_eq!(log.commit_count(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
        "# Note\n"
    );
}
