//! A storage-agnostic transaction interface: begin, write, commit, abort.
//!
//! [`Transaction`] describes the lifecycle of a batch of writes as an
//! interface contract, independent of any particular storage. It says
//! nothing about files, directories, or version control: a backend
//! implements it however it holds its documents, as long as it can accept
//! a [`Write`] and later either make every recorded write durable
//! ([`Transaction::commit`]) or discard them ([`Transaction::abort`]).
//!
//! A non-filesystem backend (an in-memory store, a database connection, a
//! remote service) implements this trait the same way a filesystem-backed
//! one would: define a type carrying whatever connection or session state
//! it needs, give it its own [`Transaction::Error`] type, and keep track of
//! the writes recorded since the last [`Transaction::begin`] well enough to
//! enumerate them when [`Transaction::commit`] is called (see the rustdoc
//! on `begin` for how the interface makes that enumeration exact).

use crate::model::{Content, Key};

/// A single write recorded within a transaction: either replace the
/// content stored at `key`, or remove it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// Create or overwrite the document at `key` with `content`.
    Put(Key, Content),
    /// Remove the document at `key`.
    Remove(Key),
}

/// Why a [`Transaction::write`] call was rejected.
#[derive(Debug)]
pub enum WriteRejected<E> {
    /// The caller lacked permission to make this write.
    ///
    /// Per the failed-state contract on [`Transaction::write`], a
    /// permission rejection always moves the transaction into a failed
    /// state: from that point on, only [`Transaction::abort`] is
    /// permitted, and [`Transaction::commit`] must refuse.
    PermissionDenied,
    /// Some other backend-specific failure, not covered by the contract
    /// above. Whether a failure of this kind also fails the transaction is
    /// left to the backend to decide.
    Other(E),
}

/// Why a [`Transaction::commit`] call was refused.
#[derive(Debug)]
pub enum CommitError<E> {
    /// The transaction is in the failed state described on
    /// [`Transaction::write`]: a prior write was rejected for lack of
    /// permission, and only [`Transaction::abort`] is available from here.
    Failed,
    /// Some other backend-specific failure while committing.
    Other(E),
}

/// A storage-agnostic transaction: begin, write, commit, abort.
///
/// This trait describes the shape any backend must offer to participate in
/// a transaction, without assuming a filesystem, a git repository, or any
/// other concrete storage. A backend that is not filesystem-based (an
/// in-memory map, a database, a remote API) implements `Transaction` on
/// whatever type represents its connection or session, using [`Key`] and
/// [`Content`] — both already storage-agnostic — to describe what is being
/// written, and its own [`Transaction::Error`] to describe what can go
/// wrong. Nothing in this trait's signatures names a path, a file, or a
/// repository.
pub trait Transaction {
    /// A backend-specific error for failures not already carried by this
    /// trait's own [`WriteRejected`] / [`CommitError`] variants — for
    /// example, `begin` failing to acquire a session, or `abort` failing
    /// to discard pending state.
    type Error;

    /// Begins a new transaction on this handle.
    ///
    /// Implementations are expected to give each transaction its own,
    /// freshly-cleared record of the writes recorded on it (for example,
    /// an internal `Vec<Write>` reset to empty when `begin` runs).
    /// Because [`Transaction::write`] only ever appends to that record,
    /// and [`Transaction::commit`] / [`Transaction::abort`] only ever
    /// consult that same record before it is cleared again by the next
    /// `begin`, a backend can enumerate at commit exactly the writes
    /// belonging to the current transaction and no others. No separate
    /// transaction identifier needs to appear in this trait for that to
    /// hold: the record a backend keeps between one `begin` and the next
    /// commit or abort *is* the boundary between one transaction's writes
    /// and another's.
    fn begin(&mut self) -> Result<(), Self::Error>;

    /// Records a write as part of the current transaction.
    ///
    /// A write may be rejected. If it is rejected because the caller
    /// lacked permission to make it, the transaction moves into a failed
    /// state:
    ///
    /// - From a failed state, only [`Transaction::abort`] is permitted.
    /// - From a failed state, [`Transaction::commit`] must refuse
    ///   (returning [`CommitError::Failed`]) rather than attempt to commit
    ///   a partial or unauthorized set of writes.
    ///
    /// This is an interface contract every implementation must uphold, not
    /// merely a suggestion for how a particular backend happens to behave.
    fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>>;

