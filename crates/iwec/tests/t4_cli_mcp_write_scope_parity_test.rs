//! Acceptance-criteria test (e) for
//! `efforts/multi-agent-orchestration/implementation/mind-write-separation/t4-cli-parity`.
//!
//! Written from the task's persisted contract only, without reading or
//! waiting on Developer's implementation of the same task (independence
//! requirement of the Test-builder role). The contract's Shared surface
//! leaves the CLI entrypoint that performs a scoped write "NOT pinned"
//! and directs Developer to name it and write that name back into the
//! contract before Test-builder starts this test; at the time this file
//! was written no such update had landed in the graph, so per the task
//! instructions this was resolved by black-box discovery rather than by
//! reading Developer's in-progress source:
//!
//!   - `iwe --help` (built binary, run as an external caller would) lists
//!     `create` among the top-level subcommands, and `iwe create --help`
//!     describes exactly a scoped, key-addressed write ("Content mode ...
//!     writes the document you pass ... The key is required").
//!   - Running the *current*, pre-fix binary against a temp project with
//!     `IWE_TRANSACTIONS_ALLOW=mind/**` set and a key outside `mind/**`
//!     landed the write anyway (exit 0, file created) -- independently
//!     confirming, behaviorally rather than by inspection of Developer's
//!     work, that `iwe create` is the divergent write path the contract
//!     describes and is fixing.
//!
//! `iwe create` is therefore this test's best-effort discovery of the
//! entrypoint, not a confirmed name -- flagged as such in the handback.
//!
//! The MCP-side counterpart tool name, `iwe_create`, is not a guess: it
//! is read off `crates/iwec/tests/validating_write_gate_test.rs` and
//! `crates/iwec/tests/write_permission_test.rs`, both already-landed
//! (t1-t3 era) test files documenting existing, shared MCP surface, not
//! this task's in-progress implementation. Likewise `crates/iwec/src/
//! main.rs`'s call to `diwe::config::load_config()` before constructing
//! `IweServer` is pre-existing wiring from earlier tasks, not something
//! Developer is editing for t4.
//!
//! Both binaries are driven as real subprocesses (`iwe` via
//! `std::process::Command`, `iwec` via `rmcp`'s `TokioChildProcess`
//! stdio transport) rather than in-process/mocked, per the contract's
//! preference: this test's whole point is proving parity as an external
//! caller would experience it.
//!
//! `ValidationFailure::WriteScopeDenied`'s Display text ("<listed>
//! rejected: refused by the configured write scope", from
//! `crates/diwe/src/validating_transaction.rs`) is the one substring
//! that identifies *this* failure variant rather than `Conflict` /
//! `LockTimeout` / `LockStale` / `Violations` / `Config`. Across the MCP
//! subprocess boundary only that rendered text is observable (no Rust
//! type crosses a JSON-RPC error response), so the MCP-side assertion
//! below matches on that exact substring plus the denied key -- the
//! subprocess-black-box equivalent of the `matches!(err,
//! ValidationFailure::WriteScopeDenied(keys) if keys.contains(&key))`
//! check `crates/diwe/tests/write_scope_check_test.rs` uses in-process.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rmcp::model::*;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tempfile::TempDir;

// Same resolved deny/allow config for both binaries: only `mind/**` is
// writable. Same target keys for both binaries: `DENIED_KEY` sits
// outside the allow-list, `ALLOWED_KEY` sits inside it.
const ALLOW_ENV: &str = "mind/**";
const DENIED_KEY: &str = "other/t4-parity-denied";
const ALLOWED_KEY: &str = "mind/t4-parity-allowed";
const DENIED_CONTENT: &str = "---\ntype: note\n---\n# Denied\n\nBody.\n";
const ALLOWED_CONTENT: &str = "---\ntype: note\n---\n# Allowed\n\nBody.\n";

/// `ValidatingTransaction::for_config` (`crates/diwe/src/
/// validating_transaction.rs`) -- the exact constructor the contract's
/// AC (a) asks whether the CLI's write path shares with MCP's -- returns
/// `None`, short-circuiting past the deny/allow scope check entirely,
/// whenever `[transactions] validate` resolves to its default `none`.
/// `IWE_TRANSACTIONS_ALLOW`/`_DENY`'s env overlay only ever touches
/// `deny`/`allow`, never `validate` (`apply_transactions_env_overlay` in
/// `crates/diwe/src/config.rs`), so a project with no config file at all
/// would resolve `validate: none` and never reach the scope check on
/// either binary regardless of the env overlay -- which would make this
/// test pass by construction (neither binary enforcing anything) rather
/// than by proving the two binaries agree on live enforcement. Every
/// fixture here therefore also pins `validate = "full"` in
/// `.iwe/config.toml`, the same non-default scope
/// `crates/iwec/tests/validating_write_gate_test.rs` and `crates/iwe/
/// tests/cli_write_gate_test.rs` already use to exercise this
/// already-landed, shared (non-t4) code path.
fn write_project_config(dir: &Path) {
    let iwe_dir = dir.join(".iwe");
    fs::create_dir_all(&iwe_dir).expect("create .iwe dir");
    fs::write(
        iwe_dir.join("config.toml"),
        "version = 3\n\n[transactions]\nvalidate = \"full\"\n",
    )
    .expect("write .iwe/config.toml");
}

