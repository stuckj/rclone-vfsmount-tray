# rclone-vfsmount-tray

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

> **Status: early development.** The workspace, the rc API models and CI are in place; there
> is no working applet yet. See the [roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1).

## What it will do

- Mount and unmount rclone mounts from the tray
- Show your existing remotes, browse their paths, and configure where to mount them
- **Surface VFS write-back cache state** — which files are still pending upload, how many
  bytes are outstanding, and per-file progress

The last one is the gap. rclone's write-back queue is effectively invisible: copy 4 GB into
a mount, unmount a minute later, and you lose whatever had not uploaded, with nothing to warn
you. Existing GUIs show `core/stats` job progress, which covers explicit `copy`/`sync` but
not writes that came in through a mount.

## How it is put together

Three processes:

| | |
|---|---|
| `rclone-vfsmount-trayd` | A systemd **user** service. Owns the mounts, watches upload state, serves D-Bus. Runs headless — no graphical session needed. |
| `rclone-vfsmount-tray` | The tray icon. A client. |
| `rclone-vfsmount-tray-gtk` | The configuration and transfer windows. Also a client. |

**Mounts belong to the service, and quitting the tray does not unmount anything.** Neither
does restarting the service for a package upgrade. That is a deliberate guarantee, and it is
covered by tests — see [DESIGN.md](DESIGN.md) for the full lifetime matrix.

You can run the service with no tray at all, which is the sensible configuration on a
headless box.

## Installing

Not yet published. Once 0.1.0 ships there will be apt, dnf/yum, Homebrew and Nix channels.

### From source

```sh
git clone https://github.com/stuckj/rclone-vfsmount-tray
cd rclone-vfsmount-tray
cargo build --release
```

Requires a Rust toolchain and, for the GTK client only, the GTK4 development headers. The
service and tray link no system C libraries, so `cargo build` works without GTK installed —
the GTK crate is excluded from the workspace's default members and is built explicitly:

```sh
cargo build -p rclone-vfsmount-tray-gtk   # needs GTK4 headers
```

At runtime you need `rclone` and `fuse3`.

## Development

```sh
cargo test                    # core, service and tray — no system deps needed
cargo clippy --all-targets
cargo fmt --all
```

The tests in `crates/core/tests/fixtures.rs` parse `testdata/`, which holds real responses
captured from a live rclone v1.75.0. They exist so that rclone changing its wire format is
caught by a test naming the field that moved, rather than by a tray icon quietly ceasing to
show progress.

## Documentation

- [DESIGN.md](DESIGN.md) — architecture, the capability ladder, the security model, and the
  reasoning behind the decisions
- [Roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1)

## License

MIT — see [LICENSE](LICENSE).
