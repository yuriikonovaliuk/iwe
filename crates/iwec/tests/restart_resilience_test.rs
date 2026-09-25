//! Restarting the HTTP daemon must be invisible to its clients, and must
//! never let an agent's writes land outside a transaction it believes open.
//!
//! Every test here runs the real `iwec --transport http` binary on an
//! ephemeral port, stops it with a real SIGTERM (systemd's stop signal) and
//! starts it again on the same port and `--state-dir`. Requests are plain
//! HTTP/1.1 so a test fully controls the `Mcp-Session-Id` it sends after a
//! restart, exactly as a connected client (Claude Code) would keep sending it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Daemon plumbing
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn start(store: &Path, port: u16, state_dir: Option<&Path>, extra: &[&str]) -> Daemon {
        let mut command = Command::new(env!("CARGO_BIN_EXE_iwec"));
        command
            .args(["--transport", "http", "--host", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .arg("--store")
            .arg(store)
            .args(extra);
        if let Some(state_dir) = state_dir {
            command.arg("--state-dir").arg(state_dir);
        }
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn HTTP iwec");
        let stderr = child.stderr.take().expect("piped stderr");
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                writer.lock().unwrap().push(line);
            }
        });
        // Wait for this child's own "listening" line, not merely for the
        // port to answer: between a stop and a restart the port is free and
        // could be answered by another test's daemon.
        let listening = format!("listening on http://127.0.0.1:{port}/mcp");
        let started = Instant::now();
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(&listening))
        {
            if let Ok(Some(status)) = child.try_wait() {
                panic!("iwec exited early ({status}): {:?}", log.lock().unwrap());
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "iwec never listened on port {port}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        Daemon { child, log }
    }

    fn sigterm(&self) {
        let rc = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        assert_eq!(rc, 0, "send SIGTERM");
    }

    /// Waits for the process to exit on its own, returning its status and
    /// how long it took.
    fn wait_exit(&mut self, within: Duration) -> (ExitStatus, Duration) {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return (status, started.elapsed());
            }
            assert!(
                started.elapsed() < within,
                "iwec did not exit within {within:?}; log: {:?}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// SIGTERM, then a clean exit.
    fn stop(mut self) -> Vec<String> {
        self.sigterm();
        let (status, _) = self.wait_exit(Duration::from_secs(15));
        assert!(
            status.success(),
            "iwec must exit cleanly on SIGTERM: {status}; {:?}",
            self.log()
        );
        // Let the stderr reader drain.
        std::thread::sleep(Duration::from_millis(50));
        self.log()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn wait_for_log(&self, needle: &str, within: Duration) {
        let started = Instant::now();
        while !self.log().iter().any(|line| line.contains(needle)) {
            assert!(
                started.elapsed() < within,
                "no log line containing {needle:?}: {:?}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// An ephemeral port never handed out before in this test process, so a
/// test restarting its daemon on its port cannot collide with a sibling.
fn free_port() -> u16 {
    static USED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        let mut used = USED.lock().unwrap();
        if !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

/// A store accepting transactions, with one seed document.
fn store() -> TempDir {
    let dir = tempfile::tempdir().expect("temp store");
    std::fs::create_dir_all(dir.path().join(".iwe")).expect("create .iwe");
    std::fs::write(
        dir.path().join(".iwe/config.toml"),
        "[transactions]\nvalidate = \"full\"\n",
    )
    .expect("write config");
    std::fs::write(dir.path().join("seed.md"), SEED).expect("write seed");
    dir
}

const SEED: &str = "# Seed\n\nOriginal body.\n";
const SEED_EDITED: &str = "# Seed\n\nEdited body.\n";

fn read(store: &Path, key: &str) -> Option<String> {
    std::fs::read_to_string(store.join(format!("{key}.md"))).ok()
}

// ---------------------------------------------------------------------------
// A minimal MCP-over-HTTP client that keeps its session ID across restarts
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The JSON-RPC response in the body, whether sent as JSON or as SSE.
    fn rpc(&self) -> Value {
        if let Ok(value) = serde_json::from_str::<Value>(&self.body) {
            return value;
        }
        self.body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .find(|value| value.get("result").is_some() || value.get("error").is_some())
            .unwrap_or_else(|| panic!("no JSON-RPC response in body {:?}", self.body))
    }
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = raw.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size_text = String::from_utf8_lossy(&raw[..line_end]);
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16).unwrap_or(0);
        raw = &raw[line_end + 2..];
        if size == 0 || raw.len() < size {
            return out;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[(size + 2).min(raw.len())..];
    }
}

fn http(port: u16, method: &str, session: Option<&str>, body: Option<&Value>) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut request = format!(
        "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n",
        payload.len()
    );
    if let Some(session) = session {
        request.push_str(&format!(
            "Mcp-Session-Id: {session}\r\nMCP-Protocol-Version: 2025-06-18\r\n"
        ));
    }
    request.push_str("\r\n");
    request.push_str(&payload);
    stream.write_all(request.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response headers");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let body_raw = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    let body = if chunked {
        dechunk(body_raw)
    } else {
        body_raw.to_vec()
    };
    HttpResponse {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

struct Session {
    port: u16,
    id: String,
    next: u64,
}

impl Session {
    /// `initialize` + `notifications/initialized`, as any MCP client does.
    fn open(port: u16) -> Session {
        let response = http(
            port,
            "POST",
            None,
            Some(&json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "restart-test", "version": "0.0.1"}
                }
            })),
        );
        assert_eq!(response.status, 200, "initialize: {}", response.body);
        let id = response
            .header("mcp-session-id")
            .expect("session id header")
            .to_string();
        assert!(response.rpc().get("result").is_some(), "{}", response.body);
        let session = Session { port, id, next: 1 };
        let notified = http(
            port,
            "POST",
            Some(&session.id),
            Some(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
        );
        assert_eq!(notified.status, 202, "initialized: {}", notified.body);
        session
    }

    fn request(&mut self, method: &str, params: Value) -> HttpResponse {
        self.next += 1;
        http(
            self.port,
            "POST",
            Some(&self.id),
            Some(&json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})),
        )
    }

    /// `tools/call`: `Ok(result text)` or `Err(error message)`.
    fn call(&mut self, tool: &str, arguments: Value) -> Result<String, String> {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert_eq!(
            response.status, 200,
            "{tool}: HTTP {} {}",
            response.status, response.body
        );
        let rpc = response.rpc();
        if let Some(error) = rpc.get("error") {
            return Err(error["message"].as_str().unwrap_or_default().to_string());
        }
        let result = &rpc["result"];
        let text: String = result["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if result["isError"].as_bool().unwrap_or(false) {
            Err(text)
        } else {
            Ok(text)
        }
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> String {
        self.call(tool, arguments.clone())
            .unwrap_or_else(|e| panic!("{tool} {arguments} must succeed: {e}"))
    }

    fn refused(&mut self, tool: &str, arguments: Value) -> String {
        match self.call(tool, arguments.clone()) {
            Ok(text) => panic!("{tool} {arguments} must be refused, got {text}"),
            Err(message) => message,
        }
    }
}

fn assert_lost_restart(message: &str, key: &str) {
    assert!(
        message.contains(&format!(
            "transaction {key} was dropped by a daemon restart at "
        )) && message.contains("none of its staged writes were applied")
            && message.contains("call iwe_tx_abort to acknowledge, then begin again"),
        "expected the lost-transaction refusal for {key}, got: {message}"
    );
}

fn state_dir(root: &TempDir) -> PathBuf {
    root.path().join("state")
}

fn open_transactions(state: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(state.join("open-transactions.json")).expect("state file"),
    )
    .expect("state file JSON")
}

// ---------------------------------------------------------------------------
// A. Persistent sessions
// ---------------------------------------------------------------------------

#[test]
fn a_session_survives_a_restart_without_reinitializing() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();

    let daemon = Daemon::start(store.path(), port, Some(&state), &[]);
    let mut session = Session::open(port);
    session.ok("iwe_find", json!({"lexical": "seed"}));
    let session_file = state.join("sessions").join(format!("{}.json", session.id));
    assert!(
        session_file.is_file(),
        "the session is persisted after initialize"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&session_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "session files are private");
    }
    let log = daemon.stop();
    assert!(
        log.iter()
            .any(|l| l.contains("drain complete; no transaction dropped")),
        "{log:?}"
    );

    let daemon = Daemon::start(store.path(), port, Some(&state), &[]);
    let listed = session.request("tools/list", json!({}));
    assert_eq!(
        listed.status, 200,
        "tools/list after restart: {}",
        listed.body
    );
    let tools = listed.rpc()["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        tools.iter().any(|t| t["name"] == "iwe_find"),
        "tools/list lists the tools after restart: {}",
        listed.body
    );
    let found = session.ok("iwe_find", json!({"lexical": "seed"}));
    assert!(
        found.contains("\"seed\""),
        "tools/call works after restart: {found}"
    );

    // A client DELETE ends the session and removes its file.
    let deleted = http(port, "DELETE", Some(&session.id), None);
    assert!(
        deleted.status < 300,
        "DELETE: {} {}",
        deleted.status,
        deleted.body
    );
    assert!(
        !session_file.exists(),
        "the session file goes with the session"
    );
    drop(daemon);
}

