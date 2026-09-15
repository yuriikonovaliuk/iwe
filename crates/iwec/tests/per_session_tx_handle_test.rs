// T2 (`efforts/crew-and-iwe-memory-footprint-reduction/iwec-shared-daemon/
// t2-per-session-tx-handle`): under `--transport http`, the implicit
// (no-`handle`) transaction slot must be keyed off the rmcp per-session
// identity instead of the literal `"default"` — today, every HTTP session
// collides on that one literal key, so two concurrent HTTP clients each
// running begin -> stage -> commit on the implicit handle can see, steal,
// or lose each other's staged transaction.
//
// Written from the task contract only — Developer's session-aware handle
// resolution (confined to `crates/iwec/src/lib.rs`'s `open_txs`
// resolution and `main.rs`'s session -> server wiring) is not read here.
// These tests may fail against whatever code currently exists (the
// literal `"default"` collision); that is expected until Developer's
// change lands. New file, self-contained like its siblings
// (`store_flag_test.rs`, `http_transport_test.rs`).
//
// Shared MCP tool surface exercised (`crates/iwec/src/lib.rs`):
// `iwe_tx_begin` / `iwe_tx_commit` (both take an optional `handle`),
// `iwe_create` (takes an optional `handle`, stages a write), `iwe_check`
// (per-key schema/integrity check, used here as the store-validates-clean
// probe since this crate has no whole-store-validate MCP tool).

use std::fs::{create_dir_all, write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, Implementation,
};
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tempfile::TempDir;

// ---- shared helpers (self-contained, per this crate's test-file convention) ----

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

/// A real, initialized store — `.iwe/config.toml` present with
/// transactions enabled at full validation scope, matching every other
/// transaction-exercising fixture in this crate
/// (`agent_transaction_test.rs`'s `store()`/`config()`). No schemas
/// bound: full-scope validation over an unconstrained store is trivially
/// clean, so "the store validates clean" turns only on whether the
/// commits themselves land correctly.
fn store() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    create_dir_all(dir.path().join(".iwe")).expect("mkdir .iwe");
    write(
        dir.path().join(".iwe/config.toml"),
        "[transactions]\nvalidate = \"full\"\n",
    )
    .expect("write config");
    dir
}

fn spawn_http(dir: &TempDir, port: u16) -> ServerProcess {
    ServerProcess {
        child: Command::new(env!("CARGO_BIN_EXE_iwec"))
            .arg("--transport")
            .arg("http")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn iwec"),
    }
}

async fn connect_http(addr: &str) -> RunningService<RoleClient, ClientInfo> {
    wait_until_listening(addr).await;
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t2-per-session-tx-handle-test-client", "0.0.1"),
    );
    client_info
        .serve(transport)
        .await
        .expect("client to connect")
}

async fn connect_stdio(dir: &TempDir) -> RunningService<RoleClient, ClientInfo> {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_iwec"));
    cmd.current_dir(dir.path());
    let transport = TokioChildProcess::new(cmd).expect("spawn iwec over stdio");
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t2-per-session-tx-handle-stdio-test-client", "0.0.1"),
    );
    client_info
        .serve(transport)
        .await
        .expect("client to connect over stdio")
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
        .next()
        .unwrap_or_default()
}

fn result_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&result_text(result)).expect("result to be valid JSON")
}

fn ok(result: &rmcp::model::CallToolResult) -> bool {
    !result.is_error.unwrap_or(false)
}

/// True when an `iwe_retrieve` result does not carry `marker`'s staged
/// content: either the key answers as a not-yet-a-document fill-in
/// request (empty `content`) or, degenerately, no entry at all.
fn retrieve_hides(result: &rmcp::model::CallToolResult, marker: &str) -> bool {
    !result_text(result).contains(marker)
}

