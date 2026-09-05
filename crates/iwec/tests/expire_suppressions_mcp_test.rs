// Test-builder, working independently of Developer on m6-b task 4a ("Port
// expire-suppressions as unconditional always-checker under validate=full")
// — see roles/delivery/test-builder. No Developer implementation was read
// to write this file. At the time this file was written,
// `crates/diwe/src/checkers/expire_suppressions.rs` did not exist yet in
// this worktree — only `pub mod checkers;` had landed in
// `crates/diwe/src/lib.rs`, which this file deliberately does not open or
// otherwise inspect. These tests are written against the checker's
// *observable* contract (AC2/AC4/AC6) only, the MCP half of the pair with
// `crates/iwe/tests/expire_suppressions_cli_test.rs` — see that file for
// the full citation of the fixture format/semantics (verified
// independently against `~/projects/iwe-memory/scripts/checkers/
// expire-suppressions.py`) and the AC5 marker-mtime finding.
//
// "CLI path and MCP path" (AC2): both binaries build their validating
// backend through the exact same, already-shipped
// `ValidatingTransaction::for_config` — "the one construction both
// binaries share, so a store gated for the MCP server is gated for the
// CLI too" (`crates/diwe/src/validating_transaction.rs`'s own doc
// comment). This file drives that construction the same way
// `validating_write_gate_test.rs` already does for the MCP path's
// full-scope schema gate: through `IweServer::new` + a real MCP
// `iwe_create` tool call over `Fixture`, not by spinning up a second,
// separate server-process end-to-end harness.
//
// These tests will fail (the write unexpectedly succeeds) until the
// checker is implemented and registered in
// `crates/diwe/src/validating_transaction.rs`'s always-checkers list;
// noted plainly rather than papered over.

use std::fs::create_dir_all;

use diwe::config::{Configuration, TransactionOptions, ValidationScope};
use serde_json::json;
use tempfile::TempDir;

use crate::fixture::Fixture;

/// A store whose `.iwe/config.toml` on disk carries the given raw
/// `# suppress:` lines verbatim — read by the checker the same way the
/// real `expire-suppressions.py` reads them (a raw text scan, independent
/// of the parsed `Configuration`, which has no field for them at all) —
/// plus the in-memory `Configuration` `IweServer::new` needs to decide
/// the validation scope. No `[checkers.*]` entry (AC3: no config toggle
/// of this checker's own).
async fn store_and_fixture(suppress_lines: &[&str]) -> (TempDir, Fixture) {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    create_dir_all(base.join(".iwe")).unwrap();

    let config = Configuration {
        transactions: TransactionOptions {
            validate: ValidationScope::Full,
        },
        ..Default::default()
    };
    // Hand-written instead of `toml::to_string` (iwec has no `toml`
    // dependency of its own): the only fields the checker or the
    // validating backend need from this file are `[transactions]
    // validate = "full"` and the raw `# suppress:` comment lines.
    let mut toml_text = String::from("[transactions]\nvalidate = \"full\"\n");
    for line in suppress_lines {
        toml_text.push_str(line);
        toml_text.push('\n');
    }
    std::fs::write(base.join(".iwe/config.toml"), toml_text).unwrap();

    let f = Fixture::with_path(base.to_str().unwrap(), config).await;
    (dir, f)
}

// AC4 (both directions), AC9 bullet 2 (checker firing on an MCP-path
// full-validation commit).
#[tokio::test]
async fn full_scope_refuses_a_write_with_one_expired_suppression() {
    let (dir, f) = store_and_fixture(&[
        r#"# suppress: stale-note-count until=2026-01-01 reason="waiting on cleanup""#,
    ])
    .await;

    let result = f
        .try_call_tool("iwe_create", json!({"key": "new", "content": "# New\n"}))
        .await;
    let message = result
        .expect_err("an expired suppression must block the commit")
        .to_string();
    assert!(
        message.contains("stale-note-count"),
        "refusal names the expired suppression, got: {message}"
    );
    assert!(!dir.path().join("new.md").exists(), "the write must not land");
}

#[tokio::test]
async fn full_scope_accepts_a_write_with_no_expired_suppressions() {
    let (dir, f) = store_and_fixture(&[
        r#"# suppress: still-fine until=2099-01-01 reason="long enough runway""#,
    ])
    .await;

    let result = f
        .call_tool("iwe_create", json!({"key": "new", "content": "# New\n"}))
        .await;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    assert!(dir.path().join("new.md").exists());
}

// AC6 + AC9 bullet 3: multi-violation reporting shape, exercised the same
// way as the CLI half.
#[tokio::test]
async fn full_scope_reports_every_expired_suppression_not_just_the_first() {
    let (dir, f) = store_and_fixture(&[
        r#"# suppress: alpha-check until=2026-01-01 reason="a""#,
        r#"# suppress: bravo-check until=2026-02-14 reason="b""#,
        r#"# suppress: charlie-check until=2026-08-30 reason="c""#,
    ])
    .await;

    let result = f
        .try_call_tool("iwe_create", json!({"key": "new", "content": "# New\n"}))
        .await;
    let message = result.expect_err("all three are expired").to_string();
    for name in ["alpha-check", "bravo-check", "charlie-check"] {
        assert!(
            message.contains(name),
            "refusal must name every expired suppression, missing {name}, got: {message}"
        );
    }
    assert!(!dir.path().join("new.md").exists());
}
