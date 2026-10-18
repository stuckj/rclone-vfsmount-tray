//! The tray client, in two shapes from one binary.
//!
//! With a subcommand it is a scriptable D-Bus client — the surface the integration tests
//! drive (#38, #54) and the way to work over SSH with no panel present. With none it raises
//! the StatusNotifierItem icon and stays up.
//!
//! Neither shape holds a mount or starts the service on its own; both reach it through
//! [`link`]. See DESIGN.md, "The lifetime rule".

mod client;
#[cfg(test)]
mod fixtures;
mod link;
mod menu;
mod model;
mod sni;
mod watch;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

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
pub(crate) enum Command {
    /// List configured mounts and their state.
    List,
    /// Mount one configured mount.
    Mount { name: String },
    /// Unmount one mount. Refused while anything is still using it unless `--force`.
    Unmount {
        name: String,
        /// Unmount even while the mount is in use. A file being written is severed
        /// mid-write, and rclone later uploads the partial file as though complete.
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

/// Set up logging, or return the exit code for a bad `--log-level`.
fn init_logging(log_level: Option<&str>) -> Result<(), ExitCode> {
    let filter =
        rvt_core::resolve_log_filter(log_level, std::env::var("RUST_LOG").ok()).map_err(|bad| {
            eprintln!(
                "--log-level must be one of: off, error, warn, info, debug, trace (got {bad:?}). \
                 Use RUST_LOG for per-target directives."
            );
            // Clap uses 2 for a usage error; a bad flag value is the same kind of mistake.
            ExitCode::from(2)
        })?;
    // Diagnostics go to stderr so a subcommand's stdout stays clean for `--json` and pipes.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if let Err(code) = init_logging(args.log_level.as_deref()) {
        return code;
    }

    let Some(cmd) = args.command else {
        return run_tray().await;
    };

    // The connect failure is carried into `execute` rather than unwrapped here, so that
    // `status --json` can still emit a document saying the service is unreachable.
    let conn = zbus::Connection::session()
        .await
        .map_err(link::LinkError::NoSessionBus);

    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let result = client::execute(conn, &cmd, &mut stdout).await;
    let _ = stdout.flush();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `status --json` has already written a JSON document describing the failure; a
            // second copy on stderr would be noise a script did not ask for.
            if !matches!(cmd, Command::Status { json: true }) {
                eprintln!("{}", e.message());
            }
            ExitCode::from(e.exit_code())
        }
    }
}

/// Raise the icon and stay up until the user or the session ends it.
///
/// The link to the service is not waited for: the icon appears whether or not the service is
/// running, and says which (#52).
async fn run_tray() -> ExitCode {
    let (actions, queue) = tokio::sync::mpsc::unbounded_channel();
    let model = model::TrayModel::new(actions);

    let handle = tokio::select! {
        h = raise(model) => match h {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Could not raise the tray icon: {e}");
                return ExitCode::from(5);
            }
        },
        // Still interruptible while it is waiting for a panel.
        _ = terminated() => return ExitCode::SUCCESS,
    };

    tokio::select! {
        _ = watch::run(handle.clone(), queue) => {}
        _ = terminated() => { handle.shutdown().await; }
    }
    ExitCode::SUCCESS
}

/// Put the icon up, waiting for a panel that can take it.
///
/// `ksni` routes only a `StatusNotifierWatcher` that is absent altogether to
/// [`ksni::Tray::watcher_offline`]; a watcher that owns the name but is not yet serving the
/// object, or is answering slowly, or is being replaced, comes back as an error from `spawn`.
/// At login that is the ordinary shape of a desktop still starting up, and there is nothing
/// yet to restart the tray if it gives up — so only a session bus that cannot be reached at
/// all is fatal.
async fn raise(model: model::TrayModel) -> Result<ksni::Handle<model::TrayModel>, ksni::Error> {
    use ksni::TrayMethods as _;

    let mut backoff = link::Backoff::new();
    // `spawn` takes the tray, so each attempt needs its own — built on the one queue, which
    // the menu's callbacks send to whichever attempt eventually succeeds.
    let queue = model.actions();
    let mut attempt = model;
    loop {
        // `assume_sni_available`: a panel that starts after us, or a shell restarting, must
        // leave the icon waiting rather than a process that gave up (#25).
        match attempt.assume_sni_available(true).spawn().await {
            Ok(handle) => return Ok(handle),
            Err(e @ ksni::Error::Dbus(_)) => return Err(e),
            Err(e) => {
                let wait = backoff.take();
                tracing::warn!(error = %e, ?wait, "could not register the icon yet; retrying");
                tokio::time::sleep(wait).await;
                attempt = model::TrayModel::new(queue.clone());
            }
        }
    }
}

/// Resolves on `SIGTERM` or `SIGINT`.
///
/// The session sends `SIGTERM` at logout and kills whatever has not gone shortly after, so a
/// tray that ignores it is killed rather than closed.
async fn terminated() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        // Nothing can be done about a handler that will not install, and exiting over it
        // would be worse than running without one.
        Err(e) => {
            tracing::warn!(error = %e, "no SIGTERM handler; the tray will be killed at logout");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("asked to stop; closing the icon and leaving every mount alone");
}

/// The man page and shell completions under `docs/` and `completions/` are generated from
/// [`Args`]; this proves the committed copies still match it, so a CLI change that forgets to
/// regenerate fails here rather than shipping stale docs. Regenerate with
/// `REGENERATE_CLI_DOCS=1 cargo test -p rclone-vfsmount-tray --bin rclone-vfsmount-tray`.
#[cfg(test)]
mod cli_docs {
    use clap::CommandFactory;
    use clap_complete::Shell;

    const BIN: &str = "rclone-vfsmount-tray";

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
