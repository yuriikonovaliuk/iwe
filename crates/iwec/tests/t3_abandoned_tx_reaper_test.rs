//! T3 (milestone `iwec-shared-daemon`, effort
//! `crew-and-iwe-memory-footprint-reduction`): abandoned-transaction
//! reaper (last-touched + idle force-abort sweep).
//!
//! Built from the task's acceptance criteria and Shared surface alone —
//! `crates/iwec/src/lib.rs` and `crates/iwec/src/main.rs` were not read
//! while authoring this file. Every identifier used below (the MCP tools
//! `iwe_tx_begin` / `iwe_tx_commit` / `iwe_tx_abort` / `iwe_create` /
//! `iwe_check`, the "already open" / "no transaction is open" refusal
//! vocabulary, the `--transport` / `--host` / `--port` / `--store` CLI
//! flags, and the `--tx-idle-timeout-secs` flag named in the Shared
//! surface) comes from the contract's own text or from this crate's
//! already-committed, already-pinned test conventions
//! (`tests/http_transport_test.rs`, `tests/cross_process_locking_test.rs`,
//! `tests/handle_keyed_open_txs_test.rs`, `tests/store_flag_test.rs`).
//!
//! The reaper's exact sweep interval and exact log-message wording are
//! NOT pinned by the Shared surface, so this suite does not assert on
//! literal message text beyond the vocabulary the acceptance criteria
//! themselves use ("sweep", "force-abort") and does not assume a
//! particular interval — every wait below polls with a generous bound
//! instead of sleeping a hardcoded, interval-shaped duration.
//!
//! Contract map:
//!   - `staging_writes_refresh_the_idle_clock_...`          -> criterion 1
//!   - `idle_transaction_is_force_aborted_...`               -> criterion 2
//!   - `n_abandoned_transactions_do_not_accumulate_...`      -> criterion 3
//!   - `tx_idle_timeout_flag_defaults_to_600_seconds_in_help`
//!     + `without_the_override_flag_...`                     -> criterion 4
//!   - `stdio_mode_does_not_reap_...`                        -> criterion 5
//!   - log-line assertions embedded in criteria 2/3, plus
//!     `sweep_runs_periodically_producing_multiple_cycles_over_time`
//!                                                             -> criterion 6

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock,
    Implementation,
};
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Process + client plumbing, following the established conventions in
// `tests/cross_process_locking_test.rs` and `tests/store_flag_test.rs`.
// ---------------------------------------------------------------------------

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A minimal store: `[transactions] validate = "full"` (required for
/// `iwe_tx_begin` to be accepted at all, per
/// `tests/agent_transaction_test.rs`) plus one seed document so
/// `iwe_check` has something innocuous to report on.
fn store() -> TempDir {
    let dir = tempfile::tempdir().expect("temp store");
    std::fs::create_dir_all(dir.path().join(".iwe")).expect("create .iwe");
    std::fs::write(
        dir.path().join(".iwe/config.toml"),
        "[transactions]\nvalidate = \"full\"\n",
    )
    .expect("write config");
    std::fs::write(dir.path().join("seed.md"), "# Seed\n\nSeed document.\n")
        .expect("write seed");
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind port")
        .local_addr()
        .expect("port address")
        .port()
}

async fn wait_until_listening(addr: &str) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("iwec did not start listening on {addr}");
}

/// Spawn `iwec --transport http` against `store`, piping stderr into a
/// shared, continuously-drained log buffer so the sweep/force-abort log
/// lines (criterion 6) can be asserted on while the process keeps
/// running.
fn spawn_http(
    store: &Path,
    port: u16,
    tx_idle_timeout_secs: Option<u64>,
) -> (ServerProcess, Arc<Mutex<Vec<String>>>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_iwec"));
    command
        .arg("--transport")
        .arg("http")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--store")
        .arg(store);
    if let Some(secs) = tx_idle_timeout_secs {
        command.arg("--tx-idle-timeout-secs").arg(secs.to_string());
    }
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn HTTP iwec");

    let stderr = child.stderr.take().expect("piped stderr");
    let log = Arc::new(Mutex::new(Vec::new()));
    let log_writer = log.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            log_writer.lock().unwrap().push(line);
        }
    });

    (ServerProcess(child), log)
}