async fn begin_stage_commit(
    client: &RunningService<RoleClient, ClientInfo>,
    key: &str,
    content: &str,
) -> Result<String, String> {
    let begun = call_tool(client, "iwe_tx_begin", json!({})).await;
    if !ok(&begun) {
        return Err(format!("begin failed for {key}: {}", result_text(&begun)));
    }
    let handle = result_json(&begun)["handle"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let created = call_tool(client, "iwe_create", json!({"key": key, "content": content})).await;
    if !ok(&created) {
        return Err(format!("create failed for {key}: {}", result_text(&created)));
    }

    let committed = call_tool(client, "iwe_tx_commit", json!({})).await;
    if !ok(&committed) {
        return Err(format!("commit failed for {key}: {}", result_text(&committed)));
    }
    Ok(handle)
}

// ---- criterion 1: two concurrent HTTP clients on the implicit handle
// never see, steal, or commit each other's staged transaction ----

#[tokio::test]
async fn http_two_overlapping_sessions_get_isolated_implicit_transactions() {
    let dir = store();
    let port = free_port();
    let _server = spawn_http(&dir, port);
    let addr = format!("127.0.0.1:{port}");

    let client_a = connect_http(&addr).await;
    let client_b = connect_http(&addr).await;

    // Both open an implicit (no-handle) transaction while the other is
    // still open — today's literal "default" key means B's begin is
    // refused as "already open" here; a session-derived key must let
    // both succeed.
    let begun_a = call_tool(&client_a, "iwe_tx_begin", json!({})).await;
    assert!(ok(&begun_a), "session A's implicit begin: {begun_a:?}");
    let begun_b = call_tool(&client_b, "iwe_tx_begin", json!({})).await;
    assert!(
        ok(&begun_b),
        "session B's implicit begin must not be refused by session A's still-open \
         implicit transaction: {begun_b:?}"
    );

    let handle_a = result_json(&begun_a)["handle"].as_str().unwrap_or_default().to_string();
    let handle_b = result_json(&begun_b)["handle"].as_str().unwrap_or_default().to_string();
    assert_ne!(
        handle_a, handle_b,
        "two distinct HTTP sessions' implicit handles must resolve to distinct \
         per-session keys, not both to the literal \"default\""
    );

    let doc_a = "# Doc A\n\nStaged only by session A.\n";
    let doc_b = "# Doc B\n\nStaged only by session B.\n";
    let created_a = call_tool(&client_a, "iwe_create", json!({"key": "doc-a", "content": doc_a})).await;
    assert!(ok(&created_a), "session A's staged create: {created_a:?}");
    let created_b = call_tool(&client_b, "iwe_create", json!({"key": "doc-b", "content": doc_b})).await;
    assert!(ok(&created_b), "session B's staged create: {created_b:?}");

    // Neither session's own no-handle transaction is visible to the
    // other's reads before commit.
    let seen_by_a = call_tool(&client_a, "iwe_retrieve", json!({"keys": ["doc-b"]})).await;
    assert!(
        retrieve_hides(&seen_by_a, "Staged only by session B"),
        "session A must not see session B's still-staged doc-b: {seen_by_a:?}"
    );
    let seen_by_b = call_tool(&client_b, "iwe_retrieve", json!({"keys": ["doc-a"]})).await;
    assert!(
        retrieve_hides(&seen_by_b, "Staged only by session A"),
        "session B must not see session A's still-staged doc-a: {seen_by_b:?}"
    );

    // Session B commits (or aborting session A's transaction here would
    // be the "steal" failure mode); each commit must only ever act on its
    // own transaction.
    let committed_b = call_tool(&client_b, "iwe_tx_commit", json!({})).await;
    assert!(ok(&committed_b), "session B's commit: {committed_b:?}");
    assert_eq!(result_json(&committed_b)["keys"], json!(["doc-b"]));

    // Session A's transaction must still be open and uncommitted by B's
    // commit — the concrete "commit each other's" failure mode.
    assert!(
        !dir.path().join("doc-a.md").exists(),
        "session B's commit must not have landed session A's still-open staged doc-a"
    );
    let committed_a = call_tool(&client_a, "iwe_tx_commit", json!({})).await;
    assert!(ok(&committed_a), "session A's commit: {committed_a:?}");
    assert_eq!(result_json(&committed_a)["keys"], json!(["doc-a"]));

    // Both commits landed.
    assert!(dir.path().join("doc-a.md").exists());
    assert!(dir.path().join("doc-b.md").exists());

    // The store validates clean: each landed document reports no
    // violations (this crate's `iwe_check` is the store-wide validate
    // surface available at the MCP boundary; `iwe_tx_commit`'s own
    // full-scope validation is what actually gated each commit above).
    let checked = call_tool(&client_a, "iwe_check", json!({"keys": ["doc-a", "doc-b"]})).await;
    let report = result_json(&checked);
    for entry in report.as_array().expect("check report is an array") {
        assert_eq!(entry["ok"], json!(true), "store must validate clean: {entry}");
    }

    client_a.cancel().await.expect("client A to disconnect");
    client_b.cancel().await.expect("client B to disconnect");
}

// ---- criterion 2: concurrent-write no-data-loss on the implicit handle ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_two_concurrent_sessions_writing_on_the_implicit_handle_lose_no_update() {
    let dir = store();
    let port = free_port();
    let _server = spawn_http(&dir, port);
    let addr = format!("127.0.0.1:{port}");

    let client_a = connect_http(&addr).await;
    let client_b = connect_http(&addr).await;

    // Genuinely concurrent: both sessions run begin -> stage -> commit on
    // their own implicit handle at the same time, with no explicit
    // handle and no ordering imposed between them. Either ordering is
    // acceptable per the contract; what must not happen is either
    // session's begin/create/commit being refused or overwritten by the
    // other's.
    let (result_a, result_b) = tokio::join!(
        begin_stage_commit(&client_a, "concurrent-a", "# Concurrent A\n\nBody A.\n"),
        begin_stage_commit(&client_b, "concurrent-b", "# Concurrent B\n\nBody B.\n"),
    );

    assert!(result_a.is_ok(), "session A's concurrent workflow: {result_a:?}");
    assert!(result_b.is_ok(), "session B's concurrent workflow: {result_b:?}");

    // Both committed results are present afterwards — no lost update.
    assert!(
        dir.path().join("concurrent-a.md").exists(),
        "session A's concurrent write must have landed"
    );
    assert!(
        dir.path().join("concurrent-b.md").exists(),
        "session B's concurrent write must have landed"
    );

    let checked = call_tool(
        &client_a,
        "iwe_check",
        json!({"keys": ["concurrent-a", "concurrent-b"]}),
    )
    .await;
    let report = result_json(&checked);
    for entry in report.as_array().expect("check report is an array") {
        assert_eq!(
            entry["ok"],
            json!(true),
            "store must pass schema/integrity validation after the concurrent writes: {entry}"
        );
    }

    client_a.cancel().await.expect("client A to disconnect");
    client_b.cancel().await.expect("client B to disconnect");
}

