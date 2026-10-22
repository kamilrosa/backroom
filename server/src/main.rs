//! Backroom server — command line entry point.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(version, about = "Backroom voice chat server")]
struct Args {
    /// Address and port to listen on (UDP).
    #[arg(long, env = "BACKROOM_BIND", default_value = "0.0.0.0:7777")]
    bind: SocketAddr,
    /// Shared password for everyone. Empty = no password.
    #[arg(
        long,
        env = "BACKROOM_PASSWORD",
        default_value = "",
        hide_env_values = true
    )]
    password: String,
    /// Room name shown in clients.
    #[arg(long, env = "BACKROOM_NAME", default_value = "Backroom")]
    name: String,
    /// Directory for the server certificate (must survive restarts, otherwise
    /// clients will see a fingerprint-change warning).
    #[arg(long, env = "BACKROOM_DATA", default_value = "./data")]
    data_dir: PathBuf,
    /// Maximum number of people in the room.
    #[arg(long, env = "BACKROOM_MAX_USERS", default_value_t = 16)]
    max_users: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();
    let a = Args::parse();
    if a.password.is_empty() {
        warn!("Server is running WITHOUT a password — anyone who knows the address can join.");
    }
    let server = backroom_server::start(backroom_server::Settings {
        bind: a.bind,
        password: a.password,
        name: a.name.clone(),
        data_dir: a.data_dir,
        max_users: a.max_users,
    })?;
    info!("Certificate fingerprint: {}", server.fingerprint);
    info!("Backroom \"{}\" listening on {} (UDP)", a.name, server.addr);

    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let terminate = term.recv();
    #[cfg(not(unix))]
    let terminate = std::future::pending::<Option<()>>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
    }
    info!("Shutting down");
    server.shutdown().await;
    Ok(())
}
