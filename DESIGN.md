# Design

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

This document is directional: what the pieces are, where the boundaries between them sit, who
owns what across those boundaries, and the decisions that constrain what gets built next. It
deliberately does not describe mechanism — the code is the authority on how any of this
actually works — and it does not catalogue measurements, which belong in tests that fail when
they stop being true and in the issues that took them. Where a measured fact appears here it
is because a decision rests on it, and it is stated once.

## What this is

A GUI for rclone mounts on Linux.

**rclone already solves the hard part.** Mounting arbitrary remote storage as a local
filesystem, with block-level on-demand access and a local write-back cache, is genuinely
difficult, and rclone does it for 70-odd backends. This project does not reimplement any of
that. It is the layer above: a way to set up mounts, bring them up and down, see them, and
watch data move — without a terminal.

In rough order of importance:

1. **Manage mounts** — create, edit and delete mount configurations; mount and unmount them.
2. **See their state** — which are up, which are not, and what is wrong when one fails.
3. **See data moving** — uploads out of the write-back cache and downloads into it, with
   per-file progress where rclone reports it.
4. **Eventually, file manager integration** — mark files as local or cloud-only in the file
   manager, the way Mountain Duck does in Finder and Explorer. Not yet designed or tracked.

Mountain Duck is the reference for the *feel* — a tray app where connections are things you
configure and watch, with sync state visible rather than inferred. Not for the feature list:
this deliberately does not aim at its selective-sync model, its protocol implementations, or
its bookmark ecosystem.

### Why it does not already exist

Existing rclone GUIs each miss part of it. RcloneTray is Electron and dormant; RClone Manager
and Rclone UI are Tauri webviews; Rclone Browser is Qt5 and window-first; RcloneDriveManager
does mount/unmount only.

The thinnest coverage is on point 3. Existing tools surface `core/stats` job progress, which
covers explicit `copy`/`sync` but not data that moved through a mount, so the write-back queue
is effectively invisible. That is a gap worth closing, but it is one feature of the tool, not
its purpose.

### What is *not* the problem

Unuploaded data from a file that has been **closed** is not lost when a mount goes away.
rclone's write-back cache is on disk: dirty items survive an unmount, an rclone crash and a
reboot, and upload resumes when the mount comes back. A file still *open* is the exception,
and the one case that is genuinely destructive — see the lifetime rule below.

