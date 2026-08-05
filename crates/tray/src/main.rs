//! `rclone-vfsmount-tray` — the tray icon.
//!
//! A StatusNotifierItem client. It renders the state the service publishes and
//! sends commands back over D-Bus. It holds no mounts and owns no rclone processes:
//! quitting it closes an icon, nothing more.
//!
//! Native SNI (via `ksni`) is the only thing that works on KDE Plasma 6 under
//! Wayland — legacy XEmbed is not an option there.
//!
//! The subcommands below are also how the integration tests drive the system, so
//! they must work with no tray host present at all.
//!
//! Scaffolding only — the tray itself lands with #25, #26 and #52.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rclone-vfsmount-tray",
    version,
    about = "System tray client for rclone VFS mounts"
)]
struct Args {
    /// Log verbosity. Takes precedence over `RUST_LOG`; defaults to `info`.
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List configured mounts and their state.
    List,
    /// Mount one configured mount.
    Mount { name: String },
    /// Unmount one mount. Refused when uploads are still pending unless `--force`.
    Unmount {
        name: String,
        /// Unmount even with unuploaded data in the write-back cache. This can lose
        /// data that has not reached the remote.
        #[arg(long)]
        force: bool,
    },
    /// Print mount and transfer state.
    Status {
        /// Emit JSON. This is the stable surface for scripting and tests.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter =
        rvt_core::resolve_log_filter(args.log_level.as_deref(), std::env::var("RUST_LOG").ok())
            .map_err(|bad| {
                anyhow::anyhow!(
                "--log-level must be one of: off, error, warn, info, debug, trace (got {bad:?}). \
                 Use RUST_LOG for per-target directives."
            )
            })?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();

    match args.command {
        Some(cmd) => tracing::warn!(?cmd, "not implemented yet — see issue #37"),
        None => tracing::warn!("tray not implemented yet — see issues #25, #26 and #52"),
    }
    Ok(())
}
