//! The tray client. With a subcommand it is a scriptable D-Bus client — the surface the
//! integration tests drive (#38, #54) and the way to work over SSH with no panel present.
//! With none, it will one day raise the StatusNotifierItem icon (#25, #26); until that is
//! built it says so and exits.
//!
//! The subcommands talk to the service and to nothing else: they neither hold a mount nor
//! start the service. See [`client`] for how a stopped or mismatched service is handled.

mod client;

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
        tracing::warn!("tray not implemented yet — see issues #25, #26 and #52");
        return ExitCode::SUCCESS;
    };

    // The connect failure is carried into `execute` rather than unwrapped here, so that
    // `status --json` can still emit a document saying the service is unreachable.
    let conn = zbus::Connection::session()
        .await
        .map_err(client::CliError::NoSessionBus);

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