async fn http_client(addr: &str) -> RunningService<RoleClient, ClientInfo> {
    wait_until_listening(addr).await;
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t3-reaper-test", "0.0.1"),
    )
    .serve(StreamableHttpClientTransport::from_uri(format!(
        "http://{addr}/mcp"
    )))
    .await
    .expect("connect HTTP client")
}

async fn call(
    client: &RunningService<RoleClient, ClientInfo>,
    name: &'static str,
    arguments: serde_json::Value,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let args = arguments.as_object().cloned().unwrap_or_default();
    client
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args))
        .await
}

fn is_ok(result: &Result<CallToolResult, rmcp::ServiceError>) -> bool {
    matches!(result, Ok(r) if !r.is_error.unwrap_or(false))
}

fn text_of(result: &Result<CallToolResult, rmcp::ServiceError>) -> String {
    match result {
        Ok(r) => r
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Err(e) => e.to_string(),
    }
}

/// Poll `condition` until it returns true or `timeout` elapses; panics
/// with `message` on timeout. Used instead of a hardcoded sleep so these
/// tests do not encode an assumption about the reaper's sweep interval.
async fn wait_until<F, Fut>(timeout: Duration, mut condition: F, message: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if condition().await {
            return;
        }
        if start.elapsed() > timeout {
            panic!("{message}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn log_lines_containing(log: &Arc<Mutex<Vec<String>>>, needle_lower: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|line| line.to_lowercase().contains(needle_lower))
        .count()
}

// ---------------------------------------------------------------------------
// Criterion: "OpenTransaction records a last-touched timestamp, refreshed
// on every operation against the staged tx (begin, every staging write,
// every handle touch)."
//
// Observable proxy: with a short idle timeout, a transaction that is
// staged against repeatedly (each staging write being a "touch") survives
// well past the raw timeout, because each write resets the idle clock.
// If last-touched were NOT refreshed by staging writes, the transaction
// would already be gone by the second write below.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn staging_writes_refresh_the_idle_clock_so_an_actively_used_transaction_survives() {
    let dir = store();
    let port = free_port();
    // 10s (not the tighter 2s an unloaded machine could get away with):
    // each of the three gaps below must individually land under the idle
    // timeout, so the margin between the two has to absorb real
    // scheduling jitter -- both this client's own `tokio::time::sleep`
    // and the server's MCP round trip -- when another test suite is
    // contending for CPU on the same machine, not just cover the
    // nominal 1.2s/2s gap.
    let (_server, _log) = spawn_http(dir.path(), port, Some(10));
    let addr = format!("127.0.0.1:{port}");
    let client = http_client(&addr).await;

    let begun = call(&client, "iwe_tx_begin", json!({"handle": "alpha"})).await;
    assert!(is_ok(&begun), "begin must succeed: {}", text_of(&begun));

    // Three staging writes spaced 4s apart -- total elapsed (~12s)
    // exceeds the 10s configured idle timeout, but each write is itself
    // a touch, so the transaction must never have gone idle long enough
    // to be swept.
    for i in 0..3 {
        tokio::time::sleep(Duration::from_millis(4000)).await;
        let staged = call(
            &client,
            "iwe_create",
            json!({
                "handle": "alpha",
                "key": format!("touched-{i}"),
                "content": format!("# Touched {i}\n\nBody.\n"),
            }),
        )
        .await;
        assert!(
            is_ok(&staged),
            "staging write #{i} against an actively-touched transaction must succeed \
             (a naive un-refreshed idle clock would have force-aborted it by now): {}",
            text_of(&staged)
        );
    }

    // The transaction is still alive and holds all three staged writes:
    // committing it must succeed and land every file.
    let committed = call(&client, "iwe_tx_commit", json!({"handle": "alpha"})).await;
    assert!(
        is_ok(&committed),
        "commit of the still-alive, actively-touched transaction must succeed: {}",
        text_of(&committed)
    );
    for i in 0..3 {
        assert!(
            dir.path().join(format!("touched-{i}.md")).is_file(),
            "staged write #{i} must have landed on commit"
        );
    }
}

// ---------------------------------------------------------------------------
// Criterion: "In http serving mode a periodic background sweep
// force-aborts staged transactions idle longer than the configured
// timeout; force-abort has the same store-state semantics as an explicit
// abort (staged writes discarded, store left valid, schema validate
// passes after)."
//
// Also covers half of criterion 6 (a force-abort log line, a sweep-cycle
// log line).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_transaction_is_force_aborted_leaving_store_valid_and_check_passing() {
    let dir = store();
    let port = free_port();
    let (_server, log) = spawn_http(dir.path(), port, Some(1));
    let addr = format!("127.0.0.1:{port}");
    let client = http_client(&addr).await;

    let begun = call(&client, "iwe_tx_begin", json!({"handle": "ghost"})).await;
    assert!(is_ok(&begun), "begin must succeed: {}", text_of(&begun));
    let staged = call(
        &client,
        "iwe_create",
        json!({"handle": "ghost", "key": "ghost-doc", "content": "# Ghost\n\nBody.\n"}),
    )
    .await;
    assert!(is_ok(&staged), "staging write must succeed: {}", text_of(&staged));

    // Abandon it: no further touches -- crucially, that also means no
    // further `iwe_tx_begin` against "ghost" while we wait: criterion 1
    // (see `staging_writes_refresh_the_idle_clock_...` above) has the
    // server refresh the idle clock on *every* operation against an
    // already-open handle, a duplicate `iwe_tx_begin` included. Polling
    // via `iwe_tx_begin` the way an explicit-abort check naively would
    // must therefore never be used here -- it would touch the handle on
    // every failed attempt and the transaction would never go idle long
    // enough to be swept at all. Instead, poll the reaper's own
    // force-abort log line (criterion 6) -- a purely passive
    // observation that cannot itself refresh the idle clock.
    wait_until(
        Duration::from_secs(10),
        || {
            let log = &log;
            async move {
                log_lines_containing(log, "force-abort") > 0
                    || log_lines_containing(log, "force abort") > 0
            }
        },
        "the idle transaction was never force-aborted (no force-abort log line observed)",
    )
    .await;

    // Now that the sweep has already happened, a single, one-shot begin
    // confirms the observable consequence of the force-abort -- the
    // "ghost" slot is free again, exactly as an explicit `iwe_tx_abort`
    // would leave it -- without itself being part of the wait loop above.
    let reopened = call(&client, "iwe_tx_begin", json!({"handle": "ghost"})).await;
    assert!(
        is_ok(&reopened),
        "the 'ghost' handle must be free once the force-abort log line has appeared: {}",
        text_of(&reopened)
    );
    let _ = call(&client, "iwe_tx_abort", json!({"handle": "ghost"})).await;

    // Store-state semantics match an explicit abort: the staged write
    // was discarded, never landing on disk.
    assert!(
        !dir.path().join("ghost-doc.md").exists(),
        "a force-aborted transaction's staged write must be discarded, not land on disk"
    );

    // Store left valid: a schema check against the pre-existing seed
    // document still reports ok.
    let checked = call(&client, "iwe_check", json!({"keys": ["seed"]})).await;
    assert!(is_ok(&checked), "iwe_check must succeed: {}", text_of(&checked));
    let value: serde_json::Value =
        serde_json::from_str(&text_of(&checked)).expect("iwe_check result is JSON");
    assert_eq!(
        value[0]["ok"], true,
        "schema validate must pass after a force-abort, got: {value}"
    );

    // Criterion 6: both a sweep-cycle line and a force-abort line were
    // logged.
    assert!(
        log_lines_containing(&log, "sweep") > 0,
        "a sweep cycle must emit a log line, got log: {:?}",
        log.lock().unwrap()
    );
    assert!(
        log_lines_containing(&log, "force-abort") > 0 || log_lines_containing(&log, "force abort") > 0,
        "a force-abort must emit a log line, got log: {:?}",
        log.lock().unwrap()
    );
}

