//! CLI half of the black-box test suite for the `[commit]` trigger
//! (`efforts/knowledge-compositor/m6-b-cutover-preconditions/
//! 6-t1-iwe-commit-trigger`), written directly from the task contract's
//! acceptance criteria 2–5 — never from Developer's implementation of the
//! trigger, which is not read here. Every observable asserted below is
//! pinned by the contract:
//!
//!   - the `[commit]` TOML surface (`command`, `timeout_seconds`) plus
//!     `[journal] path` on top of `Configuration`;
//!   - one trigger invocation per successful commit whose journal append
//!     produced a record (one probe line per journal line);
//!   - no record ⇒ no trigger (`[commit]` without `[journal] path`; a
//!     write with empty effects);
//!   - the probe runs inside the commit-lock window: `IWE_COMMIT_LOCK_
//!     GENERATION` equals `iwe_lock::current_generation(<store>/.iwe/
//!     write.lock)` and `is_held_now(..., 1s)` is true at probe time;
//!   - fail-open + bounded: `command = "false"`, a nonexistent binary, and
//!     a sleep beyond `timeout_seconds` never touch the write, the
//!     journal, or the caller's exit/result, emit at most one stderr line
//!     prefixed `iwe: commit trigger failed (ignored):`, and the timed-out
//!     child returns well under its own sleep.
//!
//! The CLI is driven as a real subprocess throughout, against scratch
//! stores whose `.iwe/config.toml` the test writes (never a real store).
//! The probe is this crate's `iwe_commit_probe` `[[bin]]` fixture
//! (`tests/support/iwe_commit_probe.rs`); it appends one JSON line per
//! invocation to a log path the test passes via the `command` string.
//!
//! The iwec halves of criteria 2, 3 and 6 live in
//! `crates/iwec/tests/commit_trigger_test.rs`, mirroring the
//! `cli_mcp_write_scope_parity_test` split between the two crates.

use std::fs::{self, create_dir_all, write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// This crate's probe fixture binary (a `[[bin]]`, so `cargo test`
/// guarantees it is built before these tests run).
const PROBE: &str = env!("CARGO_BIN_EXE_iwe_commit_probe");

const FAIL_PREFIX: &str = "iwe: commit trigger failed (ignored):";
const STORE_ROOT_ENV: &str = "IWE_STORE_ROOT";
const GENERATION_ENV: &str = "IWE_COMMIT_LOCK_GENERATION";

/// The sleep a timed-out trigger child is made to attempt, in ms — the
/// boundedness test asserts the caller returns well under this.
const TIMEOUT_CASE_SLEEP_MS: u64 = 3000;
/// The `timeout_seconds` the timed-out trigger's config carries.
const TIMEOUT_CASE_TIMEOUT_SECS: u64 = 1;

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

fn store_with_config(config_toml: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    create_dir_all(dir.path().join(".iwe")).expect("mkdir .iwe");
    write(dir.path().join(".iwe/config.toml"), config_toml).expect("write config");
    dir
}

/// The shell command string for the `[commit]` trigger: this crate's
/// probe binary appending to `probe_log`. Both paths are absolute; the
/// test refuses whitespace or quotes (the trigger is shell-spawned, and
/// the contract pins the command as a bare string).
fn probe_command(probe_log: &Path) -> String {
    for path in [Path::new(PROBE), probe_log] {
        let text = path.to_string_lossy();
        assert!(
            !text.chars().any(|c| c.is_whitespace() || c == '"'),
            "shell-unsafe fixture path: {text}"
        );
    }
    format!("{} {}", PROBE, probe_log.display())
}

/// `[commit]`/`[journal]`/`[transactions]` config for a scratch store.
fn trigger_config(command: &str, journal: bool, validate: Option<&str>, timeout_secs: Option<u64>) -> String {
    let mut out = String::new();
    if let Some(scope) = validate {
        out.push_str("[transactions]\n");
        out.push_str(&format!("validate = \"{scope}\"\n\n"));
    }
    if journal {
        out.push_str("[journal]\n");
        out.push_str("path = \".iwe/journal.ndjson\"\n\n");
    }
    out.push_str("[commit]\n");
    out.push_str(&format!("command = \"{command}\"\n"));
    if let Some(secs) = timeout_secs {
        out.push_str(&format!("timeout_seconds = {secs}\n"));
    }
    out
}

fn run_iwe(work_dir: &Path, args: &[&str]) -> Output {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        // Hermeticity: no ambient override may leak into the store's
        // resolved config, and neither trigger env var may be inherited
        // pre-set (the trigger must supply its own values).
        .env_remove("IWE_TRANSACTIONS_DENY")
        .env_remove("IWE_TRANSACTIONS_ALLOW")
        .env_remove(STORE_ROOT_ENV)
        .env_remove(GENERATION_ENV)
        .env_remove("IWE_TEST_LOCK_FENCING_DELAY_MS")
        .current_dir(work_dir)
        .output()
        .expect("run iwe")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read_json_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("record line parses"))
                .collect()
        })
        .unwrap_or_default()
}

