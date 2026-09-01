//! T4 — Storage-agnostic transaction interface: INDEPENDENT test design.
//!
//! IMPORTANT: everything in this file — the `TransactionalBackend` trait,
//! the `MockBackend` implementation, and the error/state types — is a
//! Test-builder-authored STUB, written independently from the design
//! description and acceptance criteria only, with no visibility into the
//! Developer's actual trait for this task. It is NOT the authoritative
//! interface. Its only purpose is to give these tests something real to
//! compile and run against, so an aggregation step can later reconcile
//! them against whatever the Developer actually built (matching method
//! names/shapes as needed) without this file being mistaken for the
//! source of truth.
//!
//! Design basis (verbatim from `m2/design-transactions`): "Begin, writes,
//! commit, abort — storage-agnostic... A write-permission rejection inside
//! a transaction puts the transaction into a failed state from which only
//! abort is permitted and commit must refuse."
//!
//! Storage-agnosticism note: the trait below is generic over `Key`/`Value`
//! and carries no filesystem, git, or path types in any signature. The
//! `MockBackend` demonstrates a trivial in-memory implementation using
//! plain `String` keys/values to show the trait imposes no storage
//! assumptions.

use std::collections::{HashMap, HashSet};

/// Independently-authored stub of the storage-agnostic transaction trait.
/// Not the authoritative interface — see module doc comment.
pub trait TransactionalBackend {
    type Key: Clone + Eq + std::hash::Hash;
    type Value: Clone;
    type TxHandle: Copy + Eq + std::hash::Hash;
    type Error;

    /// Starts a new transaction and returns a handle to it.
    fn begin(&mut self) -> Result<Self::TxHandle, Self::Error>;

    /// Records a write within the given transaction. A backend may reject
    /// the write (e.g. a permission-style rejection); on rejection the
    /// transaction moves to a failed state.
    fn write(
        &mut self,
        tx: Self::TxHandle,
        key: Self::Key,
        value: Self::Value,
    ) -> Result<(), Self::Error>;

    /// Commits exactly the writes recorded against this transaction handle
    /// and no others. Must refuse (return `Err`) if the transaction is in
    /// a failed state.
    fn commit(&mut self, tx: Self::TxHandle) -> Result<(), Self::Error>;

    /// Aborts the transaction, discarding its writes. This is the only
    /// operation permitted once a transaction has entered a failed state.
    fn abort(&mut self, tx: Self::TxHandle) -> Result<(), Self::Error>;
}

/// Errors produced by [`MockBackend`]. Independently-authored stub, not
/// the authoritative error type.
#[derive(Debug, PartialEq, Eq)]
pub enum MockTxError {
    /// A write was rejected because the key is on the mock's deny-list —
    /// stands in for a "write-permission-style rejection".
    PermissionDenied,
    /// An operation other than abort was attempted against a transaction
    /// already in the failed state.
    TransactionFailed,
    /// An operation was attempted against a transaction that has already
    /// been committed or aborted.
    AlreadyClosed,
    /// The transaction handle is not known to this backend.
    UnknownTransaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(u64);

enum TxState {
    Active(Vec<(String, String)>),
    Failed,
    Committed,
    Aborted,
}

/// Trivial in-memory mock backend used only to prove the trait above is
/// implementable without any storage-specific assumptions.
pub struct MockBackend {
    next_id: u64,
    denied_keys: HashSet<String>,
    transactions: HashMap<TxId, TxState>,
    committed: HashMap<String, String>,
}

impl MockBackend {
    pub fn new() -> Self {
        MockBackend {
            next_id: 0,
            denied_keys: HashSet::new(),
            transactions: HashMap::new(),
            committed: HashMap::new(),
        }
    }

    pub fn with_denied_keys(denied: &[&str]) -> Self {
        let mut backend = MockBackend::new();
        backend.denied_keys = denied.iter().map(|s| s.to_string()).collect();
        backend
    }

    /// Test-only inspection helper (not part of the trait): the value
    /// visible in the backend's committed store, if any.
    pub fn committed_value(&self, key: &str) -> Option<&String> {
        self.committed.get(key)
    }
}

impl TransactionalBackend for MockBackend {
    type Key = String;
    type Value = String;
    type TxHandle = TxId;
    type Error = MockTxError;

    fn begin(&mut self) -> Result<Self::TxHandle, Self::Error> {
        let id = TxId(self.next_id);
        self.next_id += 1;
        self.transactions.insert(id, TxState::Active(Vec::new()));
        Ok(id)
    }

    fn write(
        &mut self,
        tx: Self::TxHandle,
        key: Self::Key,
        value: Self::Value,
    ) -> Result<(), Self::Error> {
        let is_active = matches!(self.transactions.get(&tx), Some(TxState::Active(_)));
        if !is_active {
            return match self.transactions.get(&tx) {
                Some(TxState::Failed) => Err(MockTxError::TransactionFailed),
                Some(_) => Err(MockTxError::AlreadyClosed),
                None => Err(MockTxError::UnknownTransaction),
            };
        }

        if self.denied_keys.contains(&key) {
            self.transactions.insert(tx, TxState::Failed);
            return Err(MockTxError::PermissionDenied);
        }

        if let Some(TxState::Active(writes)) = self.transactions.get_mut(&tx) {
            writes.push((key, value));
        }
        Ok(())
    }