/// Locates a workspace binary the same way `crates/iwe/tests/common.rs`'s
/// `get_iwe_binary_path` does (walking up from this crate's manifest dir
/// to the workspace root, then into `target/{debug,release}`), but
/// generalized to the binary name so it can find either `iwe` or `iwec`.
/// Kept local to this file rather than importing the `iwe` crate's test
/// module, which integration tests of a different crate cannot reach.
fn workspace_binary(name: &str) -> PathBuf {
    let binary_name = format!("{name}{}", env::consts::EXE_SUFFIX);

    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
        let base = PathBuf::from(target_dir);
        if let Some(path) = ["debug", "release"]
            .into_iter()
            .map(|profile| base.join(profile).join(&binary_name))
            .find(|p| p.exists())
        {
            return path;
        }
    }

    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !dir.join("Cargo.toml").exists() || !dir.join("crates").exists() {
        assert!(
            dir.pop(),
            "could not find workspace root above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
    dir.push("target");

    ["debug", "release"]
        .into_iter()
        .map(|profile| dir.join(profile).join(&binary_name))
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("could not find {binary_name} binary; run `cargo build` first"))
}

/// Runs `iwe create <key> --content <content>` as a real subprocess, cwd
/// set to `dir`, with the given `IWE_TRANSACTIONS_ALLOW` value resolved
/// the same way `load_config()`'s env overlay resolves it for `iwec`
/// (`IWE_TRANSACTIONS_DENY` explicitly cleared so no ambient value from
/// the test-runner's own environment leaks in).
fn run_cli_create(dir: &Path, allow_env: &str, key: &str, content: &str) -> Output {
    Command::new(workspace_binary("iwe"))
        .args(["create", key, "--content", content])
        .current_dir(dir)
        .env("IWE_TRANSACTIONS_ALLOW", allow_env)
        .env_remove("IWE_TRANSACTIONS_DENY")
        .output()
        .expect("spawn iwe CLI subprocess")
}

/// Spawns `iwec` as a real subprocess over its stdio MCP transport, cwd
/// set to `dir` with the same `IWE_TRANSACTIONS_ALLOW` overlay, and calls
/// `iwe_create` with `key`/`content`. Returns the raw
/// `Result<CallToolResult, ServiceError>` so callers can inspect either
/// the success payload or the error text.
async fn mcp_create(
    dir: &Path,
    allow_env: &str,
    key: &str,
    content: &str,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(workspace_binary("iwec")).configure(|cmd| {
            cmd.current_dir(dir)
                .env("IWE_TRANSACTIONS_ALLOW", allow_env)
                .env_remove("IWE_TRANSACTIONS_DENY");
        }),
    )
    .expect("spawn iwec MCP subprocess");

    let client = ().serve(transport).await.expect("MCP client handshake");

    let mut arguments = serde_json::Map::new();
    arguments.insert("key".to_string(), serde_json::json!(key));
    arguments.insert("content".to_string(), serde_json::json!(content));
    let params = CallToolRequestParams::new("iwe_create".to_string()).with_arguments(arguments);

    let result = client.call_tool(params).await;
    let _ = client.cancel().await;
    result
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn doc_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.md"))
}

// ---------------------------------------------------------------------
// Denied key: outside `mind/**`. Fails on both, distinguishably.
// ---------------------------------------------------------------------

#[tokio::test]
async fn denied_key_write_fails_on_the_cli() {
    let dir = TempDir::new().unwrap();
    write_project_config(dir.path());

    let output = run_cli_create(dir.path(), ALLOW_ENV, DENIED_KEY, DENIED_CONTENT);

    assert!(
        !output.status.success(),
        "a write to a key outside the allow-list must not report success; \
         stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        output.status.code() != Some(0) && !stderr.is_empty(),
        "the rejection must be distinguishable -- a nonzero exit code and/or \
         non-empty stderr, not silently swallowed: exit={:?}, stderr={:?}",
        output.status.code(),
        stderr
    );
    assert!(
        !doc_path(dir.path(), DENIED_KEY).exists(),
        "a denied write must not land on disk"
    );
}

