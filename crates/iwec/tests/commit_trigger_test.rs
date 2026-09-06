//! MCP half of the black-box test suite for the `[commit]` trigger
//! (`efforts/knowledge-compositor/m6-b-cutover-preconditions/
//! 6-t1-iwe-commit-trigger`), covering the acceptance criteria's "iwec
//! write-tool" and "iwec `iwe_tx_commit`" once-per-record legs (criterion
//! 2), the empty-effects no-trigger leg (criterion 3), and the
//! open-transaction regression (criterion 6: a default-handle commit
//! fires the trigger while an explicit handle's staging stays intact).
//! Written from the task contract only — Developer's implementation of the
//! trigger is not read here.
//!
//! The real `iwec` binary is spawned over its HTTP transport against a
//! scratch store, exactly as `cli_mcp_write_scope_parity_test.rs` does;
//! the store's `.iwe/config.toml` carries `[commit]`/`[journal]` and, for
//! the transaction legs, `[transactions] validate`. The probe is this
//! package's own `iwe_commit_probe` `[[bin]]` fixture, sharing its source
//! with the CLI half (`crates/iwe/tests/commit_trigger_test.rs`); every
//! probe line must also satisfy the in-window invariants (criterion 4).
//!
//! The CLI halves of criteria 2–5 live in the CLI half's file.

use std::fs::{self, create_dir_all, write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceError, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;

/// This package's probe fixture binary (a `[[bin]]`, so `cargo test`
/// guarantees it is built before these tests run).
const PROBE: &str = env!("CARGO_BIN_EXE_iwe_commit_probe");

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn wait_until_listening(addr: &str) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server did not start listening on {addr}");
}

/// Scratch store whose `.iwe/config.toml` the test writes (never a real
/// store).
fn store_with_config(config_toml: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    create_dir_all(dir.path().join(".iwe")).expect("mkdir .iwe");
    write(dir.path().join(".iwe/config.toml"), config_toml).expect("write config");
    dir
}

/// The shell command string for `[commit] command`: this package's probe
/// binary appending to `probe_log`. Paths are absolute and shell-safe
/// (asserted), since the trigger is shell-spawned.
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

fn trigger_config(command: &str, journal: bool, validate: Option<&str>) -> String {
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
    out
}

/// Spawn `iwec` over HTTP on an ephemeral port, cwd set to the scratch
/// store (its `.iwe/config.toml` is the config the server loads).
fn spawn_server(dir: &TempDir) -> (u16, ServerProcess) {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_iwec"))
        .arg("--transport")
        .arg("http")
        .arg("--port")
        .arg(port.to_string())
        // Hermeticity: no ambient transactions/trigger env may leak in.
        // The trigger env vars in particular must not be inherited
        // pre-set from the test runner (the trigger supplies its own
        // values; the in-window assertions then prove it).
        .env_remove("IWE_TRANSACTIONS_DENY")
        .env_remove("IWE_TRANSACTIONS_ALLOW")
        .env_remove("IWE_STORE_ROOT")
        .env_remove("IWE_COMMIT_LOCK_GENERATION")
        .env_remove("IWE_TEST_LOCK_FENCING_DELAY_MS")
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn iwec");
    (port, ServerProcess { child })
}

async fn connect(addr: &str) -> RunningService<RoleClient, ClientInfo> {
    wait_until_listening(addr).await;
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwe-commit-trigger-test-client", "0.0.1"),
    );
    client_info
        .serve(transport)
        .await
        .expect("client to connect")
}

async fn call_tool(
    client: &RunningService<RoleClient, ClientInfo>,
    name: &'static str,
    arguments: serde_json::Value,
) -> Result<rmcp::model::CallToolResult, ServiceError> {
    let args = arguments
        .as_object()
        .cloned()
        .unwrap_or_else(|| serde_json::Map::new());
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args))
        .await
}

fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
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

/// Criterion 4's in-window invariants, asserted on every probe line a live
/// trigger produces.
fn assert_in_window(root: &Value, store: &TempDir, where_: &str) {
    let observed = PathBuf::from(root["root"].as_str().expect("probe records IWE_STORE_ROOT"));
    let canonical = store.path().canonicalize().expect("store path canonicalizes");
    assert!(
        observed.canonicalize().map(|c| c == canonical).unwrap_or(false)
            || observed == store.path(),
        "{where_}: probe root {observed:?} is not the store {canonical:?}"
    );
    let presented = root["presented"].as_u64().expect("probe records the decimal generation");
    assert_eq!(
        root["current"].as_u64(),
        Some(presented),
        "{where_}: presented generation must equal the lock's current_generation"
    );
    assert_eq!(
        root["held"].as_bool(),
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

// ---------------------------------------------------------------------------
// Criterion 2 — once per record: iwec write-tool and iwec `iwe_tx_commit`
// each produce exactly one probe line and one journal line.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn iwe_create_write_tool_commits_one_journal_line_and_one_probe_line() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, None));
    let (port, _server) = spawn_server(&store);
    let client = connect(&format!("127.0.0.1:{port}")).await;

    let created = call_tool(
        &client,
        "iwe_create",
        json!({"key": "notes/new", "content": "# New\n"}),
    )
    .await;
    assert!(created.is_ok(), "iwe_create must succeed: {created:?}");

    assert_eq!(journal_records(&store).len(), 1, "one write-tool commit, one journal line");
    assert_eq!(probe_lines(&probe_log).len(), 1, "one write-tool commit, one trigger invocation");
    assert_all_probe_lines_in_window(&probe_log, &store, "iwe_create write-tool");

    client.cancel().await.expect("client to disconnect");
}

#[tokio::test]
async fn iwe_tx_commit_commits_one_journal_line_and_one_probe_line() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, Some("full")));
    let (port, _server) = spawn_server(&store);
    let client = connect(&format!("127.0.0.1:{port}")).await;

    let begun = call_tool(&client, "iwe_tx_begin", json!({})).await;
    assert!(!begun.expect("tx_begin").is_error.unwrap_or(false));
    let created = call_tool(
        &client,
        "iwe_create",
        json!({"key": "notes/new", "content": "# New\n"}),
    )
    .await;
    assert!(!created.expect("write").is_error.unwrap_or(false));
    let committed = call_tool(&client, "iwe_tx_commit", json!({})).await;
    let committed = committed.expect("tx_commit");
    assert!(!committed.is_error.unwrap_or(false), "commit must succeed: {committed:?}");

    assert_eq!(journal_records(&store).len(), 1, "one tx commit, one journal line");
    assert_eq!(probe_lines(&probe_log).len(), 1, "one tx commit, one trigger invocation");
    assert_all_probe_lines_in_window(&probe_log, &store, "iwe_tx_commit");

    client.cancel().await.expect("client to disconnect");
}

