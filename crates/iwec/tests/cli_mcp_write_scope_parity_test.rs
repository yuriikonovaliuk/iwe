// MCP half of the `iwe`/`iwec` write-scope parity check for
// `efforts/multi-agent-orchestration/implementation/mind-write-separation/t4-cli-parity`.
//
// The sibling half of this test lives in
// `crates/iwe/tests/cli_mcp_write_scope_parity_test.rs`, driving the real
// `iwe` CLI binary (`iwe create <key> --content <content>`) with the
// identical resolved deny/allow (`IWE_TRANSACTIONS_ALLOW=mind/**` in both
// processes' env) and the same target keys (`mind/a` permitted, `other/b`
// denied). This half drives the real `iwec` MCP binary (over its HTTP
// transport, as `http_transport_test.rs` already does) rather than the
// in-process `Fixture` the other MCP tests here use, since the parity in
// question is about how each *binary* sources its `Configuration` --
// `Fixture` never calls `load_config()` at all.
//
// CLI-parity verdict (see the CLI half's header comment and this task's
// handback): `iwe`'s write path already constructs its
// `ValidatingTransaction` via the same `load_config()` +
// `ValidatingTransaction::for_config()` this server uses -- no production
// code change was needed. This test exercises that already-identical
// path, not a fixed one.

use std::fs::{create_dir_all, read_to_string, write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceError, ServiceExt};
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

/// `[transactions] validate = "affected-set"` is enough to turn on
/// `ValidatingTransaction` (and therefore its write-scope check) without
/// requiring any schema setup, exactly as the CLI half's fixture does.
fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join(".iwe")).unwrap();
    write(
        dir.path().join(".iwe/config.toml"),
        "[transactions]\nvalidate = \"affected-set\"\n",
    )
    .unwrap();
    dir
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

async fn iwe_create(
    client: &RunningService<RoleClient, ClientInfo>,
    key: &str,
    content: &str,
) -> Result<rmcp::model::CallToolResult, ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("iwe_create").with_arguments(
                serde_json::json!({ "key": key, "content": content })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
}

#[tokio::test]
async fn iwe_create_to_an_allow_listed_key_succeeds() {
    let dir = store();
    let port = free_port();
    let _server = ServerProcess {
        child: Command::new(env!("CARGO_BIN_EXE_iwec"))
            .arg("--transport")
            .arg("http")
            .arg("--port")
            .arg(port.to_string())
            .env("IWE_TRANSACTIONS_ALLOW", "mind/**")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn iwec"),
    };

    let client = connect(&format!("127.0.0.1:{port}")).await;
    let result = iwe_create(&client, "mind/a", "# A\n").await;
    client.cancel().await.expect("client to disconnect");

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(read_to_string(dir.path().join("mind/a.md")).unwrap(), "# A\n");
}

#[tokio::test]
async fn iwe_create_to_a_key_outside_the_allow_list_fails_with_write_scope_denied_and_leaves_disk_untouched()
{
    let dir = store();
    let port = free_port();
    let _server = ServerProcess {
        child: Command::new(env!("CARGO_BIN_EXE_iwec"))
            .arg("--transport")
            .arg("http")
            .arg("--port")
            .arg(port.to_string())
            .env("IWE_TRANSACTIONS_ALLOW", "mind/**")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn iwec"),
    };

    let client = connect(&format!("127.0.0.1:{port}")).await;
    let result = iwe_create(&client, "other/b", "# B\n").await;
    client.cancel().await.expect("client to disconnect");

    let message = result
        .expect_err("a write to a denied key must fail, not silently succeed")
        .to_string();
    // This is `ValidationFailure::WriteScopeDenied`'s own `Display`
    // ("write to 'other/b' rejected: refused by the configured write
    // scope"), the same message text `crates/diwe/tests/
    // write_scope_check_test.rs` pins for the direct-transaction case and
    // the CLI half of this parity test observes on its side.
    assert!(
        message.contains("other/b") && message.contains("rejected"),
        "expected the WriteScopeDenied rejection naming the key, got: {message}"
    );
    assert!(!dir.path().join("other/b.md").exists());
}
