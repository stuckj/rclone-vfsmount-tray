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

    /// Log verbosity. Takes precedence over `RUST_LOG`; defaults to `info`.
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Stay in the foreground and log to stderr. Useful when running outside
    /// systemd during development.
    #[arg(long)]
    foreground: bool,
}

/// Resolve the log filter: explicit flag, else `RUST_LOG`, else `info`.
///
/// The flag is validated as a bare level and rejected otherwise. `EnvFilter`'s
/// grammar treats an unrecognised bare word as a *target* filter, so
/// `--log-level verbose` parses successfully, matches no target, and silences the
/// process completely — no output, no warning, exit code 0. A typo must not turn a
/// service into a silent one.
///
/// `RUST_LOG` keeps the full directive grammar, which is what people expect of it.
fn resolve_log_filter(flag: Option<&str>) -> anyhow::Result<String> {
    match flag {
        Some(level) => {
            level
                .parse::<tracing_subscriber::filter::LevelFilter>()
                .map_err(|_| {
                    anyhow::anyhow!(
                        "--log-level must be one of: off, error, warn, info, debug, trace \
                         (got {level:?}). Use RUST_LOG for per-target directives."
                    )
                })?;
            Ok(level.to_string())
        }
        None => Ok(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())),
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter = resolve_log_filter(args.log_level.as_deref())?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");
    tracing::warn!("not implemented yet — see issues #12, #17 and #40");
    Ok(())
}