fn journal_records(store: &TempDir) -> Vec<Value> {
    read_json_lines(&store.path().join(".iwe/journal.ndjson"))
}

fn probe_lines(probe_log: &Path) -> Vec<Value> {
    read_json_lines(probe_log)
}

/// The contract's in-window invariants, asserted against every probe line
/// produced by a live trigger: the recorded generation is the store's lock
/// file's current generation, and the hold was observed as held — the
/// probe ran inside the caller's commit-lock window, on the same hold the
/// env var names.
fn assert_in_window(line: &Value, store: &TempDir, where_: &str) {
    let root = PathBuf::from(line["root"].as_str().unwrap_or_else(|| panic!("{where_}: probe must record IWE_STORE_ROOT")));
    let canonical = store.path().canonicalize().expect("store path canonicalizes");
    assert!(
        root.canonicalize().map(|c| c == canonical).unwrap_or(false) || root == store.path(),
        "{where_}: probe root {root:?} is not the store {canonical:?}"
    );
    let presented = line["presented"]
        .as_u64()
        .unwrap_or_else(|| panic!("{where_}: probe must record a decimal IWE_COMMIT_LOCK_GENERATION"));
    assert!(presented > 0, "{where_}: presented generation must be a live hold's generation");
    assert_eq!(
        line["current"].as_u64(),
        Some(presented),
        "{where_}: presented generation must equal the lock state's current_generation"
    );
    assert_eq!(
        line["held"].as_bool(),
        Some(true),
        "{where_}: the probe must observe the lock held (ran inside the window)"
    );
}

fn assert_all_probe_lines_in_window(probe_log: &Path, store: &TempDir, where_: &str) {
    let lines = probe_lines(probe_log);
    assert!(!lines.is_empty(), "{where_}: expected at least one probe line");
    for (index, line) in lines.iter().enumerate() {
        assert_in_window(line, store, &format!("{where_} line {index}"));
    }
}

/// Fail-open's stderr bound: at most one stderr line, and any such line
/// carries the pinned prefix (saying nothing further about which failure
/// modes print is itself the contract).
fn assert_fail_open_stderr(output: &Output) {
    let stderr = stderr(output);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(
        lines.len() <= 1,
        "at most one stderr line from a failed trigger, got: {lines:?}"
    );
    if let Some(line) = lines.first() {
        assert!(
            line.starts_with(FAIL_PREFIX),
            "the single line must carry the pinned prefix, got: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Criterion 2 — once per record: probe appends one line per invocation;
// CLI no-op write and CLI `[transactions] validate` multi-op each produce
// exactly one probe line and one journal line.
// ---------------------------------------------------------------------------

/// A no-op CLI commit: `iwe new` (template mode) with `--if-exists
/// override` writing byte-identical content over an existing document.
/// The tree does not change — the seeded bytes survive verbatim — yet the
/// commit machinery still records its `update` effect, and the trigger
/// must fire exactly once, matching that single journal record. (The
/// empty-effects contrast, where a genuinely change-free write records
/// nothing, is criterion 3's leg.)
#[test]
fn cli_no_op_override_write_commits_one_journal_line_and_one_probe_line() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, None, None));
    // The exact bytes `iwe new "Noop Test" --content "Same body"` renders
    // from the stock template (title header + blank line + content).
    write(store.path().join("noop-test.md"), "# Noop Test\n\nSame body").expect("seed doc");

    let output = run_iwe(
        store.path(),
        &["new", "Noop Test", "--content", "Same body", "--if-exists", "override"],
    );
    assert!(output.status.success(), "no-op write must succeed: {}", stderr(&output));
    assert_eq!(
        fs::read(store.path().join("noop-test.md")).expect("doc on disk"),
        b"# Noop Test\n\nSame body",
        "the override must be a true no-op: the seeded bytes survive"
    );

    let records = journal_records(&store);
    let probes = probe_lines(&probe_log);
    assert_eq!(records.len(), 1, "one commit, one journal line: {records:?}");
    assert_eq!(probes.len(), 1, "one commit, one trigger invocation");
    assert_all_probe_lines_in_window(&probe_log, &store, "no-op override write");
}

