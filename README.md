# rclone-vfsmount-tray

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

> **Status: early development — not usable yet.** The service finds rclone, reads its own
> configuration, starts, stops and adopts mounts as systemd user units — including its own
> units that the config no longer names, which is what renaming a mount leaves behind — and
> works out what each one still has to upload — over rclone's rc API where it answers, and by reading the
> write-back cache off disk where it does not. What it does not do is *serve*: it
> reconciles, polls each mount once, logs what it found, and exits. There is no D-Bus
> surface, and the tray and GTK clients are scaffolding.
> See the [roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1).

## What it will do

A GUI for rclone mounts, in the spirit of Mountain Duck. rclone already does the hard part —
mounting remote storage as a local filesystem with on-demand block access and a write-back
cache, across ~70 backends. This wraps that:

- **Manage mounts** — create, edit and delete mount configs; mount and unmount them
- **See their state** — what is up, what is not, and what went wrong
- **See data moving** — uploads out of the write-back cache and downloads into it, with
  per-file progress where rclone reports it
- **Later** — file manager integration to mark files local or cloud-only

The third point is the thinnest-covered today: existing rclone GUIs show `core/stats` job
progress, which covers explicit `copy`/`sync` but not data that moved through a mount, so
the write-back queue is effectively invisible. Nothing is *lost* when you unmount with a
full queue — rclone's cache is on disk and resumes — but you have no way to know whether
your data has reached the remote, or when it will.

That queue is now modelled, at whichever fidelity the rclone in front of it supports, and
every figure carries whether it can be trusted: a number this project cannot stand behind
is reported as unknown rather than as zero. A file still being *written* is the exception
worth knowing about: rclone only queues a file when it is closed, so no rc endpoint sees
an open write and the queue reads empty throughout it. Unmounting therefore asks the
kernel to release the mount point *before* it signals rclone, and refuses while any process
is still using the mount. Signalling first severed the writer and published the truncated
cache item as though it were complete — measured, in
[#73](https://github.com/stuckj/rclone-vfsmount-tray/issues/73).

## How it is put together

Three processes:

| | |
|---|---|
| `rclone-vfsmount-trayd` | A systemd **user** service. Owns the mounts, watches upload state, serves D-Bus. Runs headless — no graphical session needed. |
| `rclone-vfsmount-tray` | The tray icon. A client. |
| `rclone-vfsmount-tray-gtk` | The configuration and transfer windows. Also a client. |

**Mounts belong to the service, so quitting the tray will not unmount anything** — neither
will restarting the service for a package upgrade. That is the central design guarantee.
Each mount runs as its own transient systemd user unit, which is what makes it outlive the
process that started it; see [DESIGN.md](DESIGN.md) for the full lifetime matrix. The
supervisor implements and unit-tests it today; asserting the whole matrix against a real
rclone is [#38](https://github.com/stuckj/rclone-vfsmount-tray/issues/38).

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

Requires a Rust toolchain. Note that `rust-toolchain.toml` pins `stable`, so rustup will
use current stable regardless of what you have installed; the declared MSRV of **1.87** is
enforced by a dedicated CI job rather than by local builds. Every crate except the GTK
client links no system C libraries, so a bare `cargo build` needs nothing else. The GTK
client is excluded from the workspace's default members and built explicitly:

```sh
cargo build -p rclone-vfsmount-tray-gtk
```

It will need the GTK4 development headers once it gains its `gtk4` dependency; today it has
none and builds anywhere.

At runtime you will need `rclone` and `fuse3`.

## Development

```sh
cargo test --locked           # everything but the GTK client — no system deps needed
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

## Trademark

Mountain Duck® is a registered trademark of iterate GmbH. This project is not affiliated
with, endorsed by, or derived from Mountain Duck; the name is used only to describe the kind
of tool this is.