    fn commit(&mut self, tx: Self::TxHandle) -> Result<(), Self::Error> {
        match self.transactions.get(&tx) {
            Some(TxState::Active(writes)) => {
                let writes = writes.clone();
                for (k, v) in writes {
                    self.committed.insert(k, v);
                }
                self.transactions.insert(tx, TxState::Committed);
                Ok(())
            }
            Some(TxState::Failed) => Err(MockTxError::TransactionFailed),
            Some(_) => Err(MockTxError::AlreadyClosed),
            None => Err(MockTxError::UnknownTransaction),
        }
    }

    fn abort(&mut self, tx: Self::TxHandle) -> Result<(), Self::Error> {
        match self.transactions.get(&tx) {
            Some(TxState::Active(_)) | Some(TxState::Failed) => {
                self.transactions.insert(tx, TxState::Aborted);
                Ok(())
            }
            Some(_) => Err(MockTxError::AlreadyClosed),
            None => Err(MockTxError::UnknownTransaction),
        }
    }
}

// ---------------------------------------------------------------------
// Acceptance criterion: a trivial in-memory mock backend can implement
// the trait, and the interface is storage-agnostic (no fs/git/path types
// in any signature — evidenced by MockBackend using plain String
// key/value types).
// ---------------------------------------------------------------------
#[test]
fn trivial_in_memory_backend_implements_the_trait() {
    let mut backend = MockBackend::new();

    let tx = backend.begin().expect("begin should succeed");
    backend
        .write(tx, "key-a".to_string(), "1".to_string())
        .expect("write should succeed");
    backend
        .write(tx, "key-b".to_string(), "2".to_string())
        .expect("write should succeed");
    backend.commit(tx).expect("commit should succeed");

    assert_eq!(backend.committed_value("key-a"), Some(&"1".to_string()));
    assert_eq!(backend.committed_value("key-b"), Some(&"2".to_string()));
}

// ---------------------------------------------------------------------
// Acceptance criterion: the failed-state contract. A write-permission-
// style rejection mid-transaction puts the transaction into a failed
// state from which only abort is permitted and commit must refuse.
// ---------------------------------------------------------------------
#[test]
fn write_rejection_forces_failed_state_where_only_abort_is_permitted() {
    let mut backend = MockBackend::with_denied_keys(&["forbidden"]);

    let tx = backend.begin().expect("begin should succeed");
    backend
        .write(tx, "allowed".to_string(), "1".to_string())
        .expect("write to an allowed key should succeed");

    let rejection = backend.write(tx, "forbidden".to_string(), "x".to_string());
    assert_eq!(
        rejection,
        Err(MockTxError::PermissionDenied),
        "a write-permission-style rejection should be reported"
    );

    // Once failed, further writes must be refused...
    let further_write = backend.write(tx, "allowed".to_string(), "2".to_string());
    assert_eq!(
        further_write,
        Err(MockTxError::TransactionFailed),
        "writes must be refused once the transaction has failed"
    );

    // ...and commit must refuse...
    let commit_result = backend.commit(tx);
    assert_eq!(
        commit_result,
        Err(MockTxError::TransactionFailed),
        "commit must refuse once the transaction has failed"
    );

    // ...but abort must still be permitted, and succeed.
    let abort_result = backend.abort(tx);
    assert_eq!(
        abort_result,
        Ok(()),
        "abort must remain permitted after a failed write"
    );

    // Nothing from the failed transaction should ever have been committed.
    assert_eq!(backend.committed_value("allowed"), None);
    assert_eq!(backend.committed_value("forbidden"), None);
}

// ---------------------------------------------------------------------
// Acceptance criterion: at commit, the backend enumerates exactly the
// writes belonging to that transaction and no others. Verified with two
// interleaved transactions against the mock backend.
// ---------------------------------------------------------------------
#[test]
fn commit_applies_only_the_committing_transactions_own_writes() {
    let mut backend = MockBackend::new();

    let tx1 = backend.begin().expect("begin tx1 should succeed");
    backend
        .write(tx1, "a".to_string(), "tx1-a".to_string())
        .expect("write should succeed");

    // A second transaction is opened and interleaved with the first,
    // before tx1 commits.
    let tx2 = backend.begin().expect("begin tx2 should succeed");
    backend
        .write(tx2, "b".to_string(), "tx2-b".to_string())
        .expect("write should succeed");

    backend
        .write(tx1, "c".to_string(), "tx1-c".to_string())
        .expect("write should succeed");

    backend.commit(tx1).expect("commit tx1 should succeed");

    // Only tx1's writes ("a", "c") are visible after tx1 commits; tx2's
    // still-uncommitted write ("b") must not leak across.
    assert_eq!(backend.committed_value("a"), Some(&"tx1-a".to_string()));
    assert_eq!(backend.committed_value("c"), Some(&"tx1-c".to_string()));
    assert_eq!(
        backend.committed_value("b"),
        None,
        "tx2's write must not be visible via tx1's commit"
    );

    backend.commit(tx2).expect("commit tx2 should succeed");

    // Now tx2's write is visible too, and tx1's writes are unaffected.
    assert_eq!(backend.committed_value("b"), Some(&"tx2-b".to_string()));
    assert_eq!(backend.committed_value("a"), Some(&"tx1-a".to_string()));
    assert_eq!(backend.committed_value("c"), Some(&"tx1-c".to_string()));
}
