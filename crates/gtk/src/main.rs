//! The windows: a GTK4 client of the service, like the tray. Opening or closing it has
//! no effect on mounts.
//!
//! Excluded from the workspace `default-members` — it is the only crate that will link
//! a system C library. Scaffolding only; see #41 to #44.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "rclone-vfsmount-tray-gtk",
    version,
    about = "Configuration and transfer windows for rclone VFS mounts"
)]
struct Args {
    /// Open directly to a named pane, e.g. `mounts`, `transfers`, `settings`.
    #[arg(long, value_name = "PANE")]
    pane: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    eprintln!(
        "not implemented yet — see issues #41 to #44 (pane: {:?})",
        args.pane
    );
    Ok(())
}