// ---------------------------------------------------------------------------
// Criterion: "No accumulation: test stages N transactions then abandons
// them; after the timeout (small override in test) open_txs is empty and
// the sweep is observable in logs."
// ---------------------------------------------------------------------------

#[tokio::test]
async fn n_abandoned_transactions_do_not_accumulate_and_are_all_reaped() {
    let dir = store();
    let port = free_port();
    let (_server, log) = spawn_http(dir.path(), port, Some(1));
    let addr = format!("127.0.0.1:{port}");
    let client = http_client(&addr).await;

    let handles = ["h1", "h2", "h3"];
    for handle in handles {
        let begun = call(&client, "iwe_tx_begin", json!({"handle": handle})).await;
        assert!(is_ok(&begun), "begin {handle} must succeed: {}", text_of(&begun));
        let staged = call(
            &client,
            "iwe_create",
            json!({
                "handle": handle,
                "key": format!("{handle}-doc"),
                "content": format!("# {handle}\n\nBody.\n"),
            }),
        )
        .await;
        assert!(is_ok(&staged), "staging write for {handle} must succeed: {}", text_of(&staged));
    }

    // Abandon all three. Poll until every handle's own force-abort has
    // been logged -- proof that abandoned transactions do not accumulate
    // (all N are reaped, not just the first one found). Poll the log,
    // never `iwe_tx_begin` against the handle itself: per criterion 1
    // (`staging_writes_refresh_the_idle_clock_...` above), the server
    // refreshes the idle clock on *every* operation against an
    // already-open handle, including a duplicate, refused
    // `iwe_tx_begin` -- polling that way would touch each handle on
    // every failed attempt and none of them would ever go idle long
    // enough to be swept.
    for handle in handles {
        wait_until(
            Duration::from_secs(10),
            || {
                let log = &log;
                async move {
                    log.lock().unwrap().iter().any(|line| {
                        let lower = line.to_lowercase();
                        (lower.contains("force-abort") || lower.contains("force abort"))
                            && line.contains(handle)
                    })
                }
            },
            &format!("handle '{handle}' was never freed -- abandoned transactions accumulated"),
        )
        .await;
    }

    // Now that every force-abort has already been logged, a single,
    // one-shot begin+abort per handle confirms the observable
    // consequence -- each slot is free again -- without itself being
    // part of the wait loop above.
    for handle in handles {
        let reopened = call(&client, "iwe_tx_begin", json!({"handle": handle})).await;
        assert!(
            is_ok(&reopened),
            "handle '{handle}' must be free once its force-abort log line has appeared: {}",
            text_of(&reopened)
        );
        let _ = call(&client, "iwe_tx_abort", json!({"handle": handle})).await;
    }

    // None of the staged writes landed.
    for handle in handles {
        assert!(
            !dir.path().join(format!("{handle}-doc.md")).exists(),
            "abandoned transaction {handle}'s staged write must have been discarded"
        );
    }

    // Each of the three force-aborts, plus at least one sweep cycle, was
    // logged.
    assert!(
        log_lines_containing(&log, "force-abort") + log_lines_containing(&log, "force abort") >= 3,
        "each of the {} abandoned transactions must emit its own force-abort log line, got log: {:?}",
        handles.len(),
        log.lock().unwrap()
    );
    assert!(
        log_lines_containing(&log, "sweep") > 0,
        "the sweep must be observable in logs, got log: {:?}",
        log.lock().unwrap()
    );
}

