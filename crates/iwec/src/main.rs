use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use diwe::config::{load_config, load_config_in, ValidationScope};
use iwec::{explicit_store_root, IweServer};
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
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
#[command(name = "iwec", version, about = "IWE MCP server")]
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
    server.start_watching();

    match cli.transport {
        Transport::Stdio => {
            let service = server.serve(stdio()).await.inspect_err(|e| {
                tracing::error!("serving error: {:?}", e);
            })?;
            service.waiting().await?;
        }
        Transport::Http => {
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
            let service = StreamableHttpService::new(
                move || Ok(server.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_cancellation_token(cancellation.child_token()),
            );
            let router = axum::Router::new().nest_service("/mcp", service);
            let listener = tokio::net::TcpListener::bind(&bind_address).await?;
            tracing::info!("listening on http://{}/mcp", bind_address);
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    tokio::signal::ctrl_c().await.ok();
                    cancellation.cancel();
                })
                .await?;
        }
    }

    Ok(())
}
