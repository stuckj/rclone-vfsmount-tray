# rclone-vfsmount-tray

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

> **Status: early development — not usable yet.** There is no tray icon and no window. What
> works today is the service underneath: it finds rclone, reads its configuration, starts,
> stops and adopts mounts as systemd user units, and works out what each one still has to
> upload. It then logs what it found and exits — it does not stay running, and nothing
> connects to it yet. Follow the
> [roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1) for progress.

## What it does

rclone can mount cloud storage — Google Drive, S3, Dropbox, around 70 services — as a folder
on your machine, downloading files as you open them and uploading changes in the background.
It does that well, but it is a command-line tool.

This is the missing front end:

- **Manage your mounts** — add, edit and remove them; connect and disconnect with a click
- **See their state** — what is connected, what is not, and what went wrong
- **See your data moving** — what is still uploading, how far along it is, and when it is
  safe to close the laptop
- **Later** — marks in your file manager showing which files are local and which are still
  in the cloud

That third point is the one that is missing everywhere else. When you save a file to an
rclone mount, it lands in a local cache first and uploads afterwards. Nothing is lost if you
disconnect before that finishes — the cache is on disk, and rclone picks up where it left off
next time. But no existing tool tells you whether your work has actually reached the cloud,
or how long it will take. This one is built around answering that, and around never showing
you a number it cannot stand behind: if it does not know, it says so instead of showing zero.

## How it is put together

A background **service** owns the mounts and does the work. The tray icon and the settings
window are both just clients that talk to it.

That split buys you the main guarantee: **quitting the tray never disconnects anything.**
Neither does restarting the service, or the service crashing. Each mount runs on its own, and
keeps running until you say otherwise. The service also runs perfectly well with no tray at
all, which is what you want on a machine you only ever reach over SSH.

## Installing

Not yet published. Once 0.1.0 ships there will be apt, dnf/yum, Homebrew and Nix packages.

For now, build it from source. You will need a [Rust toolchain](https://rustup.rs), plus
`rclone` and `fuse3` installed to actually mount anything.

```sh
git clone https://github.com/stuckj/rclone-vfsmount-tray
cd rclone-vfsmount-tray
cargo build --release
```

The binaries land in `target/release/`. Nothing else needs installing — the build pulls in no
system libraries.

## Configuring it

Mounts are described in a TOML file at:

```
~/.config/rclone-vfsmount-tray/config.toml
```

Copy [`config.example.toml`](config.example.toml) there and edit it — it documents every
option inline. A minimal mount needs three things:

```toml
version = 1

[[mount]]
name = "photos"                          # a short identifier
remote = "backup"                        # an rclone remote, without the ":"
mount_point = "/home/you/mnt/photos"     # where it appears on your machine
```

`rclone listremotes` shows the remotes you already have; `rclone config` is what creates new
ones. **This file never contains passwords or keys** — those stay in rclone's own
configuration, and this project never reads them.

The graphical editor is not built yet, so this file is the way to set things up today. The
service reads it at start-up and does not watch it, so restart the service after editing.

## Running it

```sh
./target/release/rclone-vfsmount-trayd --foreground
```

Today that reconciles your mounts against the config, reports what each one has left to
upload, and exits. Add `--log-level debug` for detail, or `--config <path>` to point it at a
different file. Running it as a proper background service, with the tray attached, is what
0.1.0 is for.

## Documentation

- [DESIGN.md](DESIGN.md) — how the pieces fit together and why
- [CONTRIBUTING.md](CONTRIBUTING.md) — building, testing and submitting changes
- [Roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1)

## License

MIT — see [LICENSE](LICENSE).

## Trademark

Mountain Duck® is a registered trademark of iterate GmbH. This project is not affiliated
with, endorsed by, or derived from Mountain Duck; the name is used only to describe the kind
of tool this is.
</content>
</invoke>