/// A multi-op CLI commit under `[transactions] validate` — a rename, whose
/// remove + create land as one journal record — must also fire the trigger
/// once.
#[test]
fn cli_validate_full_rename_commits_one_journal_line_and_one_probe_line() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, Some("full"), None));
    write(store.path().join("a.md"), "# A\n").expect("seed doc");

    let output = run_iwe(store.path(), &["rename", "a", "b"]);
    assert!(output.status.success(), "rename must succeed: {}", stderr(&output));

    let records = journal_records(&store);
    let probes = probe_lines(&probe_log);
    assert_eq!(records.len(), 1, "one rename commit, one journal line: {records:?}");
    let effects = records[0]["effects"].as_array().expect("record carries effects");
    assert_eq!(effects.len(), 2, "a rename's record carries remove + create: {records:?}");
    assert!(effects.iter().any(|e| e["effect"] == "delete"));
    assert!(effects.iter().any(|e| e["effect"] == "create"));
    assert_eq!(probes.len(), 1, "one rename commit, one trigger invocation");
    assert_all_probe_lines_in_window(&probe_log, &store, "validate-full rename");
}

// ---------------------------------------------------------------------------
// Criterion 3 — no record ⇒ no trigger.
// ---------------------------------------------------------------------------

/// `[commit]` configured without `[journal] path`: the write lands, no
/// journal record exists, and the trigger never runs.
#[test]
fn commit_without_journal_path_fires_no_trigger() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, false, None, None));

    let output = run_iwe(store.path(), &["create", "a", "--content", "# A\n"]);
    assert!(output.status.success(), "write must succeed: {}", stderr(&output));
    assert!(store.path().join("a.md").exists(), "the write must land");

    assert!(
        !store.path().join(".iwe/journal.ndjson").exists(),
        "no journal path ⇒ no journal record"
    );
    assert!(
        !probe_log.exists(),
        "no journal record ⇒ no trigger invocation"
    );
}

/// An empty-effects write (a mutation that changes nothing, so nothing is
/// committed to the journal) must fire nothing.
#[test]
fn empty_effects_write_fires_no_trigger() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, None, None));
    write(store.path().join("a.md"), "---\nx: 1\n---\n# A\n").expect("seed doc");

    // The document already carries the requested value: the update is
    // computed to be a no-change, so zero writes, zero effects, zero
    // records happen — the write call itself still succeeds.
    let output = run_iwe(store.path(), &["update", "-k", "a", "--set", "x=1"]);
    assert!(output.status.success(), "empty-effects write must succeed: {}", stderr(&output));

    assert!(
        !store.path().join(".iwe/journal.ndjson").exists(),
        "no effects ⇒ no journal record"
    );
    assert!(!probe_log.exists(), "no record ⇒ no trigger invocation");
}

// ---------------------------------------------------------------------------
// Criterion 4 — in-window: the probe observes the presented generation as
// the lock's current_generation, held — it ran inside the commit-lock
// window.
// ---------------------------------------------------------------------------

#[test]
fn probe_runs_inside_the_commit_lock_window() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, None, None));

    let output = run_iwe(store.path(), &["create", "a", "--content", "# A\n"]);
    assert!(output.status.success(), "write must succeed: {}", stderr(&output));

    let lines = probe_lines(&probe_log);
    assert_eq!(lines.len(), 1, "one write, one probe invocation");
    assert_in_window(&lines[0], &store, "probe observation");

    // Cross-check against the crate API from outside the process, after
    // the fact: the lock state file retains the generation across release,
    // so the presented generation must equal what a fresh read reports.
    let lock_path = store.path().join(".iwe/write.lock");
    let persisted = iwe_lock::current_generation(&lock_path)
        .expect("lock state reads after the write")
        .expect("release must not erase the generation");
    assert_eq!(
        persisted.0,
        lines[0]["presented"].as_u64().expect("presented generation"),
        "the presented generation must be the store lock's recorded generation"
    );
}