// ---------------------------------------------------------------------------
// Criterion 3 — no record ⇒ no trigger (this suite's empty-effects leg).
// ---------------------------------------------------------------------------

/// Committing an open transaction that staged nothing produces no journal
/// record — and so must not fire the trigger, while still committing
/// cleanly.
#[tokio::test]
async fn empty_transaction_commit_records_nothing_and_fires_no_trigger() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, Some("full")));
    let (port, _server) = spawn_server(&store);
    let client = connect(&format!("127.0.0.1:{port}")).await;

    let begun = call_tool(&client, "iwe_tx_begin", json!({})).await;
    assert!(!begun.expect("tx_begin").is_error.unwrap_or(false));
    let committed = call_tool(&client, "iwe_tx_commit", json!({})).await;
    let committed = committed.expect("tx_commit");
    assert!(
        !committed.is_error.unwrap_or(false),
        "an empty transaction must still commit cleanly: {committed:?}"
    );
    assert!(result_text(&committed).contains("committed"));

    assert!(
        !store.path().join(".iwe/journal.ndjson").exists(),
        "no effects ⇒ no journal record"
    );
    assert!(!probe_log.exists(), "no record ⇒ no trigger invocation");

    client.cancel().await.expect("client to disconnect");
}

// ---------------------------------------------------------------------------
// Criterion 6 — open-transaction regression: a default-handle commit fires
// the trigger; an explicit handle's staged state stays intact across it.
// ---------------------------------------------------------------------------

/// Two transactions open at once: an explicit `h1` with staged content and
/// the default handle with its own staged content. The default-handle
/// commit fires the trigger; `h1`'s staging — untouched by that commit —
/// still commits afterward, for exactly two journal lines and two trigger
/// invocations in total.
#[tokio::test]
async fn default_handle_commit_fires_the_trigger_and_an_explicit_handles_staging_stays_intact() {
    let log_dir = TempDir::new().expect("tempdir");
    let probe_log = log_dir.path().join("probe.log");
    let command = probe_command(&probe_log);
    let store = store_with_config(&trigger_config(&command, true, Some("full")));
    let (port, _server) = spawn_server(&store);
    let client = connect(&format!("127.0.0.1:{port}")).await;

    // Open the explicit handle first and stage a write into it.
    let begun = call_tool(&client, "iwe_tx_begin", json!({"handle": "h1"})).await;
    assert!(!begun.expect("tx_begin h1").is_error.unwrap_or(false));
    let staged = call_tool(
        &client,
        "iwe_create",
        json!({"key": "notes/staged", "content": "# Staged\n", "handle": "h1"}),
    )
    .await;
    assert!(!staged.expect("staged write").is_error.unwrap_or(false));

    // Then the default handle, with its own staged write.
    let begun = call_tool(&client, "iwe_tx_begin", json!({})).await;
    assert!(!begun.expect("tx_begin default").is_error.unwrap_or(false));
    let implicit = call_tool(
        &client,
        "iwe_create",
        json!({"key": "notes/implicit", "content": "# Implicit\n"}),
    )
    .await;
    assert!(!implicit.expect("implicit write").is_error.unwrap_or(false));

    // The default-handle commit must fire the trigger (one journal line so
    // far).
    let committed = call_tool(&client, "iwe_tx_commit", json!({})).await;
    let committed = committed.expect("default tx_commit");
    assert!(!committed.is_error.unwrap_or(false), "default commit must succeed: {committed:?}");
    assert_eq!(journal_records(&store).len(), 1, "default commit, one journal line");
    assert_eq!(probe_lines(&probe_log).len(), 1, "default commit fires the trigger once");
    assert_all_probe_lines_in_window(&probe_log, &store, "default-handle commit");

    // The explicit handle's staging must have survived the default commit:
    // committing `h1` still lands its staged document.
    let committed = call_tool(&client, "iwe_tx_commit", json!({"handle": "h1"})).await;
    let committed = committed.expect("explicit tx_commit");
    assert!(
        !committed.is_error.unwrap_or(false),
        "the explicit handle must still be committable after the default commit: {committed:?}"
    );
    assert!(store.path().join("notes/implicit.md").exists());
    assert!(store.path().join("notes/staged.md").exists());
    assert_eq!(journal_records(&store).len(), 2, "two commits, two journal lines");
    assert_eq!(probe_lines(&probe_log).len(), 2, "two trigger-producing commits");

    client.cancel().await.expect("client to disconnect");
}