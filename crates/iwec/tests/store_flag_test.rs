// `--store <path>`: an explicit flag that overrides the process's
// cwd-based store discovery, uniformly across the http and stdio
// transports.
// `efforts/crew-and-iwe-memory-footprint-reduction/iwec-shared-daemon/
// t1-store-path-flag`.
//
// Written from the task contract only -- Developer's implementation of
// the flag (confined to `crates/iwec/src/main.rs`'s arg parsing and the
// store-root discovery path in `lib.rs`) is not read here. Tests may
// fail against whatever code currently exists; that is expected until
// Developer's change lands.
//
// Interpretation notes:
//
// - Criterion 1 ("a temp copy of a store") and criterion 3 ("non-iwe
//   path") are read together: a real store, as every other fixture in
//   this crate's test suite builds one (`.iwe/config.toml` present --
//   see `cli_mcp_write_scope_parity_test.rs`'s `store()`,
//   `commit_trigger_test.rs`'s `store_with_config()`,
//   `agent_transaction_test.rs`'s `store()`), and "non-iwe" is the
//   negation of that: a path that is not such a store. This suite tests
//   two independent negative shapes for criterion 3 -- a path that
//   exists but is not a directory (a regular file), and a directory that
//   exists but carries no `.iwe/config.toml` -- plus the unambiguous
//   "nonexistent path" leg.
// - Criterion 2's regression fixtures deliberately keep the *bare*
//   directory shape `http_transport_test.rs` already established as a
//   valid cwd-discovery target (no `.iwe/` marker at all), since the
//   point of that criterion is that cwd discovery itself is unchanged.

use std::fs::{create_dir_all, write};
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, Implementation,
};
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tempfile::TempDir;

// ---- shared helpers (this file is self-contained, like its siblings
// `http_transport_test.rs` / `cli_mcp_write_scope_parity_test.rs` /
// `commit_trigger_test.rs`) ----

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

/// A document store: a plain directory holding one `.md` file, no
/// `.iwe/` marker required (matching `http_transport_test.rs`'s
/// baseline). `marker` is planted in the document body so a read can
/// prove *which* store answered. Used for cwd-discovery fixtures
/// (criterion 2's regression legs), where the point is that discovery
/// itself is unchanged.
fn store_with_seed(marker: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path().join("seed.md"),
        format!("# Seed\n\n{marker}\n"),
    )
    .expect("write seed.md");
    dir
}

/// A real, initialized store -- `.iwe/config.toml` present, matching the
/// `store()`/`store_with_config()` fixtures every other test file in
/// this crate builds (`cli_mcp_write_scope_parity_test.rs`,
/// `commit_trigger_test.rs`, `agent_transaction_test.rs`). Used
/// wherever a fixture stands in for "a temp copy of a store" that
/// `--store` is pointed at directly.
fn iwe_store_with_seed(marker: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    create_dir_all(dir.path().join(".iwe")).expect("mkdir .iwe");
    write(dir.path().join(".iwe/config.toml"), "[transactions]\n").expect("write config");
    write(
        dir.path().join("seed.md"),
        format!("# Seed\n\n{marker}\n"),
    )
    .expect("write seed.md");
    dir
}

fn spawn_http(dir_for_cwd: &Path, store: Option<&Path>, port: u16) -> ServerProcess {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_iwec"));
    cmd.arg("--transport")
        .arg("http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .current_dir(dir_for_cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(store) = store {
        cmd.arg("--store").arg(store);
    }
    ServerProcess {
        child: cmd.spawn().expect("spawn iwec"),
    }
}

async fn connect_http(addr: &str) -> RunningService<RoleClient, ClientInfo> {
    wait_until_listening(addr).await;
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t1-store-flag-test-client", "0.0.1"),
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
) -> rmcp::model::CallToolResult {
    let args = arguments.as_object().cloned().unwrap_or_default();
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args))
        .await
        .expect(name)
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

fn result_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&result_text(result)).expect("result to be valid JSON")
}

