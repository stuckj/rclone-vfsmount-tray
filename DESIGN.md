# Design

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

This document records the architecture and, more importantly, the decisions behind it —
including the ones that look arbitrary until you know what goes wrong otherwise.

## What this is

A GUI for rclone mounts on Linux, in the spirit of Mountain Duck.

**rclone already solves the hard part.** Mounting arbitrary remote storage as a local
filesystem, with block-level on-demand access and a local write-back cache, is genuinely
difficult, and rclone does it for 70-odd backends. This project does not reimplement any of
that. It is the layer above: a way to set up mounts, bring them up and down, see them, and
watch data move — without a terminal.

Concretely, in rough order of importance:

1. **Manage mounts** — create, edit and delete mount configurations; mount and unmount them.
2. **See their state** — which are up, which are not, and what is wrong when one fails.
3. **See data moving** — uploads out of the write-back cache and downloads into it, with
   per-file progress where rclone reports it.
4. **Eventually, file manager integration** — mark files as local or cloud-only in the file
   manager, the way Mountain Duck does in Finder and Explorer. Not yet designed or tracked.

Mountain Duck is the reference for the *feel* — a tray/menu-bar app where connections are
things you configure and watch, with sync state visible rather than inferred. Not for the
feature list: this deliberately does not aim at its selective-sync model, its protocol
implementations, or its bookmark ecosystem.

## Why it does not already exist

Existing rclone GUIs each miss part of it. RcloneTray is Electron and dormant; RClone
Manager and Rclone UI are Tauri webviews; Rclone Browser is Qt5 and window-first;
RcloneDriveManager does mount/unmount only.

The thinnest coverage is on point 3. Existing tools surface `core/stats` job progress, which
covers explicit `copy`/`sync` but not data that moved through a mount, so the write-back
queue is effectively invisible — the common workaround is grepping rclone's log for
`vfs cache: queuing for upload`. That is a gap worth closing, but it is one feature of the
tool, not its purpose.

### What is *not* the problem

Unuploaded data is **not** lost when a mount goes away. rclone's write-back cache is on
disk: dirty items survive an unmount, an rclone crash and a reboot, and upload resumes when
the mount comes back. The cache also holds dirty content that exceeds `--vfs-cache-max-size`
— it will not evict data it has not yet uploaded.

