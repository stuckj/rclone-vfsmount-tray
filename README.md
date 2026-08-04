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

**Mounts will belong to the service, so quitting the tray will not unmount anything** —
neither will restarting the service for a package upgrade. That is the central design
guarantee; see [DESIGN.md](DESIGN.md) for the full lifetime matrix, which is the
specification the supervisor and its integration tests are being built against. None of it
is implemented yet.

The service is designed to run with no tray at all, which is the sensible configuration on a
headless box.

## Installing

Not yet published. Once 0.1.0 ships there will be apt, dnf/yum, Homebrew and Nix channels.

### From source

```sh
git clone https://github.com/stuckj/rclone-vfsmount-tray
cd rclone-vfsmount-tray
cargo build --release
```

Requires a Rust toolchain (1.87 or newer). The core, service and tray crates link no system
C libraries, so a bare `cargo build` needs nothing else. The GTK client is excluded from the
workspace's default members and built explicitly:

```sh
cargo build -p rclone-vfsmount-tray-gtk
```

It will need the GTK4 development headers once it gains its `gtk4` dependency; today it has
none and builds anywhere.

At runtime you will need `rclone` and `fuse3`.

## Development

```sh
cargo test --locked           # core, service and tray — no system deps needed
cargo clippy --all-targets
cargo fmt --all
```

The tests in `crates/core/tests/fixtures.rs` parse `testdata/`, which holds real responses
captured from a live rclone v1.75.0. Each fixture is checked for the keys the model
*silently dropped* — not just that it parsed — so a renamed or removed field fails a test
that names it, rather than defaulting quietly and letting the tray stop showing progress.
That check is the point; a plain parse-and-round-trip would pass either way.

## Documentation

- [DESIGN.md](DESIGN.md) — architecture, the capability ladder, the security model, and the
  reasoning behind the decisions
- [Roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1)

## License

MIT — see [LICENSE](LICENSE).
