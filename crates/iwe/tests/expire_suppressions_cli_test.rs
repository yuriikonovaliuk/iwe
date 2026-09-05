// Test-builder, working independently of Developer on m6-b task 4a ("Port
// expire-suppressions as unconditional always-checker under validate=full")
// — see roles/delivery/test-builder. No Developer implementation was read
// to write this file. At the time this file was written,
// `crates/diwe/src/checkers/expire_suppressions.rs` did not exist yet in
// this worktree — only `pub mod checkers;` had landed in
// `crates/diwe/src/lib.rs`, which this file deliberately does not open or
// otherwise inspect. These tests are therefore written against the
// checker's *observable* contract (AC2/AC3/AC4/AC6), exercised the same
// way `cli_write_gate_test.rs` exercises the CLI's existing full-scope
// write gate — a `.iwe/config.toml` at `[transactions] validate = "full"`
// and an assertion on the CLI process's exit status / stderr / what
// landed on disk. They will fail (the write unexpectedly succeeds) until
// the checker is implemented and registered in
// `crates/diwe/src/validating_transaction.rs`'s always-checkers list; note
// this plainly rather than papering over it.
//
// Fixture format and the expiry rule itself are read independently from
// the live store's actual enforcement mechanism (not invented, not copied
// from the task contract): `~/projects/iwe-memory/scripts/checkers/
// expire-suppressions.py` (invoked by `expire-suppressions.sh`, in turn
// called from `.githooks/pre-commit`, "must run before iwe schema
// validate so an expired bypass blocks the commit"). Verified directly by
// reading that script:
//   - Line format: `# suppress: <name> until=<YYYY-MM-DD> reason="<text>"`,
//     matched with
//     `^\s*#\s*suppress:\s*(?P<name>[A-Za-z0-9_-]+)\s+until\s*=\s*(?P<until>\d{4}-\d{2}-\d{2})\s+reason\s*=\s*"(?P<reason>[^"]*)"\s*$`
//     — these are TOML comments the schema engine ignores; the enforcement
//     script alone gives them meaning (confirmed by `.iwe/config.toml`'s
//     own `[suppressions]` header comment in the live store).
//   - Expiry test is `until < today` (strict): a suppression expiring
//     *today* is not yet expired.
//   - Independent finding for AC5: the script reads no marker file and no
//     mtime anywhere — the only state is the literal `until=` date text
//     living inside `.iwe/config.toml`'s own content. Flag for 4b/6: if
//     the Rust port instead keyed expiry off a *file's* mtime, that would
//     newly introduce a git-mtime-loss risk the original script never
//     had, since git does not preserve mtimes but does preserve file
//     content byte for byte.
//   - The live store's `.iwe/config.toml` carries zero live
//     `[checkers.*]` entries tied to this mechanism as of this writing —
//     confirmed by reading the file, not assumed.
//
// "CLI path and MCP path" (AC2): both binaries build their validating
// backend through the exact same, already-shipped call —
// `ValidatingTransaction::for_config` — whose own doc comment says: "The
// one construction both binaries share, so a store gated for the MCP
// server is gated for the CLI too" (`crates/diwe/src/validating_
// transaction.rs`). `crates/iwe/src/new.rs`'s `validating_backend` and
// `crates/iwec/src/lib.rs`'s `IweServer::validating_backend` both call it.
// This file is the CLI half of that pair; see
// `crates/iwec/tests/expire_suppressions_mcp_test.rs` for the MCP half.

use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};

use diwe::config::{Configuration, TransactionOptions, ValidationScope};
use tempfile::TempDir;

