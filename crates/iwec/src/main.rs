use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use diwe::config::{load_config, load_config_in, ValidationScope};
use iwec::durable::{self, FileSessionStore, SESSION_MAX_AGE};
use iwec::{explicit_store_root, IweServer};
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::store::SessionStore;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[derive(Debug, Parser)]
#[command(name = "iwec", version = liwe::VERSION, about = "IWE MCP server")]
struct Cli {
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 8000)]
    port: u16,

    /// Explicit initialized IWE store root, overriding cwd-based discovery.
    #[arg(long, value_name = "PATH")]
    store: Option<PathBuf>,

    /// Force-abort HTTP transactions left untouched for this many seconds.
    #[arg(long, default_value_t = 600, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    tx_idle_timeout_secs: u64,

    /// HTTP only: keep MCP sessions and the open-transaction record under
    /// PATH, so a restart is invisible to connected clients.
    #[arg(long, value_name = "PATH")]
    state_dir: Option<PathBuf>,

    /// HTTP only: on SIGTERM/SIGINT, wait up to N seconds for open
    /// transactions to commit or abort before exiting.
    #[arg(long, default_value_t = 30, value_name = "N")]
    drain_timeout_secs: u64,
}

/// Resolves on SIGINT or SIGTERM (systemd's stop signal), naming it.
async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            },
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM; only SIGINT stops the server");
                tokio::signal::ctrl_c().await.ok();
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        "ctrl-c"
    }
}

/// After a stop signal: refuse new transactions, keep serving everything
/// else, and wait up to `timeout` for the open ones to commit or abort. A
/// second stop signal cuts the wait short. Whatever is still open is left in
/// the state dir's record, so the next start tombstones it.
async fn drain_transactions(server: &IweServer, signal: &str, timeout: Duration, persisted: bool) {
    server.begin_draining();
    let initial = server.open_transaction_keys();
    tracing::info!(
        signal,
        open = ?initial,
        drain_timeout_secs = timeout.as_secs(),
        "stop signal received; refusing iwe_tx_begin and draining open transactions"
    );
    let deadline = Instant::now() + timeout;
    let second_signal = stop_signal();
    tokio::pin!(second_signal);
    loop {
        if server.open_transaction_keys().is_empty() || Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            again = &mut second_signal => {
                tracing::warn!(signal = again, "second stop signal; ending the drain early");
                break;
            }
        }
    }
    let remaining = server.open_transaction_keys();
    let drained: Vec<&String> = initial.iter().filter(|key| !remaining.contains(key)).collect();
    tracing::info!(drained = ?drained, "transactions closed during drain");
    if remaining.is_empty() {
        tracing::info!("drain complete; no transaction dropped");
    } else if persisted {
        tracing::warn!(
            dropped = ?remaining,
            "transactions still open at shutdown are dropped; the next start refuses their writes until acknowledged"
        );
    } else {
        tracing::warn!(
            dropped = ?remaining,
            "transactions still open at shutdown are dropped (no --state-dir: they are not remembered)"
        );
    }
}

fn main() -> Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // The variable is erased from the setuid sudo process's environment by glibc before
        // sudo builds the child's environment, so it never arrives at iwec. The in-binary
        // mallopt bypasses this limitation.
        let arena_max = match std::env::var("IWEC_MALLOC_ARENA_MAX") {
            Ok(val) if val == "0" => None,
            Ok(val) => val.parse::<i32>().ok(),
            Err(_) => Some(2),
        };
        if let Some(max) = arena_max {
            unsafe { libc::mallopt(libc::M_ARENA_MAX, max) };
        }
    }

    runtime_main()
}