// ---------------------------------------------------------------------------
// Criterion 5 — fail-open + bounded.
// ---------------------------------------------------------------------------

fn assert_fail_open_write_committed(output: &Output, store: &TempDir, probe_log: &Path) {
    assert!(output.status.success(), "fail-open must not fail the caller: {}", stderr(output));
    assert!(store.path().join("a.md").exists(), "the write must stay committed");
    assert_eq!(journal_records(store).len(), 1, "the journal record must stay committed");
    assert!(!probe_log.exists(), "a command that never runs must never record");
    assert_fail_open_stderr(output);
}

/// `command = "false"`: a child that exits non-zero must not touch the
/// write, the journal, or the caller's result/exit.
#[test]
fn fail_open_on_nonzero_exit_keeps_the_write_committed() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let store = store_with_config(&trigger_config("false", true, None, None));

    let output = run_iwe(store.path(), &["create", "a", "--content", "# A\n"]);
    assert_fail_open_write_committed(&output, &store, &probe_log);
}

/// A failing trigger's notice names its reason: the line carrying kc's
/// KC-* code, even when detail lines follow it, still on the one notice line.
#[test]
fn fail_open_notice_carries_the_triggers_last_stderr_line() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = "echo first >&2; echo 'KC-LOCK-TIMEOUT: timed out waiting' >&2; echo '    waited_ms=30000' >&2; exit 2";
    let store = store_with_config(&trigger_config(command, true, None, None));

    let output = run_iwe(store.path(), &["create", "a", "--content", "# A\n"]);
    assert_fail_open_write_committed(&output, &store, &probe_log);
    let err = stderr(&output);
    let notice: Vec<&str> = err.lines().filter(|l| l.starts_with(FAIL_PREFIX)).collect();
    assert_eq!(notice.len(), 1, "stderr: {err}");
    assert!(notice[0].ends_with(": KC-LOCK-TIMEOUT: timed out waiting"), "notice: {}", notice[0]);
}

/// A command naming a binary that does not exist: the spawn/exec failure
/// must fail open exactly like a non-zero exit.
#[test]
fn fail_open_on_nonexistent_binary_keeps_the_write_committed() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let store =
        store_with_config(&trigger_config("iwe-commit-trigger-no-such-binary-xyz", true, None, None));

    let output = run_iwe(store.path(), &["create", "a", "--content", "# A\n"]);
    assert_fail_open_write_committed(&output, &store, &probe_log);
}

/// A child that sleeps beyond `timeout_seconds`: the write stays
/// committed, the caller returns well under the sleep, and the killed
/// child records nothing.
#[test]
fn fail_open_on_timeout_keeps_the_write_committed_and_returns_well_under_the_sleep() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = format!(
        "{} --sleep-ms {}",
        probe_command(&probe_log),
        TIMEOUT_CASE_SLEEP_MS
    );
    let store = store_with_config(&trigger_config(
        &command,
        true,
        None,
        Some(TIMEOUT_CASE_TIMEOUT_SECS),
    ));

    let started = Instant::now();
    let output = run_iwe(store.path(), &["create", "a", "--content", "# A\n"]);
    let elapsed = started.elapsed();

    assert!(output.status.success(), "fail-open must not fail the caller: {}", stderr(&output));
    assert!(store.path().join("a.md").exists(), "the write must stay committed");
    assert_eq!(journal_records(&store).len(), 1, "the journal record must stay committed");
    assert_fail_open_stderr(&output);
    assert!(
        elapsed < Duration::from_millis(TIMEOUT_CASE_SLEEP_MS - 400),
        "the call must return well under the child's {TIMEOUT_CASE_SLEEP_MS}ms sleep, took {elapsed:?}"
    );
    assert!(
        probe_lines(&probe_log).is_empty(),
        "a child killed by the timeout mid-sleep must not have recorded"
    );
}