    /// Commits every write recorded since `begin`, making them durable.
    ///
    /// Must return [`CommitError::Failed`] without attempting to persist
    /// anything if the transaction is in the failed state described on
    /// [`Transaction::write`].
    fn commit(&mut self) -> Result<(), CommitError<Self::Error>>;

    /// Discards every write recorded since `begin`.
    ///
    /// Always permitted, regardless of state — including from the failed
    /// state described on [`Transaction::write`], which is why abort (and
    /// not commit) is the only way out of it.
    fn abort(&mut self) -> Result<(), Self::Error>;
}

/// A no-op skeleton implementation of [`Transaction`].
///
/// It accepts `begin` / `write` / `commit` / `abort` and always succeeds,
/// keeping just enough state (a plain `Vec<Write>`, cleared on `begin`,
/// `commit`, and `abort`) to demonstrate the commit-enumeration guarantee
/// described on [`Transaction::begin`]. It performs no storage of its own
/// and never rejects a write. Wiring a real backend's existing write paths
/// through this trait is a separate, later piece of work.
#[derive(Debug, Default)]
pub struct NoopTransaction {
    pending: Vec<Write>,
}

impl NoopTransaction {
    pub fn new() -> Self {
        Self::default()
    }

    /// The writes recorded on this transaction since the last `begin`,
    /// `commit`, or `abort` — exactly what a real backend would enumerate
    /// at commit time.
    pub fn pending(&self) -> &[Write] {
        &self.pending
    }
}

impl Transaction for NoopTransaction {
    type Error = std::convert::Infallible;

    fn begin(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        Ok(())
    }

    fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>> {
        self.pending.push(write);
        Ok(())
    }

    fn commit(&mut self) -> Result<(), CommitError<Self::Error>> {
        self.pending.clear();
        Ok(())
    }

    fn abort(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_transaction_enumerates_only_its_own_pending_writes() {
        let mut tx = NoopTransaction::new();
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("a"), "content".to_string()))
            .unwrap();
        tx.write(Write::Remove(Key::name("b"))).unwrap();

        assert_eq!(
            tx.pending(),
            &[
                Write::Put(Key::name("a"), "content".to_string()),
                Write::Remove(Key::name("b")),
            ]
        );

        tx.commit().unwrap();
        assert!(tx.pending().is_empty());

        // A fresh transaction on the same handle starts with no writes
        // from the one that was just committed.
        tx.begin().unwrap();
        assert!(tx.pending().is_empty());
    }

    #[test]
    fn noop_transaction_abort_discards_pending_writes() {
        let mut tx = NoopTransaction::new();
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("a"), "content".to_string()))
            .unwrap();

        tx.abort().unwrap();
        assert!(tx.pending().is_empty());
    }

    /// A minimal backend that demonstrates the failed-state contract:
    /// a permission-denied write moves it into a failed state from which
    /// only `abort` is permitted and `commit` refuses.
    #[derive(Default)]
    struct FailOnceTransaction {
        pending: Vec<Write>,
        failed: bool,
    }

    impl Transaction for FailOnceTransaction {
        type Error = ();

        fn begin(&mut self) -> Result<(), Self::Error> {
            self.pending.clear();
            self.failed = false;
            Ok(())
        }

        fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>> {
            if let Write::Remove(_) = write {
                self.failed = true;
                return Err(WriteRejected::PermissionDenied);
            }
            self.pending.push(write);
            Ok(())
        }

        fn commit(&mut self) -> Result<(), CommitError<Self::Error>> {
            if self.failed {
                return Err(CommitError::Failed);
            }
            self.pending.clear();
            Ok(())
        }

        fn abort(&mut self) -> Result<(), Self::Error> {
            self.pending.clear();
            self.failed = false;
            Ok(())
        }
    }

    #[test]
    fn permission_rejection_fails_the_transaction_and_commit_refuses() {
        let mut tx = FailOnceTransaction::default();
        tx.begin().unwrap();

        let rejected = tx.write(Write::Remove(Key::name("secret")));
        assert!(matches!(rejected, Err(WriteRejected::PermissionDenied)));

        let commit_result = tx.commit();
        assert!(matches!(commit_result, Err(CommitError::Failed)));

        // Only abort is permitted from the failed state.
        assert!(tx.abort().is_ok());
    }
}
