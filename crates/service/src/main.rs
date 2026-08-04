//! `rclone-vfsmount-trayd` — the service that owns the mounts.
//!
//! Runs as a systemd **user** service, headless. It starts and stops rclone mounts,
//! polls their VFS write-back state, and serves both over D-Bus to the tray and GTK
//! clients.
//!
//! Mounts belong to this service, and their lifetime is deliberately decoupled even
//! from it: restarting the service (a package upgrade, say) leaves mounts up. See
//! `DESIGN.md`.
//!
//! Scaffolding only — the real implementation lands with the supervisor (#17),
//! the rc client (#12) and the D-Bus interface (#40).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "rclone-vfsmount-trayd",
    version,
    about = "Service that owns rclone VFS mounts and serves their state over D-Bus"
)]
struct Args {
    /// Path to the configuration file. Defaults to
    /// `$XDG_CONFIG_HOME/rclone-vfsmount-tray/config.toml`.
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Log verbosity. Overridden by `RUST_LOG` when that is set.
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Stay in the foreground and log to stderr. Useful when running outside
    /// systemd during development.
    #[arg(long)]
    foreground: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| args.log_level.clone());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");
    tracing::warn!("not implemented yet — see issues #12, #17 and #40");
    Ok(())
}