So the cost of an unmount with a full queue is **delay and uncertainty**, not destruction:
your data has not reached the remote, nothing tells you so, and nothing tells you when it
does. That is what makes visibility worth building — and it is why the pending-uploads check
(#19) warns rather than refuses.

## Process model

Five crates, three processes. Four ship; the fifth exists only for tests.

```
crates/
├── core/      rvt-core                   library. rc client, typed models, cache
│                                         scanner, config. Pure Rust, no system C libs.
├── service/   rclone-vfsmount-trayd      systemd USER service. Owns mount lifetime,
│                                         polls VFS state, serves D-Bus. Headless.
├── tray/      rclone-vfsmount-tray       ksni StatusNotifierItem client over D-Bus.
│                                         Pure Rust, deliberately low RSS.
├── gtk/       rclone-vfsmount-tray-gtk   GTK4 client over D-Bus. Only crate linking GTK.
└── testutil/  rvt-testutil               scratch directories for tests. A dev-dependency
                                          of the others, never linked into a binary.
```

The service owns everything. The tray and the windows are both *clients* of it, with no
privileged position between them — the tray is not "the app" with a settings window bolted on;
it is one of two equal front-ends. Clients never write the configuration file either: mutation
goes through the service (#16), so one authority validates a change and one authority
announces it.

**Only `crates/gtk` may link a system C library.** That boundary is load-bearing and enforced
from the start: the other four lint and test on a bare runner with no `apt-get` step, which is
what keeps the common path fast. `crates/gtk` is excluded from the workspace `default-members`
for the same reason.

### The lifetime rule

**Mounts belong to the service. Nothing a client does can unmount anything.**

Someone who quits the tray to declutter their panel, or whose tray crashes, or who logs into a
session where the tray never starts, must find every mount exactly as they left it.

The rule extends to the service itself: **restarting the service does not unmount.** It
crashes — this is a young program that polls rclone, walks cache directories and talks D-Bus —
and tying whether a filesystem is reachable to whether it has a bug is far too wide a blast
radius for what it is. It is also restarted by hand, and by whatever is managing it.

Nor is an unmount free to take back. rclone exits on `SIGTERM` without flushing its write-back
queue (#73), so an unmount at a moment the user did not choose can sever a write in flight.
The cache is on disk and resumes, but the file that was mid-write does not un-truncate.

| Event | Mounts |
|---|---|
| Tray quits via its menu | unaffected |
| Tray crashes or is `SIGKILL`ed | unaffected |
| Tray never starts (headless, SSH) | unaffected — the service runs standalone |
| GTK client opens and closes | unaffected |
| Service restarts | unaffected — reconciled and adopted on start |
| Service crashes | unaffected; adopted on restart |
| rclone is upgraded | unaffected — each mount keeps serving from the binary it started with, and picks up the new one only when it is next mounted |
| Service stopped explicitly | unaffected by default; unmounts only if `unmount_on_service_stop` is on |
| User clicks Unmount | unmounted — refused while anything is still using the mount, unless forced |
| Session ends / logout | depends on `loginctl enable-linger` |
| Suspend / resume | mounts survive; stale handles recovered |

This is the kind of property that regresses quietly — a `KillMode`, a cgroup setting, or a
tidy-looking `Drop` impl that unmounts "for cleanliness". The matrix is therefore a
specification for tests rather than a description of them; asserting it against a real rclone
is #38.

**Whether a system update restarts this service is not established**, and the answer differs
by install path: Home Manager's `sd-switch` does restart changed user units, while
`nixos-rebuild switch` is long documented as not reliably doing so
([nixpkgs#29146](https://github.com/NixOS/nixpkgs/issues/29146), #34). The `.deb`/`.rpm` case
is unverified (#30). None of the reasoning above depends on the answer. It is written down as
an open question rather than deleted so the next person does not re-derive it.

## How the service supervises mounts

**One rclone process per mount, each run as a transient systemd user unit that the service
starts over systemd's D-Bus API.**

The alternatives, and why they lost:

**Child processes of the service.** Simplest, and gives unambiguous per-mount stats. But
mounts die with the service, which breaks the lifetime rule. Keeping them alive across a
restart would mean re-parenting or a hand-off dance — considerable machinery to recover a
property systemd gives away for free.

**A single `rclone rcd`, mounts created via `mount/mount`.** Attractive: one socket, and
`vfs/list`, `mount/listmounts`, `mount/unmount` come for free. But it is a single failure
domain — one wedged remote takes down every mount — and it still has to outlive the service,
so it wants to be a systemd unit anyway, at which point the "one process" saving is mostly
gone.

The decision therefore rests on lifetime and failure isolation. What it buys: mounts survive
service restart, crash and upgrade; systemd handles restart, backoff and rate-limiting, so we
do not reimplement it; and one wedged remote cannot take the others with it.

`MountSupervisor` in `rvt-core` exists so this stays reversible. Everything above the trait is
written against the interface, and the trait is deliberately **dyn-compatible**, so the
implementation can be chosen at runtime, consumers need not all become generic, and tests can
substitute a double.

### Delegated restart needs a pre-start hook to work at all

A hard-killed rclone — the OOM killer, or systemd's own SIGKILL after `TimeoutStopSec` —
leaves both its rc socket and its mount point behind, and rclone will not reuse either. Every
automatic restart then dies before it can mount anything, so the restart policy would work
only in the cases where it cannot help and fail in the one case it exists for.

Each mount unit therefore carries an `ExecStartPre` that re-invokes the service binary to
clear those leftovers, so the cleanup runs on systemd's restarts and not only on an explicit
mount. Two constraints shape it:

- **It talks to nothing.** systemd is blocked waiting on it, so asking systemd anything there
  would deadlock.
- **It clears a mount point only when that point is stale.** It runs without the ownership
  checks an explicit mount applies, so releasing a *live* mount there would be exactly the
  take-over this service refuses to do.

It is passed the config path the service was loaded from, because a transient unit does not
inherit the caller's environment and a hook left to re-derive its own path would silently read
a different file, or none.

The cost is a dependency on systemd for supervision. Acceptable — the service is already a
systemd user unit, and the target is Linux desktops.

### Ownership: ours, orphaned, or foreign

Every unit this service starts is named `rvt-mount-<mount>.service`, and ownership is decided
from that name. A mount point the kernel lists with no unit of ours behind it belongs to
**somebody else**: it is never started, restarted or detached, only adopted for display, and
released outright only if the user forces it (#18).

Deriving that name from the current config is not enough on its own, because the config
changes under running mounts. Rename a mount and its unit is still up and still serving the
same path, while the name now asked about does not exist — so the mount reads as foreign, and
every gate that keys on ownership then works against the user (#71).

The prefix is therefore **swept** as well as constructed. A unit matching it that no config
entry names, and that is still serving, is **orphaned**: ours, distinct from foreign, and
stoppable exactly as any mount of ours is. Where an orphan mounts comes from its own unit —
the only place that mapping survives a config edit — and a unit whose recorded arguments
cannot be read is left alone rather than guessed at. What is *actually* mounted there is not
required to match the `remote:path` the unit was given, because rclone records the Fs it
resolved rather than the argument it was handed.

## The capability ladder

How much can be said about pending uploads depends on what rclone will tell us, which varies
across the supported range. **Feature-detect with `rc/list`**, which enumerates the commands a
build actually registers — never by comparing version numbers, which only guesses.

The same applies to composing flags. rclone has changed what a flag *means* inside the
supported range — `--umask` did at 1.68.0 (#69, pinned to that version in #92) — so send the
spelling every supported version reads alike, rather than branching on a version discovered
at start-up that need not be the binary which ends up running. Refuse a value that cannot
work on *every* supported version, and as little beyond that as possible: a config
`Config::validate` rejects stops the service, not the mount carrying it.

| Tier | Source | Gives | Verdict |
|---|---|---|---|
| **T1** | `core/stats` `transferring[]` | per-file names, sizes and progress | Richest, but **does not meet the bar**: it shows transfers that have *started*, so a total taken from it reads zero while gigabytes sit queued (#9) — wrong in the unsafe direction. |
| **T2** | `vfs/queue` (per-fs) | queued items with sizes, and an in-flight flag | **The minimum bar.** |
| **T3** | `vfs/stats` (per-fs) | counts, and the cache paths | Counts only. Does not meet the bar alone, but hands T4 its roots. |
| **T4** | Cache directory scan | dirty items and their on-disk sizes | Meets the bar, and is the only tier that survives an unreachable or crashed rclone. Takes its cache roots from T3 today; finding them without rc is the rest of #22. |

T4 is a first-class tier, not a fallback of last resort. Its honest limits: no per-file upload
progress, no in-flight flag, and an aggregate rate that has to be derived by differencing, so
large files stall then jump. It is also a *walk*, which costs real time on a large cache, so
it runs only when rclone is unreachable and at the idle cadence even then. Making it cheap
enough to run continuously is what inotify is for (#22).

### The rule that follows

**Tier the display honestly. Never fake precision the data source cannot support.**

A progress bar that actually means "we have no idea" is worse than no number at all.
`TransferState` therefore carries the tier that produced it, so a client renders exactly what
is supported and no more; when the tier degrades mid-session the UI must visibly lose
precision rather than freeze on stale figures, and a figure this project cannot stand behind
is reported as unknown rather than as zero.

Two consequences that are easy to get backwards:

- **A transfer must be attributed to a mount before it counts**, and that takes two
  conditions rather than one: it is not an explicit `copy`/`sync` job, *and* its source is
  that mount's own cache path. `core/stats` is process-global and reports cache *downloads* (#9)
  alongside write-back uploads, so the first condition alone shows a file being downloaded as
  a pending upload and counts its bytes toward the total that decides whether unmounting is
  safe — wrong in the unsafe direction.
- **An empty queue is not an idle mount.** rclone enqueues a file when it is *closed* (#73), so
  every rc endpoint reports nothing outstanding for the whole duration of a large copy — this
  is the normal state, not a narrow race. Only the on-disk dirty flag sees it. For this one
  question T4 is strictly better than T2: the single place the ladder's ordering does not
  hold.

Some cache modes make that blindness permanent rather than transient. Under `off` writes
stream straight to the remote, and under `minimal` some do; a streamed write is visible in
`core/stats` but carries nothing tying it to a mount, so no queue ever knows about it. On such
a mount the applet says **unmonitored** or **partially observed** rather than implying it is
idle. Code deciding which mode is in force must fail **closed**: a mode it cannot parse is
"writes may be streaming", never "all writes are queued".

## The unmount order

Asking the kernel to release the mount point is the only signal there is that something is
still using it, so `unmount()` asks it **first**: ownership and source checks, then the
release, then stop the unit (#73). Refusing after the SIGTERM has gone out refuses nothing —
rclone will already have exited and severed the writer.

- Release succeeds — nothing was holding it, and the stop is bookkeeping.
- Release fails, point gone anyway — a crash or a concurrent stop won the race.
- Release fails, point still there — **refuse**, with nothing signalled.

`fusermount` reports every non-zero exit alike, so a refusal cannot tell "busy" from "not a
mount point". It therefore fails closed and says what was refused rather than asserting why.

**`force` (#18) is refuse → stop → refuse → detach**, not "stop, then unmount". Killing rclone
does not free the mount, because the *holder's* descriptor is what pins it — so a sequence
that stopped the unit and then unmounted would end with the file sacrificed **and** the
unmount failed. Detaching is reachable only on that last step, where the writer is already
severed; release and detach are therefore separate operations rather than one flag, because
they are not interchangeable.

Detaching is gated on ownership too: a held **foreign** mount refuses even under force, since
its rclone was never signalled and detaching would strand it serving a mount nothing can see.
An orphan is not foreign for this purpose — its unit is stopped like any other of ours, so the
escalation stays reachable.

None of this makes `safe_to_unmount()` whole: it still cannot see an open write, so a mount it
calls idle can be refused. #22 is what would close that gap.

## Security

rclone's rc API is equivalent to shell access as the rclone user: `core/command` re-executes
the binary, `config/dump` returns every backend credential, and authentication is
all-or-nothing with no per-endpoint scoping. Two boundaries follow, and it is worth being
precise about what each one is and is not, because it is easy to write something reassuring
that is not true.

### The rc socket

**It is not a privilege boundary against same-user code.** Unix permissions are per-UID.
Anything already running as this user can read `rclone.conf` and execute `rclone` directly, so
reaching the rc socket confers nothing it did not already have. What the socket controls is
exposure to **other** users on the machine:

- **The rc endpoint is a UNIX socket, never a TCP bind.** This is the control that matters. A
  TCP listener is reachable by every local user and, misconfigured, from the network — which
  would turn a shell-equivalent API into a remote one.
- The socket lives in a `0700` directory the service creates explicitly. That directory mode,
  not the socket's own mode, is what excludes other users, since rclone applies no mode of its
  own when binding. The rc client additionally refuses a socket whose mode or parent directory
  would let anyone else reach it, and checks the peer's uid after connecting.
- **The unit's `UMask` is not the lever for this.** rclone masks its own file and directory
  defaults with the same value, so tightening it also changes the modes of everything *inside*
  the mount, which breaks any mount deliberately shared via `allow_other`.
- rclone runs with `--rc-no-auth`: an rc password stored in a file readable by the same user
  adds a step, not a boundary.

### D-Bus, and only for sandboxed callers

Applying the same standard honestly: an unsandboxed process running as this user is **not**
constrained by the D-Bus interface either. It can run `fusermount -u` on the mount point, kill
the rclone unit, or read `rclone.conf` directly. Against that caller nothing here is a
boundary, and pretending otherwise would be the same mistake as overselling the socket mode.

The genuinely distinctive case is a **sandboxed application granted session-bus access** — a
Flatpak with `--socket=session-bus`, say. It can reach this interface but can do none of those
things. For that caller the D-Bus surface is the whole attack surface, and it is the only
reason this interface is a boundary at all. Everything here is scoped to that:

- The interface is a **curated set of methods, never a generic rc passthrough.** Proxying a
  shell-equivalent API wholesale would hand a sandboxed caller exactly what the sandbox exists
  to withhold. `core/command` and `config/dump` are never called and never reachable.
- **rclone's own configuration is never read.** Mounting and reporting progress need remote
  *names* and paths, not secrets. There is no safe subset to read even if one were wanted:
  `config/get` is not a per-field getter, and returns a whole remote's configuration with its
  credentials in it.
- **This project's own config is not automatically safe to publish.** `extra_args` reaches
  rclone as verbatim argv, so a user can put a credential flag in a mount, and mount
  configuration crosses this interface in both directions once clients can edit it (#16,
  #42). The surface has to treat that field as secret-bearing — redacted or gated — rather
  than assume nothing here can be a secret.
- **Safety checks live service-side**, so a client cannot skip one by leaving a parameter out.
  A forced unmount is destructive, and `force` is explicit and defaults to off — a guard
  against accident and bugs, not against malice.
- Closing the malice case needs an authorization decision the bus cannot make for us: a polkit
  action for forced unmount, or accepting the risk on the grounds that a sandboxed app able to
  destroy unuploaded data is a narrow threat. **This is deliberately left open** and must be
  settled when the interface is implemented rather than discovered afterwards.

One further surface to keep in mind: the on-disk cache scanner walks a path taken from an rc
response, so it must treat that path as untrusted input and refuse to follow it outside the
expected cache root.

## Data flow

```
rclone (per mount, systemd user unit)
   │  rc over UNIX socket (in a 0700 directory)
   ▼
rclone-vfsmount-trayd ──── poller ──► TransferState (carries its own fidelity tier)
   │                                        │
   │  D-Bus (session bus, curated methods)  │
   ├────────────────────────────────────────┴──► rclone-vfsmount-tray   (SNI)
   └───────────────────────────────────────────► rclone-vfsmount-tray-gtk (windows)
```

## Non-goals

- Not a file manager. Opening a mount hands off to `xdg-open`.
- Not a general rclone job runner. Explicit `copy`/`sync` jobs are somebody else's tool — and
  are actively filtered *out* of the display.
- No embedded browser or webview.
- Creating and editing rclone **remotes** is deliberately out of scope until 0.3.0. Listing
  and browsing existing remotes to choose a mount source is in from the start.