// ---- criterion 3: under stdio the implicit handle remains the literal
// "default" (backward-compat) ----

#[tokio::test]
async fn stdio_implicit_handle_remains_literal_default() {
    let dir = store();
    let client = connect_stdio(&dir).await;

    let begun = call_tool(&client, "iwe_tx_begin", json!({})).await;
    assert!(ok(&begun), "{begun:?}");
    assert_eq!(
        result_json(&begun)["handle"],
        json!("default"),
        "under stdio, the implicit handle must remain the literal \"default\": {begun:?}"
    );

    let content = "# Stdio Doc\n\nWritten on the implicit stdio handle.\n";
    let created = call_tool(&client, "iwe_create", json!({"key": "stdio-doc", "content": content})).await;
    assert!(ok(&created), "{created:?}");

    let committed = call_tool(&client, "iwe_tx_commit", json!({})).await;
    assert!(ok(&committed), "{committed:?}");
    assert!(dir.path().join("stdio-doc.md").exists());

    client.cancel().await.expect("client to disconnect");
}

// ---- criterion 4: explicit named handles work unchanged in both
// transports (regression) ----

async fn explicit_named_handle_still_isolated_and_lands(client: &RunningService<RoleClient, ClientInfo>, dir: &TempDir) {
    let begun = call_tool(client, "iwe_tx_begin", json!({"handle": "explicit-1"})).await;
    assert!(ok(&begun), "{begun:?}");
    assert_eq!(result_json(&begun)["handle"], json!("explicit-1"));

    let content = "# Explicit\n\nStaged under an explicit named handle.\n";
    let created = call_tool(
        client,
        "iwe_create",
        json!({"key": "explicit-doc", "content": content, "handle": "explicit-1"}),
    )
    .await;
    assert!(ok(&created), "{created:?}");

    // Unhandled (default) reads must not see an explicit handle's staged
    // write — unchanged by this task.
    let seen = call_tool(client, "iwe_retrieve", json!({"keys": ["explicit-doc"]})).await;
    assert!(
        retrieve_hides(&seen, "Staged under an explicit named handle"),
        "an explicit handle's staged write must stay invisible to no-handle reads: {seen:?}"
    );

    let committed = call_tool(client, "iwe_tx_commit", json!({"handle": "explicit-1"})).await;
    assert!(ok(&committed), "{committed:?}");
    assert_eq!(result_json(&committed)["keys"], json!(["explicit-doc"]));
    assert!(dir.path().join("explicit-doc.md").exists());
}

#[tokio::test]
async fn http_explicit_named_handle_regression() {
    let dir = store();
    let port = free_port();
    let _server = spawn_http(&dir, port);
    let client = connect_http(&format!("127.0.0.1:{port}")).await;

    explicit_named_handle_still_isolated_and_lands(&client, &dir).await;

    client.cancel().await.expect("client to disconnect");
}

#[tokio::test]
async fn stdio_explicit_named_handle_regression() {
    let dir = store();
    let client = connect_stdio(&dir).await;

    explicit_named_handle_still_isolated_and_lands(&client, &dir).await;

    client.cancel().await.expect("client to disconnect");
}