#[tokio::main]
async fn runtime_main() -> Result<()> {
    let cli = Cli::parse();

    if env::var("IWE_DEBUG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    }

    tracing::info!("starting IWE MCP server");

    let current_dir = env::current_dir().expect("current dir");
    let server = match cli.store {
        Some(store) => {
            let store = explicit_store_root(&store).unwrap_or_else(|e| {
                eprintln!("Error: {e}");
                std::process::exit(1);
            });
            let mut configuration = load_config_in(&store).unwrap_or_else(|e| {
                eprintln!("Error: {e}");
                std::process::exit(1);
            });
            // An explicitly selected daemon store supports the staged-write
            // protocol even when its otherwise-valid config has an empty
            // `[transactions]` section.
            if configuration.transactions.validate == ValidationScope::None {
                configuration.transactions.validate = ValidationScope::Full;
            }
            IweServer::new_at_store(store, &configuration)
        }
        None => {
            let configuration = load_config().unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });
            IweServer::new(&current_dir.to_string_lossy(), &configuration)
        }
    };
    if cli.state_dir.is_some() && cli.transport != Transport::Http {
        eprintln!("Error: --state-dir is only supported with --transport http");
        std::process::exit(1);
    }

    server.start_watching();

    match cli.transport {
        Transport::Stdio => {
            let service = server.serve(stdio()).await.inspect_err(|e| {
                tracing::error!("serving error: {:?}", e);
            })?;
            service.waiting().await?;
        }
        Transport::Http => {
            let session_store: Option<Arc<FileSessionStore>> = match &cli.state_dir {
                Some(state_dir) => {
                    let (sessions, txs) = durable::open_state_dir(state_dir).unwrap_or_else(|e| {
                        eprintln!("Error: cannot use state dir {}: {e}", state_dir.display());
                        std::process::exit(1);
                    });
                    let pruned = sessions.prune(SESSION_MAX_AGE).unwrap_or_else(|e| {
                        eprintln!("Error: cannot prune {}: {e}", sessions.dir().display());
                        std::process::exit(1);
                    });
                    let txs_path = txs.path().to_path_buf();
                    let lost = server.enable_tx_state(txs).unwrap_or_else(|e| {
                        eprintln!("Error: cannot use {}: {e}", txs_path.display());
                        std::process::exit(1);
                    });
                    tracing::info!(
                        state_dir = %state_dir.display(),
                        pruned_sessions = pruned,
                        "persistent MCP sessions enabled"
                    );
                    for lost in &lost {
                        tracing::warn!(
                            handle = %lost.key,
                            cause = ?lost.cause,
                            at = %lost.at,
                            "lost transaction: writes and commits under this handle are refused until iwe_tx_abort"
                        );
                    }
                    Some(Arc::new(sessions))
                }
                None => None,
            };
            let bind_address = format!("{}:{}", cli.host, cli.port);
            let cancellation = CancellationToken::new();
            let reaper_cancellation = cancellation.child_token();
            let reaper_server = server.clone();
            let idle_timeout = Duration::from_secs(cli.tx_idle_timeout_secs);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(idle_timeout);
                loop {
                    tokio::select! {
                        _ = reaper_cancellation.cancelled() => break,
                        _ = interval.tick() => {
                            reaper_server.reap_idle_transactions(idle_timeout).await;
                        }
                    }
                }
            });
            let mut config = StreamableHttpServerConfig::default()
                .with_cancellation_token(cancellation.child_token());
            config.session_store = session_store
                .clone()
                .map(|store| store as Arc<dyn SessionStore>);
            let drain_server = server.clone();
            let service = StreamableHttpService::new(
                move || Ok(server.clone()),
                LocalSessionManager::default().into(),
                config,
            );
            let mut router = axum::Router::new().nest_service("/mcp", service);
            if let Some(store) = session_store.clone() {
                // Session IDs name files: refuse a malformed one before rmcp
                // sees it, and count each request as activity on its session
                // so startup pruning removes only sessions idle for 7 days.
                router = router.layer(axum::middleware::from_fn(
                    move |request: axum::extract::Request, next: axum::middleware::Next| {
                        let store = store.clone();
                        async move {
                            use axum::response::IntoResponse;
                            if let Some(value) = request.headers().get("mcp-session-id") {
                                match value.to_str() {
                                    Ok(id) if durable::valid_session_id(id) => store.touch(id),
                                    _ => {
                                        return (
                                            http::StatusCode::BAD_REQUEST,
                                            "Bad Request: invalid Mcp-Session-Id",
                                        )
                                            .into_response();
                                    }
                                }
                            }
                            next.run(request).await
                        }
                    },
                ));
            }
            let listener = tokio::net::TcpListener::bind(&bind_address).await?;
            tracing::info!("listening on http://{}/mcp", bind_address);
            let drain_timeout = Duration::from_secs(cli.drain_timeout_secs);
            let persisted = cli.state_dir.is_some();
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let signal = stop_signal().await;
                    drain_transactions(&drain_server, signal, drain_timeout, persisted).await;
                    cancellation.cancel();
                })
                .await?;
            tracing::info!("IWE MCP server stopped");
        }
    }

    Ok(())
}
