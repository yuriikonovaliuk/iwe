//! Black-box test suite for
//! `efforts/knowledge-compositor/m6-b-cutover-preconditions/5-iwe-t3-cli-lock-wiring/contract`,
//! written directly from that task's acceptance criteria and shared
//! surface -- never from reading Developer's in-flight wiring of the CLI
//! write commands. Every name used below comes either from the contract's
//! "Shared surface" section (`liwe::write_lock::{acquire_commit_lock,
//! CommitLockGuard, CommitLockError, DEFAULT_LOCK_PATH}`) or from the
//! already-landed, independent `iwe-lock` crate's own public API (used
//! here only to *engineer* the two required concurrency scenarios from
//! outside the CLI process, never to inspect or drive the CLI's
//! internals).
//!
//! The CLI is driven as a real subprocess (`Command`) throughout -- the
//! actual integration point the contract asks to be tested, not a
//! function call into the wiring itself.

use std::fs::{self, create_dir_all, write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use diwe::config::Configuration;
use iwe_lock::{FileLock, LockConfig};
use liwe::write_lock::{acquire_commit_lock, DEFAULT_LOCK_PATH};
use tempfile::TempDir;

/// Minimal store -- same shape `crates/iwe/tests/create_test.rs`'s
/// `setup()` uses. Nothing about locking is store-specific, so the
/// simplest possible store is enough for every test here.
fn store() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe")).expect("mkdir .iwe");
    let mut config = Configuration::default();
    config.library.path = "".to_string();
    config.markdown.refs_extension = "".to_string();
    write(
        temp.path().join(".iwe/config.toml"),
        toml::to_string(&config).expect("config"),
    )
    .expect("write config");
    temp
}

fn spawn_iwe(work_dir: &Path, args: &[&str]) -> Child {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn iwe")
}

fn run_iwe(work_dir: &Path, args: &[&str]) -> Output {
    spawn_iwe(work_dir, args)
        .wait_with_output()
        .expect("run iwe")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Recursive snapshot of every file's relative path and content under
/// `root`, excluding the lock file itself -- whose churn is exactly what
/// these tests engineer, not part of the "tree" the acceptance criteria
/// mean by "does not modify the tree."
fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
                continue;
            }
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if rel == Path::new(DEFAULT_LOCK_PATH) {
                continue;
            }
            let bytes = fs::read(&path).expect("read file");
            out.push((rel, bytes));
        }
    }
    let mut out = Vec::new();
    if root.exists() {
        walk(root, root, &mut out);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Raw generation field of the lock's on-disk state (first 8 bytes,
/// little-endian, per `iwe_lock`'s own module documentation), read
/// passively -- never through `FileLock::acquire`, which would itself
/// grab a free lock rather than just observe it. `None` while the file
/// doesn't exist yet, is too short to hold a generation, or still reads
/// generation 0 (never held).
///
/// This is the only way to observe, from outside, that the CLI has
/// *already* completed its own first acquire without racing it for that
/// very acquire. The on-disk byte layout isn't part of the contract's
/// pinned surface (only `DEFAULT_LOCK_PATH` is); this is a best-effort
/// engineering technique for driving the race deterministically, not an
/// API dependency -- see the test-builder's delivery report for the
/// tradeoff this implies.
fn observed_generation(lock_path: &Path) -> Option<u64> {
    let bytes = fs::read(lock_path).ok()?;
    if bytes.len() < 8 {
        return None;
    }
    let generation = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    (generation != 0).then_some(generation)
}

/// Required test (1): "a CLI write attempted while the lock is held by a
/// live heartbeating other holder times out and reports refusal, not a
/// hang." Exercises the acceptance criteria "On Timeout, the command
/// refuses the write immediately, deterministic error, no indefinite
/// block" and "acquire_commit_lock is called ... before validate-final-state"
/// (a lock contended for the whole run proves the acquire gates the
/// entire command, not some step deep inside it).
///
/// Shared-surface entry exercised: `liwe::write_lock::acquire_commit_lock`
/// (used directly, from a background thread, to hold a genuine,
/// live-heartbeating lock exactly as a concurrent `iwe`/`iwec` process
/// would).
#[test]
fn write_while_lock_held_by_live_holder_times_out_and_refuses() {
    let temp = store();

    let holder_root = temp.path().to_path_buf();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = thread::spawn(move || {
        let guard =
            acquire_commit_lock(&holder_root).expect("holder must acquire the free lock");
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(guard);
    });
    ready_rx.recv().expect("holder must report it is holding the lock");

    let started = Instant::now();
    let output = run_iwe(temp.path(), &["create", "notes/new", "--content", "# New\n"]);
    let elapsed = started.elapsed();

    release_tx.send(()).unwrap();
    holder.join().unwrap();

    assert!(
        !output.status.success(),
        "a write that can never acquire the lock must be refused, not silently succeed"
    );
    assert!(
        !stderr_of(&output).trim().is_empty(),
        "the refusal must be reported as a deterministic error, not fail silently"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "must not block indefinitely on a held, live-heartbeating lock: took {elapsed:?}"
    );
    assert!(
        !temp.path().join("notes/new.md").exists(),
        "a refused write must not land on disk"
    );
}

/// Required test (2): "a CLI write whose lock is reclaimed by a
/// concurrent process between its acquire and its apply step is caught
/// by check_fencing and provably does not modify the tree (diff tree
/// before/after)." Exercises the acceptance criterion "Immediately
/// before apply, the command calls check_fencing(); on Stale it aborts
/// without applying, refuses the commit, falls back to pending -- no
/// partial apply."
///
/// Shared-surface entry exercised: `liwe::write_lock::DEFAULT_LOCK_PATH`
/// (to locate the lock file the CLI must be using).
///
/// Engineering the race (see this suite's delivery report for the full
/// writeup): no hook into the CLI exists or is used. Instead:
/// 1. Snapshot the tree before running anything.
/// 2. Spawn the CLI write; on the main thread, passively poll the lock
///    file's raw generation (`observed_generation`) without ever calling
///    `FileLock::acquire` while it reads absent/0 -- an `acquire` call
///    against a genuinely free lock would grab it itself, racing the CLI
///    for its *own* first acquire instead of reclaiming it afterward.
/// 3. The instant a nonzero generation is observed (proof the CLI has
///    already completed its own acquire), immediately issue one
///    `iwe_lock::FileLock::acquire` against the same path with a
///    `stale_after` of 1ms -- far shorter than the real elapsed time
///    between the CLI's acquire and this reaction, given the CLI's own
///    heartbeat interval is measured in the tens of milliseconds. That
///    reclaim supersedes the CLI's generation before it can reach its
///    pre-apply fencing check.
#[test]
fn concurrent_reclaim_between_acquire_and_apply_is_caught_by_fencing_and_the_tree_is_unmodified() {
    let temp = store();
    let lock_path = temp.path().join(DEFAULT_LOCK_PATH);
    let before = snapshot_tree(temp.path());

    let mut child = spawn_iwe(temp.path(), &["create", "notes/new", "--content", "# New\n"]);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if observed_generation(&lock_path).is_some() {
            break;
        }
        if let Ok(Some(_status)) = child.try_wait() {
            panic!(
                "the CLI write finished without ever appearing to acquire {} -- \
                 either 5-iwe-t3-cli-lock-wiring's CLI wiring is not landed yet, \
                 or `iwe create` no longer acquires the commit lock there",
                DEFAULT_LOCK_PATH
            );
        }
        assert!(
            Instant::now() < deadline,
            "the CLI never appeared to acquire the commit lock within 5s"
        );
        thread::sleep(Duration::from_micros(50));
    }

    let reclaimer = FileLock::new(LockConfig {
        path: lock_path.clone(),
        heartbeat_interval: Duration::from_millis(1),
        stale_after: Duration::from_millis(1),
        acquire_timeout: Duration::from_secs(2),
    });
    let reclaim_guard = reclaimer
        .acquire()
        .expect("reclaiming a lock whose only heartbeat is already >1ms old must succeed");

    let output = child.wait_with_output().expect("run iwe create");
    let after = snapshot_tree(temp.path());

    assert!(
        !output.status.success(),
        "a write whose lock was reclaimed mid-window must refuse the commit, not apply it \
         (got success -- the reclaim likely landed after the CLI had already committed; \
         see the delivery report's timing-engineering note)"
    );
    assert!(
        !stderr_of(&output).trim().is_empty(),
        "the refusal must be reported"
    );
    assert!(
        !temp.path().join("notes/new.md").exists(),
        "no partial apply: the new document must not have been written"
    );
    assert_eq!(
        before, after,
        "the tree must be byte-for-byte unmodified when fencing catches a mid-window reclaim"
    );

    drop(reclaim_guard);
}

