//! Black-box test suite for
//! `efforts/knowledge-compositor/m6-b-cutover-preconditions/t5-fencing-check-before-write/contract`,
//! written directly from that task's acceptance criteria and shared
//! surface -- never from reading Developer's in-flight relocation of the
//! `check_fencing()` calls. Every name used below comes from the
//! contract's "Shared surface" section (the three call sites themselves,
//! and `check_fencing()` -- reused, not a new API) or from the
//! already-landed `iwe-lock` crate's own public API (used here only to
//! *engineer* the reclaim race from outside the CLI process, never to
//! inspect or drive its internals).
//!
//! `crates/iwe/tests/cli_lock_wiring_test.rs` already proves this same
//! "reclaim lands mid-window, fencing catches it, tree is untouched"
//! property for `iwe create` -- the `NoopTransaction` path through
//! `crates/iwe/src/new.rs::write_document`. This file extends that same,
//! already-established race-engineering technique to the *other two*
//! `NoopTransaction` commit paths this task's contract also names:
//!
//!   - `iwe update`, which funnels through `write_single_document` /
//!     `write_single_document_with` (`crates/iwe/src/main.rs`, lines
//!     ~4053-4128 in the contract's numbering).
//!   - `iwe delete`, which funnels through the local `apply_changes`
//!     wrapper (`crates/iwe/src/main.rs`, lines ~2908-2922) into
//!     `diwe::fs::apply_changes` / `apply_changes_with`
//!     (`crates/diwe/src/fs.rs`).
//!
//! Before this task, only the `new.rs` site had this race exercised at
//! all -- a defect fixed at only one or two of the three named sites
//! (contract acceptance criterion 4: "all three sites moved together in
//! one atomic commit") would have shipped invisibly for the other
//! site(s). These two tests close that hole: each must independently
//! catch a fencing failure on its own call path, tree left byte-for-byte
//! unmodified, matching AC6's "no partial write, falls back to
//! pending/open".
//!
//! What this technique does *not* prove, and an honest limitation shared
//! with `cli_lock_wiring_test.rs`'s own test: the reclaimer reacts as
//! soon as it can observe the CLI's lock generation on disk, which can
//! land anywhere across the whole acquire-to-completion span of the
//! command, not deterministically inside the narrow interval between
//! `check_fencing()` returning `Ok` and the actual filesystem write that
//! follows it (AC7's "check→write window"). See this task's delivery
//! report, and `fencing_placement_test.rs` in this same directory (and
//! its sibling in `crates/diwe/tests/`), for why that narrower interval
//! is addressed with a different technique instead of a race.