// ---- criterion 1: --store overrides cwd discovery under http, for both
// a read and a staged-then-committed write ----

#[tokio::test]
async fn http_store_flag_serves_the_flagged_store_for_a_read_regardless_of_cwd() {
    let flagged = iwe_store_with_seed("STORE_MARKER");
    let cwd = store_with_seed("CWD_MARKER");
    let port = free_port();
    let _server = spawn_http(cwd.path(), Some(flagged.path()), port);

    let client = connect_http(&format!("127.0.0.1:{port}")).await;
    let seen = call_tool(&client, "iwe_retrieve", json!({"keys": ["seed"]})).await;
    let docs = result_json(&seen);
    let text = docs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|d| d["content"].as_str().map(str::to_string))
        .unwrap_or_default();
    assert!(
        text.contains("STORE_MARKER"),
        "read must come from the --store path, got: {text}"
    );
    assert!(
        !text.contains("CWD_MARKER"),
        "read must not come from the cwd-discovered store, got: {text}"
    );

    client.cancel().await.expect("client to disconnect");
}

#[tokio::test]
async fn http_store_flag_lands_a_staged_then_committed_write_in_the_flagged_store_not_cwd() {
    let flagged = iwe_store_with_seed("STORE_MARKER");
    let cwd = store_with_seed("CWD_MARKER");
    let port = free_port();
    let _server = spawn_http(cwd.path(), Some(flagged.path()), port);

    let client = connect_http(&format!("127.0.0.1:{port}")).await;

    let begun = call_tool(&client, "iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");

    let content = "# New\n\nNEW_DOC_MARKER\n";
    let created = call_tool(
        &client,
        "iwe_create",
        json!({"key": "brandnew", "content": content}),
    )
    .await;
    assert!(!created.is_error.unwrap_or(false), "{created:?}");

    let committed = call_tool(&client, "iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");

    assert!(
        flagged.path().join("brandnew.md").exists(),
        "the committed write must land under the --store path"
    );
    assert!(
        !cwd.path().join("brandnew.md").exists(),
        "the committed write must not land under the cwd-discovered store"
    );

    client.cancel().await.expect("client to disconnect");
}

// ---- criterion 2: without --store, discovery is unchanged, in both
// transports (regression) ----

#[tokio::test]
async fn http_without_store_flag_discovers_the_cwd_store_as_before() {
    let cwd = store_with_seed("CWD_MARKER");
    let port = free_port();
    let _server = spawn_http(cwd.path(), None, port);

    let client = connect_http(&format!("127.0.0.1:{port}")).await;
    let seen = call_tool(&client, "iwe_retrieve", json!({"keys": ["seed"]})).await;
    let text = result_text(&seen);
    assert!(
        result_json(&seen)
            .as_array()
            .and_then(|a| a.first())
            .and_then(|d| d["content"].as_str().map(str::to_string))
            .unwrap_or_default()
            .contains("CWD_MARKER"),
        "without --store, the cwd must still be discovered as before, got: {text}"
    );

    client.cancel().await.expect("client to disconnect");
}

#[tokio::test]
async fn stdio_without_store_flag_discovers_the_cwd_store_as_before() {
    let cwd = store_with_seed("CWD_MARKER");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_iwec"));
    cmd.current_dir(cwd.path());
    let transport = TokioChildProcess::new(cmd).expect("spawn iwec over stdio");
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t1-store-flag-stdio-test-client", "0.0.1"),
    );
    let client = client_info
        .serve(transport)
        .await
        .expect("client to connect over stdio");

    let seen = call_tool(&client, "iwe_retrieve", json!({"keys": ["seed"]})).await;
    let docs = result_json(&seen);
    let text = docs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|d| d["content"].as_str().map(str::to_string))
        .unwrap_or_default();
    assert!(
        text.contains("CWD_MARKER"),
        "without --store under stdio, the cwd must still be discovered as before, got: {text}"
    );

    client.cancel().await.expect("client to disconnect");
}

// ---- criterion 4: flag semantics uniform across transports -- one
// stdio-mode test exercising --store itself ----