/// A store at `scope`, with the given raw `# suppress:` lines appended
/// verbatim to a generated `.iwe/config.toml` — exactly how the live
/// store carries them (comments the TOML parser ignores). No
/// `[checkers.*]` entry is ever added: AC3 requires the ported rule to
/// need no config toggle of its own.
fn store(scope: ValidationScope, suppress_lines: &[&str]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe")).unwrap();

    let config = Configuration {
        transactions: TransactionOptions {
            validate: scope,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut toml_text = toml::to_string(&config).unwrap();
    toml_text.push('\n');
    for line in suppress_lines {
        toml_text.push_str(line);
        toml_text.push('\n');
    }
    write(base.join(".iwe/config.toml"), toml_text).unwrap();
    dir
}

fn iwe(work_dir: &Path, args: &[&str]) -> Output {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        .current_dir(work_dir)
        .output()
        .expect("run iwe")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------
// AC4 (both directions) + AC9 bullet 1 (expiry decision logic against a
// fixture mirroring the real script: `until` in the past).
// ---------------------------------------------------------------------

#[test]
fn full_scope_refuses_a_write_with_one_expired_suppression() {
    let dir = store(
        ValidationScope::Full,
        &[r#"# suppress: stale-note-count until=2026-01-01 reason="waiting on cleanup""#],
    );
    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);

    assert!(
        !output.status.success(),
        "an expired suppression must block the commit"
    );
    let message = stderr(&output);
    assert!(
        message.contains("stale-note-count"),
        "refusal names the expired suppression, got: {message}"
    );
    assert!(
        !dir.path().join("new.md").exists(),
        "the write must not land"
    );
}

#[test]
fn full_scope_accepts_a_write_with_no_expired_suppressions() {
    let dir = store(
        ValidationScope::Full,
        &[r#"# suppress: still-fine until=2099-01-01 reason="long enough runway""#],
    );
    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(dir.path().join("new.md").exists());
}

/// Boundary from the real script: `until < today` is strict, so a
/// suppression expiring *today* is not yet expired.
#[test]
fn full_scope_accepts_a_write_with_a_suppression_expiring_today() {
    let today = chrono::Local::now().date_naive();
    let line = format!(r#"# suppress: expires-today until={today} reason="edge of the window""#);
    let dir = store(ValidationScope::Full, &[&line]);
    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);

    assert!(
        output.status.success(),
        "until == today must not be treated as expired: {}",
        stderr(&output)
    );
}

// ---------------------------------------------------------------------
// AC6 + AC9 bullet 3: multi-violation reporting shape. N expired
// suppressions must all be named, not just the first; an unexpired
// suppression mixed in among them must not be reported.
// ---------------------------------------------------------------------

#[test]
fn full_scope_reports_every_expired_suppression_not_just_the_first() {
    let dir = store(
        ValidationScope::Full,
        &[
            r#"# suppress: alpha-check until=2026-01-01 reason="a""#,
            r#"# suppress: bravo-check until=2026-02-14 reason="b""#,
            r#"# suppress: charlie-check until=2026-08-30 reason="c""#,
        ],
    );
    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);

    assert!(!output.status.success());
    let message = stderr(&output);
    for name in ["alpha-check", "bravo-check", "charlie-check"] {
        assert!(
            message.contains(name),
            "refusal must name every expired suppression, missing {name}, got: {message}"
        );
    }
    assert!(!dir.path().join("new.md").exists());
}

#[test]
fn full_scope_reports_only_the_expired_entries_among_a_mix() {
    let dir = store(
        ValidationScope::Full,
        &[
            r#"# suppress: expired-one until=2026-01-01 reason="a""#,
            r#"# suppress: still-valid until=2099-01-01 reason="b""#,
        ],
    );
    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);

    assert!(!output.status.success(), "the expired entry alone must block");
    let message = stderr(&output);
    assert!(message.contains("expired-one"), "got: {message}");
    assert!(
        !message.contains("still-valid"),
        "an unexpired suppression must not be reported as a violation, got: {message}"
    );
}

// ---------------------------------------------------------------------
// AC3: no config toggle — the fixture above carries zero `[checkers.*]`
// entries, yet the checker still fires purely because scope is "full".
// ---------------------------------------------------------------------

#[test]
fn fires_with_zero_configured_checkers_no_toggle_required() {
    let dir = store(
        ValidationScope::Full,
        &[r#"# suppress: only-one until=2020-01-01 reason="old""#],
    );
    let config_text = read_to_string(dir.path().join(".iwe/config.toml")).unwrap();
    assert!(
        !config_text.contains("[checkers."),
        "fixture must carry no configured checkers"
    );

    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);
    assert!(
        !output.status.success(),
        "must still refuse with no [checkers.*] section configured"
    );
}

// ---------------------------------------------------------------------
// AC2's "on validate=full" qualifier: the same expired suppression must
// not gate a write when the transaction backend is not asked to validate
// at all (`[transactions] validate` left at its default, "none" — AB9's
// no-op passthrough). Inferred from the existing, already-shipped
// convention every other checker in this codebase already follows
// (`ValidatingTransaction::failing_checker_reports` only runs when
// `self.scope == ValidationScope::Full`), not from Developer's
// unimplemented module.
// ---------------------------------------------------------------------

#[test]
fn without_full_scope_the_same_expired_suppression_does_not_block() {
    let dir = store(
        ValidationScope::None,
        &[r#"# suppress: stale-note-count until=2026-01-01 reason="waiting on cleanup""#],
    );
    let output = iwe(dir.path(), &["create", "new", "--content", "# New\n"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(dir.path().join("new.md").exists());
}
