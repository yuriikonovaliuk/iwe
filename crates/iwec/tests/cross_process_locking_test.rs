//! T6: the HTTP daemon and independent stdio processes share the same
//! store-wide commit lock. The daemon's transaction stages first, the stdio
//! process moves disk state, then the daemon attempts its commit.

use std::fs::{create_dir_all, read_to_string, write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tempfile::TempDir;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn store() -> TempDir {
    let dir = tempfile::tempdir().expect("temp store");
    create_dir_all(dir.path().join(".iwe")).expect("create .iwe");
    write(
        dir.path().join(".iwe/config.toml"),
        "[transactions]\nvalidate = \"full\"\n",
    )
    .expect("write config");
    write(dir.path().join("seed.md"), "# Seed\n\nSeed document.\n").expect("write seed");
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind port")
        .local_addr()
        .expect("port address")
        .port()
}

async fn wait_for_http(address: &str) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("HTTP iwec did not listen at {address}");
}

fn spawn_http(store: &Path, port: u16) -> ServerProcess {
    ServerProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_iwec"))
            .args(["--transport", "http", "--host", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .arg("--store")
            .arg(store)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn HTTP iwec"),
    )
}

async fn http_client(address: &str) -> RunningService<RoleClient, ClientInfo> {
    wait_for_http(address).await;
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-cross-process-http", "0.0.1"),
    )
    .serve(StreamableHttpClientTransport::from_uri(format!("http://{address}/mcp")))
    .await
    .expect("connect HTTP client")
}

async fn stdio_client(store: &Path) -> RunningService<RoleClient, ClientInfo> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_iwec"));
    command.arg("--store").arg(store);
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-cross-process-stdio", "0.0.1"),
    )
    .serve(TokioChildProcess::new(command).expect("spawn independent stdio iwec"))
    .await
    .expect("connect stdio client")
}

async fn call(
    client: &RunningService<RoleClient, ClientInfo>,
    name: &'static str,
    arguments: serde_json::Value,
) -> rmcp::model::CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(name)
                .with_arguments(arguments.as_object().cloned().expect("object arguments")),
        )
        .await
        .expect(name)
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(value) => Some(value.text.as_str()),
            _ => None,
        })
        .collect()
}

async fn create_and_commit(
    client: &RunningService<RoleClient, ClientInfo>,
    key: &str,
    content: &str,
) {
    let begun = call(client, "iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let created = call(client, "iwe_create", json!({"key": key, "content": content})).await;
    assert!(!created.is_error.unwrap_or(false), "{created:?}");
    let committed = call(client, "iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
}

#[tokio::test]
async fn daemon_and_independent_stdio_commit_interleaved_distinct_documents_without_loss() {
    let store = store();
    let port = free_port();
    let _server = spawn_http(store.path(), port);
    let daemon = http_client(&format!("127.0.0.1:{port}")).await;

    // Stage through the long-lived daemon before a separate OS process moves
    // the store. The daemon commit must re-read the current state under the
    // cross-process lock rather than overwrite that other document.
    let begun = call(&daemon, "iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let staged = call(
        &daemon,
        "iwe_create",
        json!({"key": "daemon", "content": "# Daemon\n\nDAEMON_WRITER\n"}),
    )
    .await;
    assert!(!staged.is_error.unwrap_or(false), "{staged:?}");

    let stdio = stdio_client(store.path()).await;
    create_and_commit(&stdio, "stdio", "# Stdio\n\nSTDIO_WRITER\n").await;
    stdio.cancel().await.expect("disconnect stdio client");

    let committed = call(&daemon, "iwe_tx_commit", json!({})).await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    daemon.cancel().await.expect("disconnect daemon client");

    // A fresh process validates from disk, not the daemon's pre-interleave
    // cache. `iwe_check` is the MCP schema/integrity validation surface.
    let verifier = stdio_client(store.path()).await;
    let checked = call(&verifier, "iwe_check", json!({"keys": ["daemon", "stdio"]})).await;
    assert!(!checked.is_error.unwrap_or(false), "{checked:?}");
    assert!(text(&checked).contains("\"ok\":true"), "{checked:?}");
    verifier.cancel().await.expect("disconnect verifier");

    assert!(read_to_string(store.path().join("daemon.md")).expect("daemon data").contains("DAEMON_WRITER"));
    assert!(read_to_string(store.path().join("stdio.md")).expect("stdio data").contains("STDIO_WRITER"));
}

#[tokio::test]
async fn daemon_commit_detects_stdio_moved_base_under_the_commit_lock() {
    let store = store();
    let port = free_port();
    let _server = spawn_http(store.path(), port);
    let daemon = http_client(&format!("127.0.0.1:{port}")).await;

    let begun = call(&daemon, "iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let staged = call(
        &daemon,
        "iwe_create",
        json!({"key": "moved-base", "content": "# Daemon version\n\nDAEMON_BASE\n"}),
    )
    .await;
    assert!(!staged.is_error.unwrap_or(false), "{staged:?}");

    let stdio = stdio_client(store.path()).await;
    create_and_commit(&stdio, "moved-base", "# Stdio version\n\nSTDIO_BASE\n").await;
    stdio.cancel().await.expect("disconnect stdio client");

    let refused = daemon
        .call_tool(CallToolRequestParams::new("iwe_tx_commit"))
        .await
        .expect_err("stale base must be refused");
    assert!(
        refused.to_string().contains("changed on disk"),
        "expected base-drift evidence: {refused:?}"
    );
    daemon.cancel().await.expect("disconnect daemon client");

    let final_content = read_to_string(store.path().join("moved-base.md")).expect("stdio document remains");
    assert!(final_content.contains("STDIO_BASE"), "stale daemon data overwrote the moved base: {final_content}");
    assert!(!final_content.contains("DAEMON_BASE"), "stale daemon data landed: {final_content}");
}