// ---------------------------------------------------------------------------
// Criterion: "Timeout: default 600s, overridable via CLI flag
// (--tx-idle-timeout-secs N); default existence and override both
// tested."
// ---------------------------------------------------------------------------

/// `--help` names the flag and its documented 600s default -- this is
/// the only feasible way to assert the default's *value* without a
/// 600-second-long test.
#[tokio::test]
async fn tx_idle_timeout_flag_defaults_to_600_seconds_in_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_iwec"))
        .arg("--help")
        .output()
        .expect("run --help");
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        help.contains("tx-idle-timeout-secs"),
        "--help must document --tx-idle-timeout-secs, got: {help}"
    );
    assert!(
        help.contains("600"),
        "--help must document the 600s default for --tx-idle-timeout-secs, got: {help}"
    );
}

/// The override changes actual behavior (proven by the two tests above,
/// which use a 1-2s override successfully). This test proves the
/// *default* is not itself already small: with no override, a
/// transaction abandoned for a few seconds is nowhere near the 600s
/// default and must remain open.
#[tokio::test]
async fn without_the_override_flag_a_briefly_idle_transaction_is_not_reaped() {
    let dir = store();
    let port = free_port();
    let (_server, _log) = spawn_http(dir.path(), port, None);
    let addr = format!("127.0.0.1:{port}");
    let client = http_client(&addr).await;

    let begun = call(&client, "iwe_tx_begin", json!({"handle": "patient"})).await;
    assert!(is_ok(&begun), "begin must succeed: {}", text_of(&begun));

    tokio::time::sleep(Duration::from_secs(3)).await;

    let message = call(&client, "iwe_tx_begin", json!({"handle": "patient"}))
        .await
        .expect_err("the default 600s timeout must not have reaped a 3s-idle transaction");
    assert!(
        message.to_string().contains("already open"),
        "the 'patient' handle must still be occupied under the default timeout, got: {message}"
    );

    let _ = call(&client, "iwe_tx_abort", json!({"handle": "patient"})).await;
}

