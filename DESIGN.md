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

Five crates, three processes. Four of them ship; the fifth exists only for tests.

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
privileged position between them — the tray is not "the app" with a settings window bolted
on; it is one of two equal front-ends.

`crates/gtk` will be the only crate that links a system C library, once it gains its `gtk4`
dependency — today it has none, so nothing in the workspace links one and tier-1 CI can run
`--workspace` across all five. The boundary is nonetheless load-bearing and enforced from
the start: once `gtk4` lands, the other four still lint and test on a bare runner with no
`apt-get install` step, so the common path stays fast. `crates/gtk` is excluded from the
workspace `default-members` for the same reason — a bare `cargo build` works without GTK
headers installed.

### Scratch directories for tests

`rvt-testutil` exists so no test builds its own path under the system temporary directory.
A leaked directory there is resident memory wherever `/tmp` is tmpfs, it accumulates every
run — 77 directories per full suite before this, never reused — and nothing reclaims it
until reboot. How much headroom that eats is a property of the machine, so it is not
recorded here; #75 has the measurement that prompted the work.

Two properties of the layout are load-bearing, and both were mistakes first:

- **No level of the path is shared between users.** The root is
  `<tmpdir>/rvt-test-<pid>-<stamp>`, straight into the temporary directory. A fixed
  intermediate directory is created by whichever user runs the suite first, at their umask,
  and every later user on that machine then fails to write inside it — for the rest of the
  boot, with an error naming a directory they have no reason to know exists. It would also
  be a stable path under a world-writable parent for anyone to pre-plant.
- **The name is not the pid alone.** Pids are reused, so a run killed hard enough to skip
  every destructor would hand its leftovers to a later process that drew the same pid,
  which would then see a dirty scratch and pass or fail for the wrong reason. A truncated
  nanosecond stamp makes that improbable — two processes need the same pid *and* start
  times an exact multiple of 4.295s apart — rather than impossible, so each scratch is
  created with `create_dir` rather than `create_dir_all` and a directory that is already
  there is refused instead of adopted. The truncation is deliberate: the full stamp costs
  11 bytes of the 108 available to a UNIX socket path.

Removal restores directory permissions and retries before giving up, because a test that
chmods a directory unreadable and panics before restoring it leaves a tree its own owner
cannot walk — which is exactly the case the crate exists to survive.

### The lifetime rule

**Mounts belong to the service. Nothing a client does can unmount anything.**

Someone who quits the tray to declutter their panel, or whose tray crashes, or who logs into
a session where the tray never starts, must find every mount exactly as they left it.

The rule extends to the service itself: **restarting the service does not unmount.** It
crashes: this is a young program that polls rclone, walks cache directories and talks
D-Bus, and tying whether a filesystem is reachable to whether it has a bug is far too wide
a blast radius for what it is. It is also restarted by hand, and by whatever is managing
it.

