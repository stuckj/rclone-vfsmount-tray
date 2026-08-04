# Design

A native Linux system-tray applet for **rclone VFS mounts**, in the spirit of Mountain Duck.

This document records the architecture and, more importantly, the decisions behind it —
including the ones that look arbitrary until you know what goes wrong otherwise.

## The problem

rclone can mount a remote and cache writes locally, uploading them in the background. That
background upload queue is effectively invisible. If you copy 4 GB into a mount and unmount
it a minute later, you lose whatever had not been uploaded, and nothing tells you.

Existing GUIs do not fill this in. RcloneTray is Electron and dormant; RClone Manager and
Rclone UI are Tauri webviews; Rclone Browser is Qt5 and window-first; RcloneDriveManager
does mount/unmount only. They surface `core/stats` job progress, which covers explicit
`copy`/`sync` but not writes that entered through a mount. The common workaround is grepping
rclone's log for `vfs cache: queuing for upload`.

So: **make the write-back queue visible**, and make mounting and unmounting safe around it.

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

Attribution therefore strips the tag and compares **by path component**, not by substring: two
mounts cached at `…/srv/photos` and `…/srv/photos-backup` must not claim each other's
transfers, and a degenerate cache path of `/` must not claim everything. That is a bounded,
deterministic match rather than the prefix guessing the original objection assumed.

The decision therefore rests on lifetime and failure isolation alone, which is the honest
basis for it.

What we get: mounts survive service restart, crash and upgrade; systemd handles restart,
backoff and rate-limiting so we do not reimplement it; and one wedged remote cannot take the
others with it.

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
| **T1** | `core/stats` `transferring[]` | `{name, size, bytes, percentage, speed, speedAvg, eta, group, srcFs, dstFs}` | Per-file progress bars. **Confirmed available** for VFS write-back uploads (#9). |
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

- T1 → per-file progress bars, real ETAs
- T2 → per-file sizes and an in-flight flag, aggregate rate, **no per-file percentages**
- T3 → counts only
- T4 → file list with sizes, aggregate bytes, coarse derived rate, **no in-flight flag**

A progress bar that actually means "we have no idea" is worse than a number. When the tier
degrades mid-session — rc goes away and we drop to T4 — the UI must visibly lose precision
rather than freeze on stale figures.

### Measured behaviour worth knowing

All from #9, against rclone v1.75.0, and all encoded as tests in `rvt-core`:

- `transferring` is **absent, not `[]`,** when nothing is in flight. "No information" and
  "nothing in flight" are different, and the models keep them different.
- `eta` is **null** early in a transfer.
- The **first `speed` reading is unreliable** — it averaged several times the true rate.
- `percentage` **never reached 100**; completion is the entry disappearing.
- There is a **~0.6 s window** where `vfs/queue` still lists an item as `uploading` after
  `core/stats` has dropped it. Hold the last known value rather than snapping to zero.
- `transferring[]` lags `vfs/queue` by `--vfs-write-back`. That window is "queued", not "0%".
- `vfs/stats` `diskCache.bytesUsed` read **0** throughout a 128 MiB upload. It is not pending
  bytes and not reliably cache size either. Do not show it.

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

- **The rc endpoint is a UNIX socket, never a TCP bind.** A TCP listener is reachable by every
  local user and, misconfigured, by the network.
- The socket lives under `$XDG_RUNTIME_DIR`, which is `0700` per the XDG specification and is
  what actually excludes other users.
- rclone performs no `chmod` or umask handling when binding (`net.Listen` only), so the socket
  gets `0777 & ~umask` — `0755` under the common umask `022`, `0775` under `002`. The fix is
  **`UMask=0077` on the systemd unit**, so the socket is never created permissive in the first
  place. Chmod-ing after rclone has bound and is accepting connections leaves a window open,
  and belongs only as a belt-and-braces follow-up.

### The boundary that does matter: D-Bus

The service publishes its interface on the **session bus**, which every process running as
the user can reach — including sandboxed applications granted session-bus access. That
interface is what turns "on the session bus" into "can enumerate this user's remotes and
mount points, and can call `Unmount(force = true)`", the one operation in the design that
destroys data. This, not the rc socket, is where the effort goes:

- The D-Bus interface is a **curated set of methods**, never a generic rc passthrough. The
  service's job is to be the narrow safe surface over a dangerous API; proxying it wholesale
  would hand that straight back out.
- `core/command` and `config/dump` are never called, and never reachable from a client.
- **A forced unmount is destructive and must be treated as such** — refused by default, with
  `force` an explicit parameter a client has to set, never inferred. The safety check lives
  service-side so a buggy or hostile client cannot skip it.
- **Credentials are never read.** Mounting and reporting progress need remote *names* and
  paths, not secrets. Note that `config/get` is not a per-field getter — it returns a whole
  remote's configuration, credentials included — so there is no "read just the safe fields"
  option, and none is needed.
- Nothing published over D-Bus carries credentials or full remote configuration.

## Data flow

```
rclone (per mount, systemd user unit)
   │  rc over UNIX socket (0600)
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