use std::fs::{self, create_dir_all, write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use diwe::config::Configuration;
use iwe_lock::{FileLock, LockConfig};
use liwe::write_lock::DEFAULT_LOCK_PATH;
use tempfile::TempDir;

fn store_with_docs(docs: &[(&str, &str)]) -> TempDir {
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
    for (key, content) in docs {
        let path = temp.path().join(format!("{key}.md"));
        if let Some(parent) = path.parent() {
            create_dir_all(parent).expect("mkdir doc parent");
        }
        write(path, content).expect("write doc");
    }
    temp
}

/// Spawns `iwe` with `IWE_TEST_LOCK_FENCING_DELAY_MS`
/// (`iwe::new::widen_fencing_window_for_test`, read by
/// `acquire_cli_commit_lock` -- the entry point all three CLI write
/// commands this suite exercises share) set in the spawned process's own
/// environment only, never this test process's. Widens the otherwise
/// microsecond-wide acquire-to-fencing-check gap so
/// `run_with_mid_window_reclaim`'s external reclaimer lands inside it
/// deterministically instead of racing real scheduler jitter -- matters
/// under CPU contention (another test suite running concurrently), where
/// the plain microsecond gap is too narrow for the reclaimer thread to
/// reliably win. Same technique as `cli_lock_wiring_test.rs`'s own
/// equivalent helper; does not change what either test proves, only how
/// reliably its race lands.
fn spawn_iwe_widening_fencing_window(work_dir: &Path, args: &[&str], delay_ms: u64) -> Child {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        .current_dir(work_dir)
        .env("IWE_TEST_LOCK_FENCING_DELAY_MS", delay_ms.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn iwe")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Recursive snapshot of every file's relative path and content under
/// `root`, excluding the lock file itself and `.iwe/config.toml` --
/// broader than `cli_lock_wiring_test.rs`'s own `snapshot_tree`, which
/// excludes only the lock file. Investigating this task independently
/// surfaced that `.iwe/config.toml` is rewritten (schema-version bump,
/// new default sections added) on *every* `iwe` invocation that reaches
/// config loading, success or refusal alike -- entirely unrelated to
/// this task's fencing defect or its fix, and already true of
/// `cli_lock_wiring_test.rs`'s own pre-existing reclaim test against the
/// current tree. Excluded here so this suite's assertions are about the
/// fencing behavior this task actually changes, not that pre-existing,
/// orthogonal config-migration side effect.
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
            if rel == Path::new(DEFAULT_LOCK_PATH) || rel == Path::new(".iwe/config.toml") {
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

/// Raw generation field of the lock's on-disk state -- see
/// `cli_lock_wiring_test.rs::observed_generation` for the full rationale;
/// duplicated here for the same reason `snapshot_tree` is.
fn observed_generation(lock_path: &Path) -> Option<u64> {
    let bytes = fs::read(lock_path).ok()?;
    if bytes.len() < 8 {
        return None;
    }
    let generation = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    (generation != 0).then_some(generation)
}

/// Spawns `iwe` with `args`, reclaims the store-wide commit lock the
/// instant the CLI's own first acquire becomes observable on disk, and
/// returns the CLI's output together with the tree snapshots taken
/// immediately before spawning and immediately after the CLI exits.
fn run_with_mid_window_reclaim(
    temp: &TempDir,
    args: &[&str],
) -> (Output, Vec<(PathBuf, Vec<u8>)>, Vec<(PathBuf, Vec<u8>)>) {
    let lock_path = temp.path().join(DEFAULT_LOCK_PATH);
    let before = snapshot_tree(temp.path());

    let mut child = spawn_iwe_widening_fencing_window(temp.path(), args, 300);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if observed_generation(&lock_path).is_some() {
            break;
        }
        if let Ok(Some(_status)) = child.try_wait() {
            panic!(
                "iwe {args:?} finished without ever appearing to acquire {} -- either this \
                 task's relocation isn't landed yet, or this command no longer acquires the \
                 commit lock on this path",
                DEFAULT_LOCK_PATH
            );
        }
        assert!(
            Instant::now() < deadline,
            "iwe {args:?} never appeared to acquire the commit lock within 5s"
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

    let output = child.wait_with_output().expect("run iwe");
    let after = snapshot_tree(temp.path());
    drop(reclaim_guard);

    (output, before, after)
}

/// Contract AC4/AC6, exercised on the `write_single_document` /
/// `write_single_document_with` site (`crates/iwe/src/main.rs`, the
/// contract's second listed location): a lock reclaimed mid-window while
/// `iwe update` is committing must be caught by fencing, refuse the
/// commit, and leave the document's on-disk content unchanged -- not a
/// half-applied edit.
#[test]
fn concurrent_reclaim_during_update_is_caught_by_fencing_and_the_tree_is_unmodified() {
    let temp = store_with_docs(&[("notes/roadmap", "---\nreviewed: false\n---\n\n# Roadmap\n")]);

    let (output, before, after) = run_with_mid_window_reclaim(
        &temp,
        &["update", "-k", "notes/roadmap", "--set", "reviewed=true"],
    );

    assert!(
        !output.status.success(),
        "an update whose lock was reclaimed mid-window must refuse the commit, not apply it \
         (got success -- the reclaim likely landed after the CLI had already committed)"
    );
    assert!(!stderr_of(&output).trim().is_empty(), "the refusal must be reported");
    assert_eq!(
        before, after,
        "the tree must be byte-for-byte unmodified when fencing catches a mid-window reclaim \
         during `iwe update`"
    );
}

/// Contract AC4/AC6, exercised on the `apply_changes` /
/// `apply_changes_with` site (`crates/iwe/src/main.rs`'s wrapper at the
/// contract's first listed location, into `crates/diwe/src/fs.rs`, the
/// contract's third listed location): a lock reclaimed mid-window while
/// `iwe delete` is committing must be caught by fencing, refuse the
/// commit, and leave the target document in place -- no partial removal.
#[test]
fn concurrent_reclaim_during_delete_is_caught_by_fencing_and_the_tree_is_unmodified() {
    let temp = store_with_docs(&[("notes/a", "# A\n"), ("notes/b", "# B\n")]);

    let (output, before, after) = run_with_mid_window_reclaim(&temp, &["delete", "notes/b"]);

    assert!(
        !output.status.success(),
        "a delete whose lock was reclaimed mid-window must refuse the commit, not apply it \
         (got success -- the reclaim likely landed after the CLI had already committed)"
    );
    assert!(!stderr_of(&output).trim().is_empty(), "the refusal must be reported");
    assert!(
        temp.path().join("notes/b.md").exists(),
        "no partial apply: a refused delete must not have removed the document"
    );
    assert_eq!(
        before, after,
        "the tree must be byte-for-byte unmodified when fencing catches a mid-window reclaim \
         during `iwe delete`"
    );
}