#[test]
fn a_real_mcp_client_keeps_working_across_a_restart() {
    use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation};
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;

    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();
    let daemon = Daemon::start(store.path(), port, Some(&state), &[]);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("restart-rmcp-client", "0.0.1"),
        )
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://127.0.0.1:{port}/mcp"
        )))
        .await
        .expect("connect");
        let find = || {
            CallToolRequestParams::new("iwe_find")
                .with_arguments(json!({"lexical": "seed"}).as_object().cloned().unwrap())
        };
        client.call_tool(find()).await.expect("call before restart");

        tokio::task::spawn_blocking(move || {
            daemon.stop();
        })
        .await
        .unwrap();
        let _daemon = Daemon::start(store.path(), port, Some(&state), &[]);

        client
            .list_all_tools()
            .await
            .expect("tools/list after restart");
        client
            .call_tool(find())
            .await
            .expect("tools/call after restart");
        let _ = client.cancel().await;
    });
}

#[test]
fn without_state_dir_a_restart_ends_the_session_as_today() {
    let store = store();
    let port = free_port();
    let daemon = Daemon::start(store.path(), port, None, &[]);
    let mut session = Session::open(port);
    session.ok("iwe_find", json!({"lexical": "seed"}));
    daemon.stop();

    let _daemon = Daemon::start(store.path(), port, None, &[]);
    let listed = session.request("tools/list", json!({}));
    assert_eq!(
        listed.status, 404,
        "an unknown session is 404 without --state-dir"
    );
    // A malformed ID is not specially treated without the flag.
    let malformed = http(
        port,
        "POST",
        Some("../x"),
        Some(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})),
    );
    assert_eq!(malformed.status, 404);
}

