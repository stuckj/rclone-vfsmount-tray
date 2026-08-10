//! The service that owns the mounts. A headless systemd **user** service: starts and
//! stops rclone mounts, polls VFS state, serves D-Bus to the tray and GTK clients.
//!
//! Mounts outlive it — restarting the service leaves them up. See DESIGN.md.
//!
//! Scaffolding only; see #12, #17, #40.

mod poller;
mod supervisor;
mod systemd;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rclone-vfsmount-trayd",
    version,
    about = "Service that owns rclone VFS mounts and serves their state over D-Bus"
)]
pub struct Args {
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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Clear what a hard-killed rclone left behind, so its unit can start.
    ///
    /// Run from each mount unit's `ExecStartPre`. See DESIGN.md, "Delegated restart needs
    /// a pre-start hook to work at all".
    #[command(hide = true)]
    PrepareMount {
        #[arg(long)]
        name: String,
    },
}

/// Where rc sockets go. `XDG_RUNTIME_DIR` is per-user and mode 0700, which is what keeps
/// the sockets unreachable by other logins; `/tmp` is not a substitute, so its absence is
/// an error rather than something to paper over with a fallback.
fn runtime_dir() -> anyhow::Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "XDG_RUNTIME_DIR is not set. It is normally provided by the session; \
                 running under systemd --user supplies it."
            )
        })?;
    Ok(std::path::PathBuf::from(base).join("rclone-vfsmount-tray"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let config_path = match &args.config {
        // Absolutised: this path is baked into every mount unit's `ExecStartPre`, and
        // systemd runs that with the user manager's working directory, not this one.
        Some(p) => std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()),
        None => rvt_core::Config::default_path()?,
    };
    let config = std::sync::Arc::new(rvt_core::Config::load(&config_path)?);

    if let Some(Command::PrepareMount { name }) = &args.command {
        // Runs while systemd waits on it, so it must not ask systemd anything.
        supervisor::prepare_for_start(
            &config,
            &runtime_dir()?,
            std::path::Path::new("/proc/self/mountinfo"),
            name,
        )
        .await?;
        return Ok(());
    }

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");
    let rclone = rvt_core::Rclone::discover(config.global.rclone_path.as_deref())?;
    tracing::info!(path = %rclone.path().display(), version = %rclone.version(), "found rclone");

    let units = systemd::dbus::SystemdUnits::connect().await?;
    let sup = supervisor::SystemdSupervisor::new(
        config.clone(),
        rclone.path().to_path_buf(),
        units,
        runtime_dir()?,
        config_path.clone(),
    );

    // Reconcile before doing anything else. The service may have restarted while its
    // mounts stayed up, and a mount somebody else started is still worth reporting.
    let found = rvt_core::supervisor::MountSupervisor::reconcile(&sup).await?;
    for m in &found {
        tracing::info!(name = %m.name, state = ?m.state, "found mount");
    }

    // One poll per mount we started, so the tier and what is outstanding are visible in
    // the log before there is anywhere to publish them. The loop that keeps this current,
    // and the D-Bus surface that serves it, are #40.
    //
    // `Mounted` rather than `is_live()`, which also admits `Foreign`. We did not start a
    // foreign mount, so none of our rc sockets addresses it and its own is unknown by
    // definition. Reaching it at all is #70.
    for m in found
        .iter()
        .filter(|m| matches!(m.state, rvt_core::supervisor::MountState::Mounted))
    {
        let socket = sup.socket_path(&m.name);
        let mut p = poller::MountPoller::connect(&m.name, rvt_core::RcClient::new(&socket)).await;
        tracing::debug!(mount = %m.name, tier = ?p.tier(), "resolved capability tier");
        let state = p.poll().await;
        tracing::info!(
            mount = %state.mount,
            tier = ?state.fidelity,
            outstanding_known = state.outstanding_known,
            pending_files = state.pending.files,
            pending_bytes = state.pending.known_bytes,
            degraded = ?state.degraded_reason,
            next_poll_secs = poller::MountPoller::interval(&state).as_secs(),
            "polled"
        );
    }

    tracing::warn!("serving is not implemented yet — see issue #40");
    Ok(())
}
