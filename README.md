# rclone-vfsmount-tray

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

> **Status: early development.** The service works: it finds rclone, reads your
> configuration, stays running, keeps track of every mount it can see — and of what each one
> it started still has to upload — and mounts or unmounts on request. There is now a tray
> icon for it, and a command-line client for terminals, scripts and over SSH. Two things are
> still missing before it is comfortable on a desktop: the settings window, so mounts are
> described by hand-editing a file; and bringing your mounts up by itself when it starts. The
> tray draws its icons from your desktop's theme until it has a set of its own. Follow the
> [roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1) for progress.

## What it will do

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
rclone mount, it lands in a local cache first and uploads afterwards. A queue you disconnect
on is not lost — the cache is on disk, and rclone picks up where it left off next time. But
no existing tool tells you whether your work has actually reached the cloud, or how long it
will take. This one is built around answering that, and around never showing you a number it
cannot stand behind: if it does not know, it says so instead of showing zero.

The case that *can* lose data is a file you are still writing when the mount goes away.
rclone only queues a file once it is closed, so it cannot report a copy in progress, and a
disconnect mid-write leaves the partial file to be uploaded later as though it were the whole
thing. Disconnecting through this applet is therefore refused while anything is still using
the mount, rather than cutting the writer off — unless you explicitly force it, which warns
first and then does exactly that.

## How it is put together

A background **service** owns the mounts and does the work. The tray icon and the settings
window are both just clients that talk to it.

That split buys you the main guarantee: **quitting the tray never disconnects anything.**
Neither does restarting the service, or the service crashing. Each mount runs on its own and
keeps running until you say otherwise — or until you log out.

That last one catches people out, on a desktop as much as over SSH. Linux shuts your user's
background services down when your last session ends, and that takes the mounts with them.
Run `loginctl enable-linger` once and they survive logouts and come back at boot; if that is
refused, `sudo loginctl enable-linger $USER`.

The service is built to run with no tray at all, which is what you want on a machine you only
ever reach over SSH.

## Installing

Not yet published. Once 0.1.0 ships there will be apt, dnf/yum, Homebrew and Nix packages.

For now, build it from source. You will need a [Rust toolchain](https://rustup.rs) and a C
linker to build it — `build-essential` on Debian and Ubuntu, `gcc` most other places — plus
`rclone` and `fuse3` to mount anything. Mounts are run as systemd user units, so this needs a
systemd-based distribution, which is most of them.

```sh
git clone https://github.com/stuckj/rclone-vfsmount-tray
cd rclone-vfsmount-tray
cargo build --release
```

The binaries land in `target/release/`. There are no development packages to hunt down beyond
the linker above — the build pulls in no system libraries at all.

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
ones. **Nothing here needs a password or a key** — remotes are referred to by name, the
secrets stay in rclone's own configuration, and this project never reads them. The one way a
credential can end up in this file is if you put one there yourself, by passing a flag like
`--s3-secret-access-key` through `extra_args`; check for that before sharing the file.

The graphical editor is not built yet, so this file is the way to set things up today. The
service reads it once at start-up and does not watch it, so any edit takes effect the next
time you start it.

## Running it

```sh
./target/release/rclone-vfsmount-trayd
```

That finds rclone, reads your config and stays running. It watches every mount it can see —
including any rclone mount already running that this project did not start — and will mount
and unmount on request. It does **not** bring anything up on its own yet, so a fresh setup
starts with all of your mounts unmounted.

The `rclone-vfsmount-tray` binary is both the tray icon and a command-line client. With a
subcommand it is the client — the way to drive the service from a terminal, a script, or over
SSH:

```sh
rclone-vfsmount-tray list                # every mount and its state
rclone-vfsmount-tray mount photos        # returns once it is actually serving
rclone-vfsmount-tray unmount photos      # refused while the mount is in use, unless --force
rclone-vfsmount-tray status --json       # machine-readable state and outstanding uploads
```

`mount` does not come back until the mount is serving, which for a cold remote can take most
of a minute — the client waits rather than reporting a failure for one still on its way up. If
the service is not running the client says so and how to start it; it never starts the service
itself, and never reports "no mounts" when it simply could not reach it. See
[docs/CLI.md](docs/CLI.md) for every subcommand, the exit codes, and the `status --json`
schema.

The service takes `--config <path>` to point it at a different file, and `--log-level debug`
for more detail. Stopping it leaves every mount exactly as it is.

## The tray icon

Run the same binary with no subcommand and it puts an icon in your panel:

```sh
rclone-vfsmount-tray
```

It needs a panel that speaks **StatusNotifierItem**: KDE Plasma does natively, and GNOME needs
the AppIndicator extension. The old XEmbed system tray is not supported — it does not exist on
Wayland. If nothing is listening the tray waits for a panel to appear rather than exiting, so
starting it before your desktop has finished coming up is fine.

The icon says what is happening, and its tooltip spells it out:

| It says | Meaning |
| --- | --- |
| Up to date | Everything that is mounted has finished uploading |
| Uploading | Files are still on their way to the remote |
| Needs attention | A mount failed, an upload errored, or the cache is full. The only state that asks the panel to emphasise the icon — uploads in progress do not |
| State partly unknown | A mount this service manages is up, and what it has left to upload cannot be read: rclone is unreachable, or the mount has no write-back cache to look in. Mounts you started yourself are never counted here — the tray simply says nothing about those |
| Nothing mounted | No mount is serving |
| Service unreachable | The tray cannot reach the service. **This says nothing about your mounts** — any that were up are still up |

The menu lists every mount with its state. Each opens onto where it is mounted, an **Open**
item that hands the mount point to your file manager, and **Mount** or **Unmount**. Mounts this
service did not start are listed apart, and offer only **Open**: the service will not mount or
unmount something it did not start. What is still to upload is summarised at the top — "3 files, 1.2 GiB pending", a rate,
and an estimate where one can be derived honestly — and again per mount, listing up to ten
files. When a figure comes from reading the cache directory rather than from rclone, the menu
says so instead of passing it off as live progress.

**Quit closes the icon and nothing else.** Your mounts keep serving and so does the service.
Stopping the service is a separate item behind a second click, which says what will happen to
your mounts first.

The tray starts whether or not the service is running, and never starts it for you. If the
service is not there the menu says so and offers to start it; if the service is restarted
underneath it, the tray notices and reconnects on its own.

## Documentation

- [docs/CLI.md](docs/CLI.md) — the command-line client: subcommands, exit codes, `status --json`
- [DESIGN.md](DESIGN.md) — how the pieces fit together and why
- [CONTRIBUTING.md](CONTRIBUTING.md) — building, testing and submitting changes
- [Roadmap](https://github.com/stuckj/rclone-vfsmount-tray/issues/1)

## License

MIT — see [LICENSE](LICENSE).

## Trademark

Mountain Duck® is a registered trademark of iterate GmbH. This project is not affiliated
with, endorsed by, or derived from Mountain Duck; the name is used only to describe the kind
of tool this is.