/// Acceptance criterion: "Guard released on every exit path" -- the
/// success path. A write that completes cleanly must leave the lock
/// free immediately afterward, not merely reclaimable once it goes
/// stale a second later.
///
/// Shared-surface entry exercised: `liwe::write_lock::acquire_commit_lock`
/// (used directly, after the CLI exits, to prove the lock is free).
#[test]
fn guard_is_released_after_a_successful_write() {
    let temp = store();
    let output = run_iwe(temp.path(), &["create", "notes/ok", "--content", "# Ok\n"]);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let started = Instant::now();
    let guard = acquire_commit_lock(temp.path());
    let elapsed = started.elapsed();

    assert!(
        guard.is_ok(),
        "the lock must be immediately acquirable after a successful write"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "acquiring right after a clean exit must be fast, not wait out a leaked lock's \
         staleness window: took {elapsed:?}"
    );
    drop(guard);
}

/// Acceptance criterion: "Guard released on every exit path" -- a
/// validation-failure path unrelated to locking (writing over an
/// existing document, refused the same way before this task's wiring
/// existed). Proves the guard's release isn't wired only into the
/// success path.
///
/// Shared-surface entry exercised: `liwe::write_lock::acquire_commit_lock`.
#[test]
fn guard_is_released_after_a_validation_failure() {
    let temp = store();
    create_dir_all(temp.path().join("notes")).unwrap();
    write(temp.path().join("notes/dup.md"), "# Existing\n").unwrap();

    let output = run_iwe(temp.path(), &["create", "notes/dup", "--content", "# Dup\n"]);
    assert!(
        !output.status.success(),
        "writing over an existing document must still be refused"
    );

    let started = Instant::now();
    let guard = acquire_commit_lock(temp.path());
    let elapsed = started.elapsed();

    assert!(
        guard.is_ok(),
        "the lock must be immediately acquirable after a validation-failure exit"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "acquiring right after a refused write must be fast, not wait out a leaked lock's \
         staleness window: took {elapsed:?}"
    );
    drop(guard);
}