#[test]
fn invalid_session_ids_are_rejected() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();
    let _daemon = Daemon::start(store.path(), port, Some(&state), &[]);

    // A would-be target outside sessions/, shaped like a session file.
    std::fs::write(
        state.join("evil.json"),
        r#"{"session_id":"../evil","created_at":"x","state":{"initialize_params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e","version":"0"}}}}"#,
    )
    .unwrap();
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    for bad in [
        "../evil",
        "a.b",
        "has space",
        &"x".repeat(129),
        "%2e%2e%2fevil",
    ] {
        let response = http(port, "POST", Some(bad), Some(&list));
        assert_eq!(
            response.status, 400,
            "id {bad:?} must be rejected: {}",
            response.body
        );
    }
    // A well-formed but unknown ID is simply not found.
    let unknown = http(port, "POST", Some("no-such-session"), Some(&list));
    assert_eq!(unknown.status, 404);
    let names: Vec<_> = std::fs::read_dir(state.join("sessions"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        names.is_empty(),
        "nothing written for rejected IDs: {names:?}"
    );
}

#[test]
fn stale_sessions_are_pruned_at_startup() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    std::fs::create_dir_all(state.join("sessions")).unwrap();
    let stale = state.join("sessions/stale-session.json");
    let fresh = state.join("sessions/fresh-session.json");
    std::fs::write(&stale, "{}").unwrap();
    std::fs::write(&fresh, "{}").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600))
        .unwrap();
    let port = free_port();
    let _daemon = Daemon::start(store.path(), port, Some(&state), &[]);
    assert!(!stale.exists(), "a session idle for 8 days is pruned");
    assert!(fresh.exists(), "a recent session is kept");
}

