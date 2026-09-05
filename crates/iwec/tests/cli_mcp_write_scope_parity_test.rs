// MCP half of the `iwe`/`iwec` write-scope parity check for
// `efforts/multi-agent-orchestration/implementation/mind-write-separation/t4-cli-parity`.
//
// Drives the real `iwec` MCP binary over its HTTP transport (the same
// transport `http_transport_test.rs` already exercises) against a
// scratch store whose `.iwe/config.toml` carries the production-default
// `[transactions]` shape -- an empty `[transactions]` section with no
// `validate` key, no in-file `deny`/`allow`. The deny/allow list is
// resolved entirely via the subprocess env (`IWE_TRANSACTIONS_ALLOW=
// mind/**`); the same shape the milestone's acceptance runs use, and
// the shape t4b's `for_config` gate widening (commit 646e449) was
// written to support. Without that widening, the production-default
// `[transactions]` config would resolve to `NoopTransaction` and the
// write-scope check would never run on either binary -- this test
// exercises the path that closed that defect.
//
// The CLI half of this parity check lives in
// `crates/iwe/tests/cli_mcp_write_scope_parity_test.rs`, driving the
// real `iwe` CLI binary (`iwe create <key> --content <content>`) with
// the identical resolved deny/allow and the same target key pairs
// (`mind/a` permitted, `other/b` denied). Both halves assert the same
// observable parity: the denied key is refused distinguishably (MCP
// error-message text here, nonzero exit + stderr text on the CLI
// half), and the permitted key lands and is readable back through the
// canonical read path on that surface (`iwe_retrieve` here, `iwe
// retrieve` on the CLI half).
//
// What is observable across the MCP subprocess boundary: only the
// `ValidationFailure::WriteScopeDenied`'s own `Display` rendering
// (literally: `write to '<key>' rejected: refused by the configured
// write scope`). The variant name never reaches the MCP message; this
// test matches on substrings ("rejected" / "write scope") plus the
// key, never on the variant identifier.

use std::fs::{create_dir_all, write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceError, ServiceExt};
use serde_json::json;
use tempfile::TempDir;

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

/// Production-default `[transactions]` shape: the section is present
/// but empty, with no `validate` key and no in-file `deny`/`allow` --
/// the very shape t4b's gate widening was written to support. The
/// deny/allow list comes entirely from the subprocess env (see
/// `spawn_server`).
fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join(".iwe")).unwrap();
    write(dir.path().join(".iwe/config.toml"), "[transactions]\n").unwrap();
    dir
}

/// Spawn `iwec` over HTTP on an ephemeral port, cwd set to `dir`, with
/// the resolved deny/allow carried by `IWE_TRANSACTIONS_ALLOW` and
/// `IWE_TRANSACTIONS_DENY` explicitly cleared (so no ambient value
/// from the test-runner's own environment leaks in -- the env overlay
/// treats either var being set as "override entirely," and the
/// fail-fast on the resolved deny-and-allow-both-non-empty case would
/// otherwise fire on a run where the runner has both set).
fn spawn_server(dir: &TempDir) -> (u16, ServerProcess) {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_iwec"))
        .arg("--transport")
        .arg("http")
        .arg("--port")
        .arg(port.to_string())
        .env("IWE_TRANSACTIONS_ALLOW", "mind/**")
        .env_remove("IWE_TRANSACTIONS_DENY")
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
        Implementation::new("iwe-t4-cli-parity-test-client", "0.0.1"),
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

fn result_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("result to have a text block");
    serde_json::from_str(&text).expect("result to be valid JSON")
}

#[tokio::test]
async fn iwe_create_to_an_allow_listed_key_succeeds_and_a_canonical_re_read_shows_it_landed() {
    let dir = store();
    let (port, _server) = spawn_server(&dir);
    let addr = format!("127.0.0.1:{port}");

    let client = connect(&addr).await;

    let write_result = call_tool(
        &client,
        "iwe_create",
        json!({ "key": "mind/a", "content": "# A\n" }),
    )
    .await;
    assert!(
        write_result.is_ok(),
        "write to an allow-listed key must succeed, got: {write_result:?}"
    );

    // Canonical re-read on this surface: the same server that wrote the
    // document reading it back through `iwe_retrieve`, not a direct
    // file read. This is the parity shape the CLI half mirrors with
    // `iwe retrieve` -- the read path proves the document landed as a
    // document in the store, not merely as bytes on disk.
    let read_result = call_tool(&client, "iwe_retrieve", json!({ "keys": ["mind/a"] })).await;
    let docs = result_json(
        &read_result.expect("re-read via iwe_retrieve must succeed"),
    );
    let docs = docs.as_array().expect("iwe_retrieve returns a JSON array");
    assert!(
        docs.iter().any(|d| {
            d["key"] == "mind/a"
                && d["content"]
                    .as_str()
                    .map(|c| c.contains("# A"))
                    .unwrap_or(false)
        }),
        "the re-read must surface the document under its key, got: {docs:?}"
    );

    client.cancel().await.expect("client to disconnect");
}

#[tokio::test]
async fn iwe_create_to_a_key_outside_the_allow_list_fails_with_a_distinguishable_message_and_leaves_disk_untouched()
{
    let dir = store();
    let (port, _server) = spawn_server(&dir);
    let addr = format!("127.0.0.1:{port}");

    let client = connect(&addr).await;

    let result = call_tool(
        &client,
        "iwe_create",
        json!({ "key": "other/b", "content": "# B\n" }),
    )
    .await;

    let message = result
        .expect_err("a write to a denied key must fail, not silently succeed")
        .to_string();
    assert!(
        message.contains("other/b"),
        "the MCP failure must name the rejected key, got: {message}"
    );
    // "rejected" / "write scope" are the distinguishing substrings of
    // `ValidationFailure::WriteScopeDenied`'s `Display` rendering. The
    // variant name itself (`WriteScopeDenied`) never reaches the MCP
    // error message; we assert on the rendered text, not the variant
    // identifier. The alternatives to match -- a JSON-RPC parse error,
    // an MCP usage error -- would carry neither substring.
    assert!(
        message.contains("rejected") || message.contains("write scope"),
        "the MCP failure must surface the write-scope rejection \
         distinguishably from an unrelated parse/usage error, got: {message}"
    );
    assert!(
        !dir.path().join("other/b.md").exists(),
        "a denied write must not land on disk"
    );

    client.cancel().await.expect("client to disconnect");
}