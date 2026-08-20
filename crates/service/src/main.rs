//! The service that owns the mounts. A headless systemd **user** service: starts and
//! stops rclone mounts, polls VFS state, serves D-Bus to the tray and GTK clients.
//!
//! Mounts outlive it — restarting the service leaves them up. See DESIGN.md.

mod dbus;
mod poller;
mod registry;
mod supervisor;
mod systemd;
mod watch;

use clap::{Parser, Subcommand};
use registry::Change;
use std::sync::Arc;

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

    /// Accepted and ignored. It will mean "stay in the foreground and log to stderr" once
    /// there is a background mode to opt out of; today that is all the service does.
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

/// Resolves on `SIGTERM` — what systemd sends — or on `Ctrl-C` when run by hand.
async fn stop_requested() -> anyhow::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        result = tokio::signal::ctrl_c() => result?,
    }
    Ok(())
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
    let config = Arc::new(rvt_core::Config::load(&config_path)?);
    let runtime_dir = runtime_dir()?;

    if let Some(Command::PrepareMount { name }) = &args.command {
        // Runs while systemd waits on it, so it must not ask systemd anything.
        supervisor::prepare_for_start(
            &config,
            &runtime_dir,
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
    let sup = Arc::new(supervisor::SystemdSupervisor::new(
        config.clone(),
        rclone.path().to_path_buf(),
        units,
        runtime_dir.clone(),
        config_path.clone(),
    ));

    let registry = Arc::new(tokio::sync::Mutex::new(registry::Registry::default()));
    let nudge = Arc::new(watch::Nudge::default());

    let mut watcher = watch::Watcher::new(
        sup.clone(),
        registry.clone(),
        config.clone(),
        runtime_dir,
        nudge.clone(),
    );

    // Swept before the name goes up, and the changes dropped because nobody can be
    // listening yet. A client that connected first would find an empty list and no way to
    // tell it from "no mounts configured". A client that cannot connect at all has to cope
    // with that anyway (#52), so the wait is the honest place to spend it — and it is a
    // wait: a mount point that has wedged makes this tens of seconds (#103).
    let (found, _) = watcher.tick().await;
    for change in &found {
        report(change);
    }

    let conn = dbus::serve(dbus::MountManager::new(
        sup,
        registry,
        config,
        nudge,
        rclone.version().to_string(),
    ))
    .await?;
    tracing::info!(name = rvt_core::ipc::BUS_NAME, "serving");

    let emitter = zbus::object_server::SignalEmitter::new(&conn, rvt_core::ipc::OBJECT_PATH)?;

    tokio::select! {
        _ = watcher.run(emitter, report) => {}
        result = stop_requested() => result?,
    }

    // Nothing is unmounted on the way out. See DESIGN.md, "The lifetime rule".
    tracing::info!("stopping; mounts are left as they are");
    Ok(())
}

/// What each change is worth in the journal.
///
/// Transfers move constantly and would drown everything else, so only the mount set is
/// reported at `info`.
fn report(change: &Change) {
    match change {
        Change::Mount(view) => {
            tracing::info!(mount = %view.name, state = %view.state, "mount state")
        }
        Change::Removed(name) => tracing::info!(mount = %name, "no longer listed"),
        Change::CapabilityTier => tracing::info!("capability tier resolved"),
        Change::Transfer(view) => tracing::debug!(
            mount = %view.mount,
            fidelity = ?view.fidelity,
            pending_files = view.pending_files,
            pending_bytes = view.pending_known_bytes,
            "outstanding"
        ),
    }
}

/// The man page and shell completions under `docs/` and `completions/` are generated from
/// [`Args`]; this proves the committed copies still match it, so a CLI change that forgets to
/// regenerate fails here rather than shipping stale docs. Regenerate with
/// `REGENERATE_CLI_DOCS=1 cargo test -p rclone-vfsmount-trayd --bin rclone-vfsmount-trayd`.
#[cfg(test)]
mod cli_docs {
    use clap::CommandFactory;
    use clap_complete::Shell;

    const BIN: &str = "rclone-vfsmount-trayd";

    /// Every generated file, as (path relative to the workspace root, its bytes).
    fn artifacts() -> Vec<(String, Vec<u8>)> {
        let cmd = crate::Args::command();

        let mut man = Vec::new();
        clap_mangen::Man::new(cmd.clone())
            .render(&mut man)
            .expect("render man page");
        let mut out = vec![(format!("docs/{BIN}.1"), man)];

        for (shell, path) in [
            (Shell::Bash, format!("completions/bash/{BIN}")),
            (Shell::Zsh, format!("completions/zsh/_{BIN}")),
            (Shell::Fish, format!("completions/fish/{BIN}.fish")),
        ] {
            let mut buf = Vec::new();
            clap_complete::generate(shell, &mut cmd.clone(), BIN, &mut buf);
            out.push((path, buf));
        }
        out
    }

    /// `crates/<crate>/` sits two levels below the workspace root, where the committed
    /// artifacts live.
    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root above crates/<crate>")
            .to_path_buf()
    }

    #[test]
    fn committed_man_and_completions_match_the_cli() {
        let root = workspace_root();
        let regenerate = std::env::var_os("REGENERATE_CLI_DOCS").is_some();
        let mut stale = Vec::new();

        for (rel, bytes) in artifacts() {
            let path = root.join(&rel);
            if regenerate {
                std::fs::create_dir_all(path.parent().unwrap()).expect("create output dir");
                std::fs::write(&path, &bytes).expect("write artifact");
            } else if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                stale.push(rel);
            }
        }

        assert!(
            regenerate || stale.is_empty(),
            "committed CLI docs are stale ({stale:?}). Regenerate with \
             `REGENERATE_CLI_DOCS=1 cargo test -p {BIN} --bin {BIN}`."
        );
    }
}
