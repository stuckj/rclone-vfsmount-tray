//! `rclone-vfsmount-tray-gtk` — the windows.
//!
//! A GTK4 client, and like the tray a pure consumer of the service over D-Bus.
//! Opening and closing it has no effect on what is mounted.
//!
//! This crate is excluded from the workspace's `default-members` because it is the
//! only one that links system C libraries; see the root `Cargo.toml`.
//!
//! Scaffolding only — the `gtk4` dependency and the actual windows land with #41,
//! #42, #43 and #44.

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