// ---------------------------------------------------------------------------
// B. Transaction guard across restarts
// ---------------------------------------------------------------------------

#[test]
fn a_lost_implicit_transaction_refuses_writes_until_acknowledged() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();
    let args = ["--drain-timeout-secs", "1"];

    let daemon = Daemon::start(store.path(), port, Some(&state), &args);
    let mut session = Session::open(port);
    let key = format!("session:{}", session.id);
    session.ok("iwe_tx_begin", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_eq!(open_transactions(&state)["open"], json!([key]));
    let log = daemon.stop();
    assert!(
        log.iter().any(
            |l| l.contains("transactions still open at shutdown are dropped") && l.contains(&key)
        ),
        "the drop is logged: {log:?}"
    );
    assert_eq!(
        read(store.path(), "seed").as_deref(),
        Some(SEED),
        "nothing staged landed"
    );

    let daemon = Daemon::start(store.path(), port, Some(&state), &args);
    // The agent still believes its implicit transaction open: a no-handle
    // write must be refused, not applied straight to disk.
    let refused = session.refused("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_lost_restart(&refused, &key);
    let refused = session.refused(
        "iwe_create",
        json!({"key": "fresh", "content": "# Fresh\n"}),
    );
    assert_lost_restart(&refused, &key);
    let refused = session.refused("iwe_delete", json!({"key": "seed"}));
    assert_lost_restart(&refused, &key);
    let refused = session.refused("iwe_tx_commit", json!({}));
    assert_lost_restart(&refused, &key);
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED));
    assert!(read(store.path(), "fresh").is_none());
    // Reads are unaffected.
    session.ok("iwe_retrieve", json!({"keys": ["seed"]}));
    session.ok("iwe_find", json!({"lexical": "seed"}));
    // Another session is unaffected.
    let mut other = Session::open(port);
    other.ok(
        "iwe_create",
        json!({"key": "other", "content": "# Other\n"}),
    );

    // The tombstone survives a second restart.
    daemon.stop();
    let _daemon = Daemon::start(store.path(), port, Some(&state), &args);
    let refused = session.refused("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_lost_restart(&refused, &key);
    let refused = session.refused("iwe_tx_commit", json!({}));
    assert_lost_restart(&refused, &key);
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED));

    // Acknowledge; the same write now goes through (no transaction open).
    let aborted = session.ok("iwe_tx_abort", json!({}));
    assert!(aborted.contains("\"acknowledged\""), "{aborted}");
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED_EDITED));
    assert_eq!(open_transactions(&state)["lost"], json!([]));
}