// ---------------------------------------------------------------------------
// Criterion: "Reaper runs only in http mode; stdio behavior unchanged
// (test)."
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_mode_does_not_reap_idle_transactions_even_with_a_short_override() {
    let dir = store();

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_iwec"));
    command
        .arg("--transport")
        .arg("stdio")
        .arg("--store")
        .arg(dir.path())
        .arg("--tx-idle-timeout-secs")
        .arg("1");
    let client = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("iwec-t3-stdio-reaper-test", "0.0.1"),
    )
    .serve(TokioChildProcess::new(command).expect("spawn stdio iwec"))
    .await
    .expect("connect stdio client");

    let begun = call(&client, "iwe_tx_begin", json!({"handle": "stdio-handle"})).await;
    assert!(is_ok(&begun), "begin must succeed: {}", text_of(&begun));
    let staged = call(
        &client,
        "iwe_create",
        json!({"handle": "stdio-handle", "key": "stdio-doc", "content": "# S\n\nBody.\n"}),
    )
    .await;
    assert!(is_ok(&staged), "staging write must succeed: {}", text_of(&staged));

    // Wait well past the 1s override -- if a reaper wrongly ran in stdio
    // mode, this is more than enough time for it to have swept.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let message = call(&client, "iwe_tx_begin", json!({"handle": "stdio-handle"}))
        .await
        .expect_err("stdio mode must not run the reaper -- the handle must still be occupied");
    assert!(
        message.to_string().contains("already open"),
        "the 'stdio-handle' must remain occupied in stdio mode past the override timeout, got: {message}"
    );

    // Store-state proof: the original staged write is still intact and
    // commits cleanly.
    let committed = call(&client, "iwe_tx_commit", json!({"handle": "stdio-handle"})).await;
    assert!(
        is_ok(&committed),
        "the untouched-by-any-reaper transaction must still commit: {}",
        text_of(&committed)
    );
    assert!(
        dir.path().join("stdio-doc.md").is_file(),
        "the staged write must have survived the idle window in stdio mode"
    );
}

// ---------------------------------------------------------------------------
// Criterion 6 (periodicity leg): "Each sweep cycle ... emit[s] a log
// line" -- proven independently of any reaping, by observing more than
// one cycle over a window several times the configured timeout.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sweep_runs_periodically_producing_multiple_cycles_over_time() {
    let dir = store();
    let port = free_port();
    let (_server, log) = spawn_http(dir.path(), port, Some(1));
    let addr = format!("127.0.0.1:{port}");
    wait_until_listening(&addr).await;

    // No transaction is ever opened -- this isolates the sweep's own
    // periodic logging from any force-abort logging.
    tokio::time::sleep(Duration::from_secs(6)).await;

    assert!(
        log_lines_containing(&log, "sweep") >= 2,
        "the background sweep must run periodically, producing more than one \
         sweep-cycle log line over a 6s window against a 1s timeout, got log: {:?}",
        log.lock().unwrap()
    );
}