So the cost of an unmount with a full queue is **delay and uncertainty**, not destruction:
your data has not reached the remote, nothing tells you so, and nothing tells you when it
does. That is what makes visibility worth building — and it is why the unmount check (#19)
warns rather than refuses.

> An earlier version of this document claimed data was lost in that situation. It was wrong,
> and contradicted the T4 row below, which correctly notes that a dead rclone's dirty items
> are still on disk.

## Trademark

Mountain Duck® is a registered trademark of iterate GmbH. This project is not affiliated
with, endorsed by, or derived from Mountain Duck; the name is used only to describe the kind
of tool this is.

## Process model

Four crates, three processes.

```
crates/
├── core/      rvt-core                   library. rc client, typed models, cache
│                                         scanner, config. Pure Rust, no system C libs.
├── service/   rclone-vfsmount-trayd      systemd USER service. Owns mount lifetime,
│                                         polls VFS state, serves D-Bus. Headless.
├── tray/      rclone-vfsmount-tray       ksni StatusNotifierItem client over D-Bus.
│                                         Pure Rust, deliberately low RSS.
└── gtk/       rclone-vfsmount-tray-gtk   GTK4 client over D-Bus. Only crate linking GTK.
```

The service owns everything. The tray and the windows are both *clients* of it, with no
privileged position between them — the tray is not "the app" with a settings window bolted
on; it is one of two equal front-ends.

`crates/gtk` will be the only crate that links a system C library, once it gains its `gtk4`
dependency — today it has none, so nothing in the workspace links one. The boundary is
nonetheless load-bearing and enforced from the start: it lets CI lint and test three of the
four crates on a bare runner with no `apt-get install` step, so the common path stays fast.
`crates/gtk` is excluded from the workspace `default-members` for the same reason — a bare
`cargo build` works without GTK headers installed.

### The lifetime rule

**Mounts belong to the service. Nothing a client does can unmount anything.**

Someone who quits the tray to declutter their panel, or whose tray crashes, or who logs into
a session where the tray never starts, must find every mount exactly as they left it.

The rule extends to the service itself: **restarting the service does not unmount.** A
package upgrade restarts the service, and nobody expects `apt upgrade` to unmount their
filesystems.

| Event | Mounts |
|---|---|
| Tray quits via its menu | unaffected |
| Tray crashes or is `SIGKILL`ed | unaffected |
| Tray never starts (headless, SSH) | unaffected — the service runs standalone |
| GTK client opens and closes | unaffected |
| Service restarts (package upgrade) | unaffected — reconciled and adopted on start |
| Service crashes | unaffected; adopted on restart |
| Service stopped explicitly | unaffected by default; unmounts only if `unmount_on_service_stop` is on |
| User clicks Unmount | unmounted, after the pending-uploads check |
| Session ends / logout | depends on `loginctl enable-linger`; documented in the README |
| Suspend / resume | mounts survive; stale handles recovered |

This is the kind of property that regresses quietly — a `KillMode`, a cgroup setting, or a
tidy-looking `Drop` impl that unmounts "for cleanliness". It must therefore be asserted by
integration tests rather than merely intended. Those tests do not exist yet: there is no
supervisor implementation to test against. They land with the supervisor, and the matrix
above is their specification.

## Decision: how the service supervises mounts

**One rclone process per mount, each run as a transient systemd user unit that the service
starts over systemd's D-Bus API.**

The alternatives, and why they lost:

**Child processes of the service.** Simplest, and gives unambiguous per-mount stats. But
mounts die with the service, which breaks the lifetime rule above. Keeping them alive across
a restart would mean re-parenting or a hand-off dance — considerable machinery to recover a
property that systemd gives away for free.

**A single `rclone rcd`, mounts created via `mount/mount`.** Attractive: one socket, and
`vfs/list`, `mount/listmounts`, `mount/unmount` come for free. Two problems. It is a single
failure domain — one wedged remote takes down every mount — and it still has to outlive the
service, so it wants to be a systemd unit anyway, at which point the "one process" saving is
mostly gone.

Historically there was a third objection to the shared daemon: `core/stats` is process-global,
so `transferring[]` mixes every mount together and would need attributing by path prefix.
**That objection no longer holds** — the investigation in issue #9 found that each transfer
carries a `srcFs` containing the owning VFS's cache directory, prefixed with a backend tag
(`:local{8un-i}:`), and the remainder matches that mount's `vfs/stats` `diskCache.path`.

Attribution therefore strips the tag and compares for **equality**. Once the tag is removed
the two strings are identical, so anything looser is looser than the data requires — and
looser misattributes in two directions. Substring matching lets `…/srv/photos` claim
`…/srv/photos-backup`. Prefix matching fixes that but still breaks on *nesting*: mounting
both `remote:/srv` and `remote:/srv/photos` from one process gives cache paths where one is
a genuine prefix of the other, and the parent would absorb the child's transfers and report
a byte total silently too large. Equality has neither failure mode, and if a future rclone
changes the format the fixture tests say so.

The decision therefore rests on lifetime and failure isolation alone, which is the honest
basis for it.

What we get: mounts survive service restart, crash and upgrade; systemd handles restart,
backoff and rate-limiting so we do not reimplement it; and one wedged remote cannot take the
others with it.

### Delegated restart needs a pre-start hook to work at all

rclone binds its rc socket with a bare `net.Listen` and will not replace a stale one, and Go
unlinks a UNIX socket only on a clean close. So a hard-killed rclone — the OOM killer, or
systemd's own SIGKILL after `TimeoutStopSec` — leaves both a socket and a stale mount point
behind, and every automatic restart dies on `EADDRINUSE` before it can mount anything. The
restart policy would then work only in the cases where it cannot help (a failure before the
socket is bound, such as bad credentials, which fails identically forever) and fail in the one
case it exists for.

Each mount unit therefore carries an `ExecStartPre` that re-invokes the service binary to
clear those leftovers, so the cleanup runs on systemd's restarts and not only on an explicit
mount. Two constraints shape it:

- **It talks to nothing.** systemd is blocked waiting on it, so asking systemd anything there
  would deadlock.
- **It clears a mount point only when that point is stale.** It runs without the ownership
  checks an explicit mount applies, so releasing a *live* mount there would be exactly the
  take-over this service refuses to do.

It is passed the config path the service was loaded from. A transient unit does not inherit
the caller's environment, so a hook left to re-derive its own path would silently read a
different file, or none.

The cost is a dependency on systemd for supervision. Acceptable — the service is already a
systemd user unit, and the target is Linux desktops.

`MountSupervisor` in `rvt-core` exists so this stays reversible. Everything above the trait
is written against the interface, and the trait is deliberately **dyn-compatible** — its
methods return boxed futures rather than `impl Future` — so the implementation can be chosen
at runtime, consumers need not all become generic, and tests can substitute a double. These
operations mount filesystems and fire at human frequency; one allocation per call is not
worth optimising against those properties.

## The capability ladder

How much can be said about pending uploads depends on what rclone will tell us, which varies.
Feature-detect with `rc/list`, which enumerates the commands a build actually registers —
not by comparing version numbers, which only guesses.

| Tier | Source | Gives | Notes |
|---|---|---|---|
| **T1** | `core/stats` `transferring[]` | `{name, size}` always; `{bytes, percentage, speed, speedAvg, eta, group}` once rclone attaches accounting; `srcFs`/`dstFs` when a source/destination exists | Per-file progress bars. **Confirmed available** for VFS write-back uploads (#9). **Does not meet the bar**, despite being the most detailed tier: it shows transfers that have *started*, and lags `vfs/queue` by `--vfs-write-back`, so a total taken from it reads zero while gigabytes sit queued — wrong in the unsafe direction. Mixes directions — see below. Treat every field but `name` and `size` as optional. |
| **T2** | `vfs/queue` (per-fs) | `{name, id, size, expiry, tries, delay, uploading}` | **The minimum bar.** `sum(size)` = bytes to send; `uploading` = in flight. `vfs/queue-set-expiry` forces an upload. |
| **T3** | `vfs/stats` (per-fs) | `diskCache{uploadsInProgress, uploadsQueued, erroredFiles, outOfSpace, path, pathMeta}` | Counts only. Does not meet the bar alone, but hands over the cache paths for T4. |
| **T4** | Cache directory scan | `vfsMeta/<backend>/<path>` JSON `{Size, Dirty, …}` | `sum(Size where Dirty)` = bytes to send. **Meets the bar with no rc at all**, and survives an rclone crash — a dead process's dirty items are still on disk. |

T4 is a first-class tier, not a fallback of last resort. It is the only tier that works when
the rclone process is unreachable, and the only one that survives a crash, because the rc
endpoints only know a *running* process's in-memory queue.

Its honest limits: no per-file upload progress (the cached file reflects what the application
wrote, not what was uploaded); no in-flight flag (`Dirty` stays true until upload completes,
so "queued" and "uploading" are indistinguishable); and the aggregate rate must be derived by
differencing total dirty bytes as files drop out, so large files stall then jump.

### The rule that follows

**Tier the display honestly. Never fake precision the data source cannot support.**

- T1 → per-file progress bars, real ETAs — but take the *outstanding total* from T2 or T4,
  never from `transferring[]`
- T2 → per-file sizes and an in-flight flag, aggregate rate, **no per-file percentages**
- T3 → counts only
- T4 → file list with sizes, aggregate bytes, coarse derived rate, **no in-flight flag**

A progress bar that actually means "we have no idea" is worse than a number. When the tier
degrades mid-session — rc goes away and we drop to T4 — the UI must visibly lose precision
rather than freeze on stale figures.

### Identifying a write-back upload takes two conditions, not one

`core/stats` reports every transfer in the process, and the `group` field separates rc
jobs (`job/<n>`) from everything else (`global_stats`). It is tempting to read
`global_stats` as "VFS write-back upload". **It is not.** VFS cache *downloads* —
reading a file through a `--vfs-cache-mode full` mount — are ungrouped too, and appear
in `transferring[]` with the same group.

Measured, with an empty upload queue while pulling a file down:

| | write-back upload | cache download |
|---|---|---|
| `group` | `global_stats` | `global_stats` |
| `srcFs` | the VFS **cache** directory | the **remote** |
| `dstFs` | the remote | **absent** |

So a write-back upload is `group == global_stats` **and** `srcFs` matching that mount's
`vfs/stats` `diskCache.path`. Filtering on the group alone shows a file being
*downloaded* as a pending upload, and counts its bytes toward the outstanding total that
decides whether unmounting is safe — wrong in the unsafe direction.

The same cache-path match is what attributes a transfer to a *particular* mount, so one
condition does double duty; it just must not be skipped when there is only one mount.

### Measured behaviour worth knowing

All measured in #9 against rclone v1.75.0. The **shape** claims are encoded as tests in
`rvt-core`; the **timing** ones (marked ⏱) are not, and cannot be — a static fixture cannot
express a lag or an unstable first reading. Confirming those needs the live-rclone harness
in #38.

- `transferring` is **absent, not `[]`,** when nothing is in flight. "No information" and
  "nothing in flight" are different, and the models keep them different.
- `eta` is **null** early in a transfer.
- ⏱ The **first `speed` reading is unreliable** — it averaged several times the true rate.
- `percentage` **never reached 100** in any sample; completion is the entry disappearing.
- ⏱ There is a **~0.6 s window** where `vfs/queue` still lists an item as `uploading` after
  `core/stats` has dropped it. Hold the last known value rather than snapping to zero.
- ⏱ `transferring[]` lags `vfs/queue` by `--vfs-write-back`. That window is "queued", not "0%".
- `vfs/stats` `diskCache.bytesUsed` read **0** throughout a 128 MiB upload. It is not pending
  bytes and not reliably cache size either. Do not show it.
### One limit that is not from #9

Read from rclone's source rather than measured, and not covered by a test. Recorded here because it changes what the applet may claim, and flagged
as unverified so nobody treats it as established.

Not every write through a mount enters the write-back cache:

- Under `--vfs-cache-mode off`, writes stream straight to the remote via `operations.Rcat`.
- Under `minimal`, only *write-only* opens stream. Read-write opens, and any file already in
  the cache, go through it normally and are fully visible to T2/T3/T4.

A streamed write is **visible but unattributable**: `Rcat` accounts the transfer, so it does
appear in `core/stats` `transferring[]` with `name`, `dstFs`, `group` and byte progress — but
`size` is `-1` and there is **no `srcFs`**, which is the field that ties a transfer to a
mount's cache. So it cannot be attributed to a mount, and `vfs/queue` never knows about it.

The consequence for the UI: on a mount configured `off` (or `minimal` with write-only
opens), "nothing pending" does not mean "nothing in flight". The applet should say the mount
is unmonitored rather than imply it is idle — and the T1 data is rich enough to show the
transfer, just not to attribute it, so an "unattributed transfers" line is implementable.

## Security

rclone's own documentation is explicit that rc access is equivalent to shell access as the
rclone user: `core/command` re-executes the binary, `config/dump` returns every backend
credential, and authentication is all-or-nothing with no per-endpoint scoping.

### What the rc socket is and is not a boundary against

It is worth being precise here, because it is easy to write something reassuring that is not
true.

**The rc socket is not a privilege boundary against same-user code.** Unix permissions are
per-UID. Anything already running as this user can read `rclone.conf` and execute `rclone`
directly, so reaching the rc socket confers nothing it did not already have. Tightening the
socket mode does not change that, and presenting it as if it does would be misleading.

What the socket controls is exposure to **other** users on the machine:

- **The rc endpoint is a UNIX socket, never a TCP bind.** This is the control that matters. A
  TCP listener is reachable by every local user and, misconfigured, from the network — which
  would turn a shell-equivalent API into a remote one.
- The socket lives under `$XDG_RUNTIME_DIR`, which is `0700` per the XDG specification. That
  directory mode, not the socket's own mode, is what actually excludes other users.
- Given the above, the socket's permissions are **defence in depth, not the primary control**.
  rclone performs no `chmod` or umask handling when binding (`net.Listen` only), so the socket
  gets `0777 & ~umask` — `0755` under the common umask `022`.

  **The unit's umask is not the lever to use for this.** rclone's `--umask` defaults to the
  process umask, and it masks its own `0777`/`0666` defaults with it, so a `UMask=0077` on the
  unit also makes every file and directory *inside the mount* `0600`/`0700`. A mount shared with
  another service account via `allow_other` would then fail with `EACCES` on every file. The
  directory mode is what excludes other users, and the service creates
  `$XDG_RUNTIME_DIR/rclone-vfsmount-tray` `0700` explicitly rather than relying on the socket's
  own mode. The rc client additionally refuses a socket whose mode or parent directory would let
  anyone else reach it, and checks the peer's uid after connecting.
- Because the socket is the boundary, rclone is run with `--rc-no-auth`: an rc password stored
  in a config file readable by the same user adds a step, not a boundary, and would need to be
  passed on a command line or through the environment.

### The boundary that does matter: D-Bus, and only for sandboxed callers

Applying the same standard honestly: an unsandboxed process running as this user is **not**
constrained by the D-Bus interface either. It can run `fusermount -u` on the mount point, kill
the rclone unit, or read `rclone.conf` directly. Against that caller nothing here is a
boundary, and pretending otherwise would be the same mistake as overselling the socket mode.

The genuinely distinctive case is a **sandboxed application granted session-bus access** — a
Flatpak with `--socket=session-bus`, say. It can reach this interface but cannot run
`fusermount`, cannot see `rclone.conf`, and cannot signal the rclone process. For that caller
the D-Bus surface is the whole attack surface, and it is the only reason this interface is a
boundary at all. Everything below is scoped to that:

- The interface is a **curated set of methods**, never a generic rc passthrough. Proxying a
  shell-equivalent API wholesale would hand a sandboxed caller exactly what the sandbox exists
  to withhold.
- `core/command` and `config/dump` are never called, and never reachable from a client.
- **Credentials are never read at all.** Mounting and reporting progress need remote *names*
  and paths, not secrets. Note `config/get` is not a per-field getter — it returns a whole
  remote's configuration, credentials included — so "read only the safe fields" is not on
  offer, and is not needed. Nothing published over D-Bus carries credentials or full remote
  configuration.
- **A forced unmount is destructive**, and `force` is an explicit parameter that defaults to
  off. Being service-side, the pending-uploads check cannot be skipped by a client that simply
  omits it — but it does not stop a caller that deliberately passes `force = true`. That is a
  guard against accident and bugs, not against malice.

  Closing the malice case needs an authorization decision the bus cannot make for us. The
  options are a polkit action for forced unmount, or accepting the risk on the grounds that a
  sandboxed app able to destroy unuploaded data is a narrow threat. **This is deliberately
  left open** and must be settled when the D-Bus interface is implemented rather than
  discovered afterwards; recording it as unresolved is more useful than claiming a mitigation
  that does not hold.

One further surface to keep in mind: the on-disk cache scanner walks a path taken from an rc
response, so it must treat `diskCache.path` as untrusted input and refuse to follow it outside
the expected cache root.

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

`TransferState` carries the tier that produced it, so a client can render exactly what is
supported and no more.

## Non-goals

- Not a file manager. Opening a mount hands off to `xdg-open`.
- Not a general rclone job runner. Explicit `copy`/`sync` jobs are somebody else's tool —
  and are actively filtered *out* of the display.
- No embedded browser or webview.
- Creating and editing rclone **remotes** is deliberately out of scope until 0.3.0. Listing
  and browsing existing remotes to choose a mount source is in from the start.