Nor is an unmount free to take back. rclone exits on `SIGTERM` **without** flushing its
write-back queue — measured, in [the unmount order](#the-unmount-order) — so an unmount at
a moment the user did not choose can sever a write in flight. The cache is on disk and
resumes, but the file that was mid-write does not un-truncate.

**Whether a system update restarts this service is not established on any platform it
ships to.** It is a `systemd --user` service, and that is the distinction every answer
turns on:

- `.deb` / `.rpm` — the Debian helpers manage *system* units. Restarting a running user
  unit across sessions is not something packaging normally does. Unverified; #30.
- NixOS — `nixos-rebuild switch` is long documented as **not** reliably restarting or
  reloading systemd user services
  ([nixpkgs#29146](https://github.com/NixOS/nixpkgs/issues/29146)), with
  `systemctl --user daemon-reload` the usual advice.
- Home Manager — `systemd.user.startServices = "sd-switch"` **does** restart changed user
  units on switch, so under that install path an update can restart us. #34.

So it depends on how the user installed, and none of the reasoning above rests on it. It
is written down as a question rather than deleted, so the next person does not re-derive
it from scratch.

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
| User clicks Unmount | unmounted — refused while anything is still using the mount, unless forced. The pending-uploads *warning* is still #19 |
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

### The unit prefix is what makes a mount ours

Every unit this service starts is named `rvt-mount-<mount>.service`, and ownership is
decided from that name: a mount point the kernel lists with no unit of ours behind it
belongs to somebody else, and is never started, restarted or detached — only adopted for
display, and released outright if the user forces it (#18).

Building that name from the config entry is not enough on its own, because the config
changes under running mounts. Rename `backup` to `backups` and `rvt-mount-backup.service`
is still up and still serving the same path while the name now asked about does not exist,
so the mount reads as *foreign* — and everything ownership gates then works against the
user (#71). An unmount refuses; `mount` reports the path as somebody else's; and because
`Release::Detach` is gated on ownership, `force` on a **busy** mount of theirs dead-ends in
`Busy` rather than stopping the unit and escalating, which is the one case where forcing
was the whole point.

Releasing the point without stopping the unit is not, in itself, the disaster it looks
like. Measured on rclone v1.75.0 and Linux 6.8, against a transient unit carrying the
properties this service sets: `fusermount3 -u` on a live mount makes rclone exit **0**, so
`Restart=on-failure` never fires, the unit settles `inactive`/`success` with `NRestarts=0`,
and systemd collects it. No stray unit is left. The damage is the refusals above, not a
resurrected rclone.

The prefix is therefore swept as well as constructed. `ListUnitsByPatterns("rvt-mount-*")`
lists every unit of ours systemd still holds; one that no config entry names and that is
still serving is **orphaned** — ours, distinct from foreign, and stoppable. Where it
mounts comes from its own `ExecStart` argv, the only place the unit-to-mount-point mapping
survives a config edit, and a unit whose argv cannot be read is left alone rather than
guessed at. An orphan then comes down exactly as any mount of ours does, kernel first and
`StopUnit` second, whether it is addressed by the name its unit runs under or reached
through the configured mount whose point it is holding.

Deciding that from the argv alone would be too generous, because the argv records where a
unit *meant* to mount and outlives its rclone. So a unit counts only while systemd reports
it `active` or `deactivating`, and only if no configured mount at that point has a unit of
its own up. `activating` is excluded deliberately: it covers the gap before a restart, in
which the previous rclone has already exited, and a unit on its way out would otherwise
claim the very mount that replaced it.

What is mounted there is **not** required to match the `remote:path` the unit was given.
mountinfo carries the Fs rclone resolved rather than the argument it was handed — measured
on v1.75.0, an `alias` remote mounted as `ali:` reports its backing path, and a trailing
slash in the argument is dropped. Demanding equality quietly excused every such config
from the sweep, which is the failure above, left in place with nothing to show for it.

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
| **T4** | Cache directory scan | `vfsMeta/<backend>/<path>` JSON `{Size, Dirty, …}` plus the data file's own size | Bytes to send is the **data file's** size summed over dirty items — the descriptor's `Size` is stale while a handle is open — 0 for a new file, the previous size for a rewrite (#10), so summing it reports nothing during exactly the copy the user is watching. Survives an rclone crash, and is the only tier that sees a write before it is closed. **Meets the bar with no rc at all in principle** — but as implemented the cache roots come only from `vfs/stats`, so a mount that has never answered has nowhere to scan; composing them from `rclone config paths` is the rest of #22. |

T4 is a first-class tier, not a fallback of last resort. It is the only tier that works when
the rclone process is unreachable, and the only one that survives a crash, because the rc
endpoints only know a *running* process's in-memory queue.

Its honest limits: no per-file upload progress (the cached file reflects what the application
wrote, not what was uploaded); no in-flight flag (`Dirty` stays true until upload completes,
so "queued" and "uploading" are indistinguishable); and the aggregate rate must be derived by
differencing total dirty bytes as files drop out, so large files stall then jump.

Two more that come from walking rather than watching. A walk is bounded at 50 000 entries,
and under `full` every *read* file is one — so on a large media cache it reports itself
incomplete rather than pretending to a total. And the roots come from `vfs/stats`, so a mount
that has never answered has nowhere to scan. Both are what the inotify half of #22 removes:
watching the tree costs nothing per poll, and a watcher has to resolve the root once anyway.

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
### Writes that never enter the write-back cache

Measured against rclone v1.75.0 on live FUSE mounts (#21), having previously been read from
rclone's source and recorded here as unverified. Every claim below held.

Not every write through a mount enters the write-back cache:

- Under `--vfs-cache-mode off`, writes stream straight to the remote via `operations.Rcat`.
- Under `minimal`, only *write-only* opens stream. Read-write opens, and any file already in
  the cache, go through it normally — visible to T4 immediately, and to T2/T3 once closed.

A streamed write is **visible but unattributable**. Eight seconds into a 20 MB write on a
`minimal` mount: `vfs/queue` was `[]`, every `diskCache` *upload* counter was `0` (`files`
was 1, since the count tracks cache entries rather than pending work), and the transfer
appeared in `core/stats` `transferring[]` with `name`, `dstFs`, `group` and byte progress —
but `size: -1` and **no `srcFs`**, which is the field that ties a transfer to a mount's
cache. So it cannot be attributed to a mount, and `vfs/queue` never knows about it.

What distinguishes the three modes over rc:

| `--vfs-cache-mode` | `opt.CacheMode` | `vfs/queue` | `diskCache` | the queue is |
|---|---|---|---|---|
| `off` | 0 | `{}` — **no key** | absent | nothing |
| `minimal` | 1 | `{"queue":[]}` | present | a **floor** |
| `writes` / `full` | 2 / 3 | `{"queue":[…]}` | present | a floor **while any file is open** |

`minimal` builds a cache *and* a queue, so it is identical to `writes` at every endpoint
except `opt.CacheMode`. That ordinal is therefore load-bearing, and code reading it must
fail **closed**: a mode it cannot parse has to be treated as "writes may be streaming",
never as "all writes are queued".

The consequence for the UI: on a mount configured `off` or `minimal`, "nothing pending" does
not mean "nothing in flight". The applet says the mount is unmonitored (`off`) or partially
observed (`minimal`) rather than implying it is idle — and the T1 data is rich enough to show
the transfer, just not to attribute it, so an "unattributed transfers" line is implementable.

### rclone enqueues on close, so an empty queue is not an idle mount

Measured on a live `--vfs-cache-mode writes` mount (#21), the default mode. With a 12 MB file
open and 8 MB written but not yet closed:

```
vfs/queue                    []
diskCache.uploadsInProgress  0        diskCache.uploadsQueued  0
diskCache.bytesUsed          0        diskCache.files          1
core/stats.transferring      absent
cache file on disk           8388608
vfsMeta                      "Dirty": true
```

Every rc endpoint reports nothing outstanding while 8 MB sits dirty and unsent. The item
enters `vfs/queue` on `close()`, not as it is written — so this is the state for the whole
duration of any large copy, not a narrow race.

**No rc field answers this.** `diskCache.files` moves with the write, but it counts clean
entries too: it stays up for the cache's retention window (`--vfs-cache-max-age`, default
1h) after an upload finishes, and under `full` a plain *read* creates one — measured, one
read and no write leaves `files: 1` with the descriptor's `Dirty` false. Counting cache
entries would condemn every read-mostly `full` mount, which is what `full` is recommended
for, to a permanent "cannot tell".

Only the on-disk `Dirty` flag under `diskCache.pathMeta` is exact — `Dirty` is set when the
file is *written*, measured both for a first write and for a rewrite of an already-uploaded
file, whereas an item reaches `vfs/queue` only on close. `rvt_core::scan` reads it, and the
poller falls to it when rclone is unreachable.

**Reading it on every poll of a live mount is still the wrong shape, and is deliberately not
done:**

- rclone rewrites descriptors non-atomically — 260 zero-length reads in 291,614 while a
  mount was under load — so a single-shot read votes "clean" for a file that is dirty.
- A clean tree is read in full every time, and "queue empty" is the *permanent* state of a
  read-mostly mount: ~0.7 s per 15 s poll over 50k descriptors, ~7 s over 500k.
- The path comes from an rc response, so walking it needs the untrusted-input handling this
  document already requires below.

A torn read is not the same as a clean one: `rvt_core::scan` counts an entry it could not
read and refuses to call the result complete, so a walk that lost a race says so rather
than voting "clean". What remains is the cost, which is why the walk runs only when rclone
is unreachable, and at the idle cadence even then. Note "unreachable" also covers a timeout
or a transport fault, which a live and busy rclone can produce — so this is a cheaper place
to walk, not a guarantee that nothing is moving. Making it cheap enough to run
continuously is what inotify is for, in the rest of #22.

**T4 is therefore strictly better than T2 for this one question**: the single place the
ladder's ordering does not hold.

On a *live* mount the queue is still taken at its word, and **that blind spot could lose
data**. Measured end to end on rclone v1.75.0, `--vfs-cache-mode writes`, 15 MB written to
a file still open:

1. `fusermount3 -u` does refuse: exit 1, `Device or resource busy`.
2. But `unmount()` did not start there. It called `StopUnit` first, and with
   `KillMode=mixed` that is a SIGTERM. rclone logs `Failed to unmount: … Device or
   resource busy` and **exits anyway**, in under a second. The mount goes `ENOTCONN` and
   the writer is severed.
3. On the next mount with the same `--cache-dir`, rclone uploads the dirty cache item as
   it stands. The remote received `held.bin` at 15728640 bytes — a truncated object
   presented as complete.

### The unmount order

Step 1 is the only signal there is, so `unmount()` asks the kernel **first** (#73):
ownership and source-mismatch checks, then `fusermount3 -u`, then `StopUnit`. Those checks
moved ahead of the stop for the same reason as the release did — refusing after the
SIGTERM has gone out refuses nothing.

- Release succeeds — nothing was holding it, and the stop is bookkeeping.
- Release fails, point gone anyway — a crash or a concurrent stop won the race.
- Release fails, point still there — **refuse**, with nothing signalled.
- `force` (#18) warns and escalates.

`fusermount` reports every non-zero exit alike, so `EBUSY` cannot be told from "not a
mount point". The refusal therefore fails closed and says what was refused rather than
asserting why; a failure to *run* it passes through as itself.

#### What `force` has to do

Measured on rclone v1.75.0 and Linux 6.8, 15 MB written to a file still open:

| step | result |
| --- | --- |
| `-u`, rclone alive, holder's fd open | refused |
| SIGTERM → rclone exits without unmounting | point stays in mountinfo |
| `-u`, rclone **dead**, fd still open | **still refused** |
| `-u`, after the holder exits | succeeds |
| `-u -z`, rclone dead, fd open | succeeds |

Row three is the one to know, and the easy one to assume away: killing rclone does not
free the mount, because the *holder's* descriptor pins it. So `force` is refuse → stop →
refuse → detach. "Refuse → stop → unmount" would end with the file sacrificed **and** the
unmount failed. `-z` is reachable only on that last step, where the writer is already
severed and detaching costs a mount-table entry — hence `Release::Refuse` and
`Release::Detach` as separate operations rather than a flag.

Detaching is gated on ownership too: a held **foreign** mount refuses even under `force`,
since its rclone was never signalled and `-z` would strand it serving a mount nothing can
see. An orphan is not foreign for this purpose — its unit is stopped like any other of
ours, so the escalation is reachable. The settle wait after the stop keeps its full length,
because a holder letting go inside it is the difference between a clean release and having
to detach.

`fusermount3` is now required to unmount any live mount. rclone is a static binary that
execs it to mount anyway, so this adds no dependency.

None of it makes `TransferState::safe_to_unmount()` whole: that still cannot see an open
write, so a mount it calls idle can be refused. #22 is what would close the gap.

### Two field names that mean less than they say

Also measured against rclone v1.75.0 (#21). `tries` is pinned by
`testdata/vfs-queue-retrying.json`; the `erroredFiles` behaviour is measured but **not**
pinned, since it needs a capture taken while a remote is refusing writes:

- **`tries` counts attempts, not failures.** It increments when an attempt *starts*, so a
  healthy in-flight upload reports `tries: 1`, and so does one that has already failed once
  and is backing off. Above 1 is proof; 1 is ambiguous, and `uploading: false` alongside it
  is what separates the two.
- **`erroredFiles` stays 0 while a file is failing.** A file refused on every attempt for
  four minutes, backing off to a 64 s delay and reaching `tries: 7`, left
  `diskCache.erroredFiles` at 0 throughout. `tries` is the only signal that a file is stuck.

### `--umask` is two flags wearing one name

Measured (#69) against the official `linux-amd64` builds on Linux 6.8, mounting `:memory:`
so that no on-disk permission can stand in for the computed one. Modes are of a file and a
directory created inside the mount:

| rclone | how the flag parses | `--umask 0022` | `--umask 22` | `--umask 0o22` |
|---|---|---|---|---|
| 1.61.0 – **1.67.0** | pflag `int` → `ParseInt(s, 0, 64)` | 644 / 755 | **640 / 751** | 644 / 755 |
| **1.68.0** – 1.75.0 | `vfscommon.FileMode` → `ParseInt(s, 8, 32)` | 644 / 755 | 644 / 755 | **mount fails** |

1.67.0 is the last release of the first kind and 1.68.0 the first of the second, with no
1.67.x between them. Every minor from 1.61 to 1.75 was checked for the flag's type, and both
ends of each row for the modes.

Base 0 is Go's own literal rule: a leading `0` is octal, bare digits are decimal, `0x` is
hex. So `--umask 22` masks `0o026` on the old flag and `0o022` on the new one. The bit that
moves is **other-read** — group-write is stripped either way — so the old flag's reading is
the *tighter* of the two, and nothing reports the difference. `0o22` goes wrong in the other
direction: accepted by the old flag, and on the new one a parse error that stops the mount.

A third spelling still stops it. From 1.68.0 the flag is parsed as a **signed** 32-bit int,
so a mask above `0o17777777777` is refused outright — `--umask 020000000000` fails to start
on 1.68.0 and mounts happily on 1.67.0. `Config::validate` refuses those, rather than
canonicalising a value into a flag rclone will throw out.

The ceiling sits there and not at the `0o777` that is all rclone can *use*. Everything
between the two mounts fine on every supported version, because masking `0666` and `0777`
discards the extra bits — `--umask 07777` and `--umask 0777` produce identical modes.

The rule is **refuse only what cannot work on a supported rclone**, and it is deliberately
not "refuse nothing that works today": a mask above `i32::MAX` did load before, and still
mounts on 1.67.0, but which rclone will exec is not knowable when the config is read, so a
value that dies on half the range is refused for all of it. What the rule does rule out is
refusing `0o7777`, which works everywhere and merely wastes bits.

That asymmetry is worth the care because an invalid config is not a local failure.
`Config::load` fails, so the **service** exits at startup — no tray, no reconcile, no
auto-mount — over one mount's typo. The mount units themselves survive it: their
`ExecStartPre` is registered `ignore_errors`, so `prepare-mount` failing does not stop rclone.
It leaves the preparation undone instead, which is its own problem — the stale socket and
stale mount point that hook exists to clear are still there, and the next restart is the one
that fails.

Hence `canonical_umask` in `rvt-core`, which re-spells the `umask` **field** as leading-zero
octal — the one form both parsers take and agree on. `extra_args` is exempt by design: it is
the verbatim escape hatch, so `--umask` given that way still means whatever the running
rclone thinks it means. **The version does not enter into it.** Feature detection is the rule
here (see the capability ladder), and this is the same instinct applied where there is
nothing to detect: an argv that is right for every supported version is still right when the
binary is replaced between discovery and the next mount.

One consequence is worth stating plainly, because it changes the permissions reported for
files that already exist. A bare-digit umask on rclone ≤1.67.0 was being read as decimal, so
normalising it changes the mask on the next remount, with no config edit and no prompt.
Measured:

| config | before, on ≤1.67.0 | after, on any version | what moves |
|---|---|---|---|
| `umask = "22"` | 640 / 751 | 644 / 755 | other gains read |
| `umask = "63"` | 600 / 700 | 604 / 714 | other gains read; group gains execute on directories |
| `umask = "12"` | 662 / 763 | 664 / 765 | other **loses write**, gains read |
| `umask = "755"` | 404 / 414 | 022 / 022 | group and other **gain write**; owner loses read |

It moves in both directions, and not by a little: `"12"` takes the reported write bit off
other while adding read, and `"755"` — a file *mode* written into a mask field, which is the
likeliest way to end up with a bare-digit value — reports everything in the mount as `022`,
unusable to its owner on paper.

**On paper is the whole of it. These bits are reported, not enforced.** FUSE checks a file's
mode only when mounted with `default_permissions`; rclone exposes that as
`--default-permissions`, leaves it off, and `mount_args` never passes it. Measured on 1.75.0:

- `--umask 0777` gives a mount root of `d---------` and files of `----------`, and the owner
  reads and writes them normally.
- `--uid 4242 --gid 4242 --umask 0077` reports `-rw------- 4242 4242`, and uid 1001 reads the
  contents in full.
- Add `--default-permissions` to that same command and the read is refused. So the flag is
  what would make the mode mean something, and it is not one we pass.

What the change really moves, then, is the mode every tool *reads* out of the mount — `ls`,
`rsync -p`, `tar`, `cp -p`, a script testing `-w`, git's executable bit — and the modes copies
made from the mount inherit. That is worth telling someone about; it is not an access-control
change, and reading these rows as one would send a user hunting an exposure that cannot have
happened. Who can reach the mount at all remains `allow_other`'s question, and with
`allow_other` set the mode gates nothing either, for the same reason.

Nothing rewrites the config, so an ambiguous spelling stays in the file and the question
comes back at every mount. The supervisor therefore logs a warning as it starts any mount
whose spelling would have meant a different mask before 1.68.0, naming both — `before_1_68`
and `now`, as effective masks, so that what it prints is always something that can be written
straight back into the field.

Two things it deliberately does not do. It is silent for a leading-zero value, which means
the same to both parsers: a warning that fires for the config the example recommends is one
everybody learns to ignore. And it is not gated on the version of rclone in hand, because
what it reports is a property of the *spelling* — that config is ambiguous whichever build
reads it, and the remedy is the same either way. The cost is that someone who has only ever
run 1.68.0 or later sees a warning about a mask that never moved for them; the two fields are
what let them work that out.

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
  off. Being service-side, the kernel's refusal cannot be skipped by a client that simply
  omits it — but it does not stop a caller that deliberately passes `force = true`, which
  logs a warning naming the mount and then severs and detaches. That is a guard against
  accident and bugs, not against malice. (The pending-uploads check this paragraph used to
  name is not implemented; it is #19.)

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
