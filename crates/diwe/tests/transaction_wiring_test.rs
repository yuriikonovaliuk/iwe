// M2 fix-wave: `AffectedSetTransaction`'s concrete type staying dormant as
// the production default is correct (AB9: "transactions default to no-op
// passthrough") — but the *interface* it would plug into must be genuinely
// routed through by production paths, reachable by a future caller (the
// compositor, in a later milestone) without rewriting these call sites.
//
// `diwe::fs::apply_changes_with` / `diwe::fs::write_store_at_path_with`
// were already `pub fn` (unlike `iwec`'s equivalents, which the same
// fix-wave found private and fixed) — this test lives outside the `diwe`
// crate, in a separate compilation unit that can only see `diwe`'s
// genuinely `pub` surface, and proves that wiring point is real: an
// external caller can drive both write paths with its own transaction
// factory, the same composition the CLI (`iwe::main`) already drives with
// `NoopTransaction::new` as the default.

use diwe::config::Format;
use diwe::fs::{apply_changes_with, write_store_at_path_with};
use liwe::model::{Key, State};
use liwe::operations::Changes;
use liwe::transaction::{RecordingTransaction, TransactionLog};

fn allow(
    _key: &Key,
    _content: &str,
    _prior_content: Option<&str>,
) -> Result<(), diwe::permissions::WritePermissionError> {
    Ok(())
}

/// Same as [`allow`], but shaped for `apply_changes_with`'s `check` closure
/// (M4/R1: it additionally takes a `diwe::permissions::WriteOperation`).
fn allow4(
    _key: &Key,
    _content: &str,
    _prior_content: Option<&str>,
    _operation: diwe::permissions::WriteOperation,
) -> Result<(), diwe::permissions::WritePermissionError> {
    Ok(())
}

#[test]
fn apply_changes_with_is_reachable_and_drivable_from_outside_the_crate() {
    let dir = tempfile::tempdir().unwrap();
    let changes = Changes::new().create(Key::name("note"), "# Note\n".to_string());
    let log = TransactionLog::new();

    let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
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
fn write_store_at_path_with_is_reachable_and_drivable_from_outside_the_crate() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = State::new();
    store.insert("note".to_string(), "# Note\n".to_string());
    let log = TransactionLog::new();

    let result = write_store_at_path_with(&store, dir.path(), Format::Markdown, allow, {
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