#[tokio::test]
async fn denied_key_write_fails_on_the_mcp_server_with_write_scope_denied() {
    let dir = TempDir::new().unwrap();
    write_project_config(dir.path());

    let result = mcp_create(dir.path(), ALLOW_ENV, DENIED_KEY, DENIED_CONTENT).await;

    let message = result
        .expect_err("a write to a key outside the allow-list must be refused")
        .to_string();
    assert!(
        message.contains("refused by the configured write scope"),
        "the failure must be `ValidationFailure::WriteScopeDenied` specifically \
         (its distinguishing Display text), not some other validation failure: {message}"
    );
    assert!(
        message.contains(DENIED_KEY),
        "the refusal must name the denied key: {message}"
    );
    assert!(
        !doc_path(dir.path(), DENIED_KEY).exists(),
        "a denied write must not land on disk"
    );
}

// ---------------------------------------------------------------------
// Permitted key: inside `mind/**`. Succeeds on both, under the same
// resolved config that just refused the key above.
// ---------------------------------------------------------------------

#[tokio::test]
async fn permitted_key_write_succeeds_on_the_cli() {
    let dir = TempDir::new().unwrap();
    write_project_config(dir.path());

    let output = run_cli_create(dir.path(), ALLOW_ENV, ALLOWED_KEY, ALLOWED_CONTENT);

    assert!(
        output.status.success(),
        "a write to an allow-listed key must succeed: exit={:?}, stderr={}",
        output.status.code(),
        stderr_of(&output)
    );
    // Content-mode's exact on-disk formatting (e.g. whether a blank line
    // separates frontmatter from the heading) is a normalization detail
    // orthogonal to write-scope enforcement, so this checks substance,
    // not byte-for-byte equality.
    let on_disk = std::fs::read_to_string(doc_path(dir.path(), ALLOWED_KEY))
        .expect("the permitted write must land on disk");
    assert!(on_disk.contains("# Allowed") && on_disk.contains("Body."), "{on_disk}");
}

#[tokio::test]
async fn permitted_key_write_succeeds_on_the_mcp_server() {
    let dir = TempDir::new().unwrap();
    write_project_config(dir.path());

    let result = mcp_create(dir.path(), ALLOW_ENV, ALLOWED_KEY, ALLOWED_CONTENT).await;

    let call_result = result.expect("a write to an allow-listed key must succeed over MCP");
    assert!(
        !call_result.is_error.unwrap_or(false),
        "the MCP call must not report a tool-level error: {call_result:?}"
    );
    let on_disk = std::fs::read_to_string(doc_path(dir.path(), ALLOWED_KEY))
        .expect("the permitted write must land on disk");
    assert!(on_disk.contains("# Allowed") && on_disk.contains("Body."), "{on_disk}");
}

// ---------------------------------------------------------------------
// One same-process cross-check: identical resolved config, both target
// keys, both binaries, in a single test -- the shape the contract's
// bullet describes literally ("one test driving the iwe CLI binary and
// one driving the iwec MCP binary, same resolved deny/allow ... same
// target keys").
// ---------------------------------------------------------------------

#[tokio::test]
async fn cli_and_mcp_agree_on_both_keys_under_the_same_resolved_scope() {
    let cli_dir = TempDir::new().unwrap();
    let mcp_dir = TempDir::new().unwrap();
    write_project_config(cli_dir.path());
    write_project_config(mcp_dir.path());

    let cli_denied = run_cli_create(cli_dir.path(), ALLOW_ENV, DENIED_KEY, DENIED_CONTENT);
    let cli_allowed = run_cli_create(cli_dir.path(), ALLOW_ENV, ALLOWED_KEY, ALLOWED_CONTENT);
    let mcp_denied = mcp_create(mcp_dir.path(), ALLOW_ENV, DENIED_KEY, DENIED_CONTENT).await;
    let mcp_allowed = mcp_create(mcp_dir.path(), ALLOW_ENV, ALLOWED_KEY, ALLOWED_CONTENT).await;

    assert!(
        !cli_denied.status.success() && mcp_denied.is_err(),
        "both binaries must refuse the same denied key under the same config: \
         cli_exit={:?}, mcp_result_is_ok={}",
        cli_denied.status.code(),
        mcp_denied.is_ok()
    );
    assert!(
        cli_allowed.status.success() && mcp_allowed.is_ok(),
        "both binaries must accept the same permitted key under the same config: \
         cli_exit={:?}, cli_stderr={}, mcp_result_is_ok={}",
        cli_allowed.status.code(),
        stderr_of(&cli_allowed),
        mcp_allowed.is_ok()
    );
}