#[tokio::test]
async fn stdio_store_flag_serves_the_flagged_store_regardless_of_cwd() {
    let flagged = iwe_store_with_seed("STORE_MARKER");
    let cwd = store_with_seed("CWD_MARKER");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_iwec"));
    cmd.arg("--store").arg(flagged.path()).current_dir(cwd.path());
    let transport = TokioChildProcess::new(cmd).expect("spawn iwec over stdio");
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t1-store-flag-stdio-override-test-client", "0.0.1"),
    );
    let client = client_info
        .serve(transport)
        .await
        .expect("client to connect over stdio");

    let seen = call_tool(&client, "iwe_retrieve", json!({"keys": ["seed"]})).await;
    let docs = result_json(&seen);
    let text = docs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|d| d["content"].as_str().map(str::to_string))
        .unwrap_or_default();
    assert!(
        text.contains("STORE_MARKER"),
        "--store must override cwd discovery under stdio too, got: {text}"
    );
    assert!(
        !text.contains("CWD_MARKER"),
        "--store must override cwd discovery under stdio too, got: {text}"
    );

    client.cancel().await.expect("client to disconnect");
}

// ---- criterion 3: an explicit --store that cannot be served fails
// fast, cleanly, and never falls back to a freshly-initialized empty
// store ----

fn spawn_http_with_raw_store_arg(port: u16, store: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_iwec"))
        .arg("--transport")
        .arg("http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--store")
        .arg(store)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn iwec")
}

/// Poll for exit rather than block on `wait_with_output` directly, so a
/// developer implementation that does *not* fail fast (e.g. one that
/// opens the listener and hangs waiting for connections) fails this test
/// with a clear message instead of hanging the suite.
fn wait_for_exit_within(mut child: Child, timeout: Duration) -> Output {
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return child.wait_with_output().expect("collect output");
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "iwec did not exit within {timeout:?} against an unservable --store -- \
                 it must fail fast, not open a listener and hang"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_fails_fast_naming_the_path(store: &Path, port: u16) {
    let child = spawn_http_with_raw_store_arg(port, store);
    let output = wait_for_exit_within(child, Duration::from_secs(10));

    assert!(
        !output.status.success(),
        "an unservable --store must exit nonzero, got: {:?}",
        output.status
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&store.display().to_string()),
        "the failure must name the offending path, got stderr: {stderr}"
    );

    // No listener was left open: the flag must never fall back to
    // silently serving a freshly-initialized empty store on that port.
    let addr = format!("127.0.0.1:{port}").parse().expect("parse addr");
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err(),
        "no listener may be left open after an unservable --store"
    );
}

#[test]
fn store_flag_pointing_at_a_nonexistent_path_fails_fast() {
    let base = TempDir::new().expect("tempdir");
    let missing = base.path().join("does-not-exist");
    assert_fails_fast_naming_the_path(&missing, free_port());
}

#[test]
fn store_flag_pointing_at_a_regular_file_fails_fast() {
    // See the file-level note: a path that exists but is not a
    // directory can never be a store under any reading.
    let base = TempDir::new().expect("tempdir");
    let not_a_store = base.path().join("just-a-file.txt");
    write(&not_a_store, "not a store\n").expect("write plain file");
    assert_fails_fast_naming_the_path(&not_a_store, free_port());
}

#[test]
fn store_flag_pointing_at_an_uninitialized_directory_fails_fast() {
    // A directory that exists but was never initialized as an iwe store
    // (no `.iwe/config.toml`) -- the other independent reading of
    // "non-iwe path" from the file-level note.
    let base = TempDir::new().expect("tempdir");
    let not_a_store = base.path().join("plain-directory");
    create_dir_all(&not_a_store).expect("mkdir plain directory");
    assert_fails_fast_naming_the_path(&not_a_store, free_port());
}

// ---- criterion 5: --help documents the flag ----

#[test]
fn help_documents_the_store_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_iwec"))
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--store"),
        "--help must document the --store flag, got:\n{text}"
    );
}
