// M2 fix-wave: `AffectedSetTransaction`'s concrete type staying dormant as
// the production default is correct (AB9: "transactions default to no-op
// passthrough") — but the *interface* it would plug into must be genuinely
// routed through by production paths, reachable by a future caller (the
// compositor, in a later milestone) without rewriting these call sites.
//
// `iwe::new::write_document_with` was already `pub fn` (unlike `iwec`'s
// equivalents, which the same fix-wave found private and fixed) — this
// test lives outside the `iwe` crate's binary, in a separate compilation
// unit that can only see `iwe`'s genuinely `pub` library surface, and
// proves that wiring point is real: an external caller can drive the
// CLI's create/new write path with its own transaction factory, the same
// composition the CLI itself drives with `NoopTransaction::new` as the
// default.

use diwe::config::Configuration;
use iwe::new::{write_document_with, PreparedDocument};
use liwe::model::Key;
use liwe::transaction::{RecordingTransaction, TransactionLog};

#[test]
fn write_document_with_is_reachable_and_drivable_from_outside_the_crate() {
    let dir = tempfile::tempdir().unwrap();
    let config = Configuration::default();
    let prepared = PreparedDocument {
        key: Key::name("note"),
        path: dir.path().join("note.md"),
        content: "# Note\n".to_string(),
    };
    let log = TransactionLog::new();

    let result = write_document_with(&config, &prepared, {
        let log = log.clone();
        move || RecordingTransaction::new(log.clone())
    });

    assert!(result.is_ok(), "{:?}", result.err());
    assert_eq!(log.begin_count(), 1);
    assert_eq!(log.commit_count(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
        "# Note\n"
    );
}