#[test]
fn a_lost_explicit_handle_is_refused_until_acknowledged() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();
    let args = ["--drain-timeout-secs", "1"];

    let daemon = Daemon::start(store.path(), port, Some(&state), &args);
    let mut session = Session::open(port);
    session.ok("iwe_tx_begin", json!({"handle": "h1"}));
    session.ok(
        "iwe_create",
        json!({"handle": "h1", "key": "staged", "content": "# Staged\n"}),
    );
    daemon.stop();

    let _daemon = Daemon::start(store.path(), port, Some(&state), &args);
    let refused = session.refused(
        "iwe_create",
        json!({"handle": "h1", "key": "staged", "content": "# Staged\n"}),
    );
    assert_lost_restart(&refused, "h1");
    let refused = session.refused("iwe_tx_commit", json!({"handle": "h1"}));
    assert_lost_restart(&refused, "h1");
    // Any session naming the handle sees the same refusal.
    let mut other = Session::open(port);
    let refused = other.refused(
        "iwe_update",
        json!({"handle": "h1", "key": "seed", "content": SEED_EDITED}),
    );
    assert_lost_restart(&refused, "h1");
    assert!(read(store.path(), "staged").is_none());

    session.ok("iwe_tx_abort", json!({"handle": "h1"}));
    session.ok("iwe_tx_begin", json!({"handle": "h1"}));
    session.ok(
        "iwe_create",
        json!({"handle": "h1", "key": "staged", "content": "# Staged\n"}),
    );
    session.ok("iwe_tx_commit", json!({"handle": "h1"}));
    assert_eq!(read(store.path(), "staged").as_deref(), Some("# Staged\n"));
}

#[test]
fn beginning_again_on_a_lost_key_clears_its_tombstone() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();
    let args = ["--drain-timeout-secs", "1"];

    let daemon = Daemon::start(store.path(), port, Some(&state), &args);
    let mut session = Session::open(port);
    session.ok("iwe_tx_begin", json!({}));
    daemon.stop();

    let _daemon = Daemon::start(store.path(), port, Some(&state), &args);
    session.ok("iwe_tx_begin", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_eq!(
        read(store.path(), "seed").as_deref(),
        Some(SEED),
        "staged, not landed"
    );
    session.ok("iwe_tx_commit", json!({}));
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED_EDITED));
}

// ---------------------------------------------------------------------------
// B.4 Graceful stop
// ---------------------------------------------------------------------------

#[test]
fn a_transaction_committed_during_the_drain_lands_and_the_daemon_exits_cleanly() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();

    let mut daemon = Daemon::start(
        store.path(),
        port,
        Some(&state),
        &["--drain-timeout-secs", "30"],
    );
    let mut session = Session::open(port);
    let key = format!("session:{}", session.id);
    session.ok("iwe_tx_begin", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));

    daemon.sigterm();
    daemon.wait_for_log("draining open transactions", Duration::from_secs(5));

    // New transactions are refused while draining...
    let mut late = Session::open(port);
    let refused = late.refused("iwe_tx_begin", json!({}));
    assert!(
        refused.contains("daemon is restarting, retry shortly"),
        "{refused}"
    );
    let refused = late.refused("iwe_tx_begin", json!({"handle": "late"}));
    assert!(
        refused.contains("daemon is restarting, retry shortly"),
        "{refused}"
    );
    // ...everything else is still served, and the open one can finish.
    late.ok("iwe_find", json!({"lexical": "seed"}));
    session.ok("iwe_tx_commit", json!({}));

    let (status, took) = daemon.wait_exit(Duration::from_secs(10));
    assert!(status.success(), "clean exit: {status}");
    assert!(
        took < Duration::from_secs(10),
        "exits once drained, not at the timeout"
    );
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED_EDITED));
    std::thread::sleep(Duration::from_millis(50));
    let log = daemon.log();
    assert!(
        log.iter()
            .any(|l| l.contains("transactions closed during drain") && l.contains(&key)),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.contains("drain complete; no transaction dropped")),
        "{log:?}"
    );
    assert_eq!(open_transactions(&state)["open"], json!([]));

    // Nothing is tombstoned on the next start.
    let _daemon = Daemon::start(store.path(), port, Some(&state), &[]);
    session.ok("iwe_update", json!({"key": "seed", "content": SEED}));
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED));
}

