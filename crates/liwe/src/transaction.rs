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

/// One call recorded by [`RecordingTransaction`]: which interface
/// operation was invoked and, for `write`, what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxEvent {
    Begin,
    Write(Write),
    Commit,
    Abort,
}

/// A shared call record that one or more [`RecordingTransaction`] values
/// write into.
///
/// Every canonical write path in this codebase builds a fresh transaction
/// value per individual write (see `NoopTransaction::new()` at each call
/// site), so a test driving such a call site has no handle to inspect
/// afterward once that value is dropped. Cloning a `TransactionLog` clones
/// the handle, not the record: every `RecordingTransaction` built from
/// clones of the same log (for example, from a `FnMut() ->
/// RecordingTransaction` factory that clones the log into each value it
/// produces) appends to the one underlying record, so a test can inspect
/// it after the write path under test has returned.
#[derive(Debug, Clone, Default)]
pub struct TransactionLog(std::rc::Rc<std::cell::RefCell<Vec<TxEvent>>>);

impl TransactionLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call recorded on this log so far, in the order it happened.
    pub fn events(&self) -> Vec<TxEvent> {
        self.0.borrow().clone()
    }

    pub fn begin_count(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, TxEvent::Begin))
            .count()
    }

    pub fn commit_count(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, TxEvent::Commit))
            .count()
    }

    pub fn abort_count(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, TxEvent::Abort))
            .count()
    }

    pub fn write_count(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, TxEvent::Write(_)))
            .count()
    }

    fn record(&self, event: TxEvent) {
        self.0.borrow_mut().push(event);
    }
}

/// A test-only stub [`Transaction`] backend that records every `begin` /
/// `write` / `commit` / `abort` call it receives into a shared
/// [`TransactionLog`], and can optionally be configured to reject the next
/// `write` or refuse every `commit`.
///
/// This exists so a test can assert, from outside the write path under
/// test and against the stub's own call record, that a canonical write
/// path actually drives `begin` and `commit` on whatever `Transaction` it
/// is given — rather than merely compiling against the trait — and that a
/// refusal from the backend (a rejected write, a refused commit) is
/// neither silently swallowed nor allowed to let the write land.
#[derive(Debug, Clone)]
pub struct RecordingTransaction {
    log: TransactionLog,
    failed: bool,
    reject_next_write: bool,
    refuse_commit: bool,
}

impl RecordingTransaction {
    /// A stub that accepts every call, like [`NoopTransaction`], but
    /// records each one into `log`.
    pub fn new(log: TransactionLog) -> Self {
        Self {
            log,
            failed: false,
            reject_next_write: false,
            refuse_commit: false,
        }
    }

    /// A stub whose `commit` always refuses with a backend-specific
    /// error, standing in for a real backend that fails to make writes
    /// durable (a full disk, a lost connection, a rejected push) — for
    /// proving a commit refusal surfaces to the caller as an error rather
    /// than being silently swallowed, and does not let the write land.
    pub fn refusing_commit(log: TransactionLog) -> Self {
        Self {
            refuse_commit: true,
            ..Self::new(log)
        }
    }

    /// A stub whose next `write` call is rejected for lack of permission,
    /// standing in for the real freeze/mutability logic T10/T11 add
    /// later (not landed as of T6). Per the failed-state contract on
    /// [`Transaction::write`], this moves the transaction into the
    /// failed state, so a subsequent `commit` must refuse and only
    /// `abort` is permitted from there.
    pub fn rejecting_next_write(log: TransactionLog) -> Self {
        Self {
            reject_next_write: true,
            ..Self::new(log)
        }
    }
}

impl Transaction for RecordingTransaction {
    type Error = String;

    fn begin(&mut self) -> Result<(), Self::Error> {
        self.log.record(TxEvent::Begin);
        self.failed = false;
        Ok(())
    }

    fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>> {
        self.log.record(TxEvent::Write(write));
        if self.reject_next_write {
            self.reject_next_write = false;
            self.failed = true;
            return Err(WriteRejected::PermissionDenied);
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), CommitError<Self::Error>> {
        self.log.record(TxEvent::Commit);
        if self.failed {
            return Err(CommitError::Failed);
        }
        if self.refuse_commit {
            return Err(CommitError::Other(
                "commit refused by test stub".to_string(),
            ));
        }
        Ok(())
    }

    fn abort(&mut self) -> Result<(), Self::Error> {
        self.log.record(TxEvent::Abort);
        self.failed = false;
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

    #[test]
    fn recording_transaction_logs_begin_write_commit() {
        let log = TransactionLog::new();
        let mut tx = RecordingTransaction::new(log.clone());

        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("a"), "content".to_string()))
            .unwrap();
        tx.commit().unwrap();

        assert_eq!(
            log.events(),
            &[
                TxEvent::Begin,
                TxEvent::Write(Write::Put(Key::name("a"), "content".to_string())),
                TxEvent::Commit,
            ]
        );
    }

    #[test]
    fn recording_transaction_refusing_commit_records_the_attempt_and_refuses() {
        let log = TransactionLog::new();
        let mut tx = RecordingTransaction::refusing_commit(log.clone());

        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("a"), "content".to_string()))
            .unwrap();
        let commit_result = tx.commit();

        assert!(matches!(commit_result, Err(CommitError::Other(_))));
        assert_eq!(log.commit_count(), 1);
    }

    #[test]
    fn recording_transaction_rejecting_next_write_fails_the_transaction() {
        let log = TransactionLog::new();
        let mut tx = RecordingTransaction::rejecting_next_write(log.clone());

        tx.begin().unwrap();
        let rejected = tx.write(Write::Put(Key::name("a"), "content".to_string()));
        assert!(matches!(rejected, Err(WriteRejected::PermissionDenied)));

        let commit_result = tx.commit();
        assert!(matches!(commit_result, Err(CommitError::Failed)));
        assert!(tx.abort().is_ok());
        assert_eq!(log.abort_count(), 1);
    }
}