#[test]
fn a_transaction_still_open_at_the_drain_timeout_is_tombstoned() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let state = state_dir(&root);
    let port = free_port();

    let mut daemon = Daemon::start(
        store.path(),
        port,
        Some(&state),
        &["--drain-timeout-secs", "2"],
    );
    let mut session = Session::open(port);
    let key = format!("session:{}", session.id);
    session.ok("iwe_tx_begin", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));

    daemon.sigterm();
    let (status, took) = daemon.wait_exit(Duration::from_secs(15));
    assert!(status.success(), "clean exit after the timeout: {status}");
    assert!(
        took >= Duration::from_secs(2),
        "waited for the drain timeout: {took:?}"
    );
    std::thread::sleep(Duration::from_millis(50));
    let log = daemon.log();
    assert!(
        log.iter().any(
            |l| l.contains("transactions still open at shutdown are dropped") && l.contains(&key)
        ),
        "{log:?}"
    );
    assert_eq!(open_transactions(&state)["open"], json!([key]));
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED));

    let _daemon = Daemon::start(store.path(), port, Some(&state), &[]);
    let refused = session.refused("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_lost_restart(&refused, &key);
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED));
}

#[test]
fn without_state_dir_sigterm_drains_and_exits_cleanly() {
    let store = store();
    let port = free_port();
    let mut daemon = Daemon::start(store.path(), port, None, &["--drain-timeout-secs", "30"]);
    let mut session = Session::open(port);
    session.ok("iwe_tx_begin", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    daemon.sigterm();
    daemon.wait_for_log("draining open transactions", Duration::from_secs(5));
    session.ok("iwe_tx_commit", json!({}));
    let (status, _) = daemon.wait_exit(Duration::from_secs(10));
    assert!(status.success());
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED_EDITED));
}

#[test]
fn state_dir_is_refused_for_stdio() {
    let store = store();
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_iwec"))
        .arg("--store")
        .arg(store.path())
        .arg("--state-dir")
        .arg(state_dir(&root))
        .stdin(Stdio::null())
        .output()
        .expect("run iwec");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--state-dir is only supported with --transport http"));
}

// ---------------------------------------------------------------------------
// B.3 The idle reaper
// ---------------------------------------------------------------------------

#[test]
fn an_idle_reaped_implicit_transaction_refuses_writes_instead_of_landing_them() {
    let store = store();
    let port = free_port();
    let daemon = Daemon::start(store.path(), port, None, &["--tx-idle-timeout-secs", "1"]);
    let mut session = Session::open(port);
    let key = format!("session:{}", session.id);
    session.ok("iwe_tx_begin", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    daemon.wait_for_log("force-aborted idle transaction", Duration::from_secs(10));

    let refused = session.refused("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert!(
        refused.contains(&format!(
            "transaction {key} was force-aborted after 1s idle at "
        )) && refused.contains("none of its staged writes were applied")
            && refused.contains("call iwe_tx_abort to acknowledge, then begin again"),
        "{refused}"
    );
    let refused = session.refused("iwe_tx_commit", json!({}));
    assert!(refused.contains("was force-aborted"), "{refused}");
    assert_eq!(
        read(store.path(), "seed").as_deref(),
        Some(SEED),
        "nothing landed"
    );

    session.ok("iwe_tx_abort", json!({}));
    session.ok("iwe_update", json!({"key": "seed", "content": SEED_EDITED}));
    assert_eq!(read(store.path(), "seed").as_deref(), Some(SEED_EDITED));
}

#[test]
fn an_idle_reaped_explicit_handle_is_named_in_the_refusal() {
    let store = store();
    let port = free_port();
    let daemon = Daemon::start(store.path(), port, None, &["--tx-idle-timeout-secs", "1"]);
    let mut session = Session::open(port);
    session.ok("iwe_tx_begin", json!({"handle": "slow"}));
    daemon.wait_for_log("force-aborted idle transaction", Duration::from_secs(10));
    let refused = session.refused(
        "iwe_create",
        json!({"handle": "slow", "key": "late", "content": "# Late\n"}),
    );
    assert!(
        refused.contains("transaction slow was force-aborted"),
        "{refused}"
    );
    assert!(read(store.path(), "late").is_none());
}
