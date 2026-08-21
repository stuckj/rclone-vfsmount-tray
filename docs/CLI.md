# Command-line interface

The `rclone-vfsmount-tray` binary is two things in one. Run with a subcommand it is a
scriptable D-Bus client that drives the service; run with none it raises the tray icon, which
the [README](../README.md#the-tray-icon) describes. The subcommands need no graphical session —
they work over SSH, and they are how the integration tests drive the system.

They talk to the service and to nothing else. They hold no mount, and they do not start the
service: if it is not running they say so and tell you how to start it, rather than starting
it behind your back.

## `rclone-vfsmount-tray` — the client

```
rclone-vfsmount-tray [--log-level LEVEL] [COMMAND]
```

`--log-level` is one of `off, error, warn, info, debug, trace`, and takes precedence over
`RUST_LOG`. All logging goes to stderr, so a command's stdout stays clean for `--json` and
for pipes.

### Subcommands

| Command | What it does |
| --- | --- |
| `list` | Print every configured mount and its state, one per line. |
| `mount <name>` | Bring up a configured mount. Returns once it is serving, not once rclone is spawned — for a cold remote that can take most of a minute. |
| `unmount <name> [--force]` | Tear a mount down. `--force` detaches the mount point from whatever still holds it, severing a write in progress; it is never implied. |
| `status [--json]` | Print the service's versions and every mount's state and outstanding uploads. |

With no subcommand the binary raises the tray icon and stays up until it is asked to stop —
from its own Quit item, or by `SIGTERM` or `SIGINT`. It exits `0` then, and `5` if it could not
reach the session bus to raise an icon at all. A panel that is not yet running is not a
failure: the tray waits for one.

### Exit codes

A distinct code per outcome, so a script can branch without parsing prose:

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | The service answered and refused — unknown mount, mount point busy, and the like. The message is the service's own. |
| `2` | Usage error: an unknown flag, or a `--log-level` that is not a level. |
| `3` | The service is not running. |
| `4` | The service speaks an interface this build cannot use — a different major version, or one too old for the command. |
| `5` | No session bus; the service took the call and did not answer; or the call failed for some other reason. |

Codes `3`, `4` and `5` all mean the client never got an answer. None of them says anything
about your mounts: a mount that was up before is up still. This is why `status` reports them
as a disconnection and never as an empty mount list.

## `status --json`

The stable surface for scripting and for the integration tests (#38, #54). Fields are added
over time — a consumer should ignore keys it does not know — but the shape below does not
change under a consumer's feet without the interface version moving with it.

### Connected

```json
{
  "connected": true,
  "service": {
    "service_version": "0.1.0",
    "interface_version": 1,
    "client_interface_version": 1,
    "rclone_version": "1.75.0",
    "capability_tier": "unknown"
  },
  "mounts": [
    {
      "name": "photos",
      "state": "mounted",
      "live": true,
      "managed": true,
      "reason": null,
      "mount_point": "/home/j/mnt/photos",
      "remote": "gdrive:Photos",
      "transfer": {
        "fidelity": "T2",
        "outstanding_known": true,
        "has_progress": false,
        "pending_files": 3,
        "pending_known_bytes": 1288490188,
        "pending_unknown_size_files": 1,
        "uploading": 1,
        "errored_files": 0,
        "out_of_space": false,
        "rate_bytes_per_sec": 4194304,
        "degraded_reason": null,
        "files": []
      }
    }
  ]
}
```

- `state` is one of `unmounted`, `mounting`, `mounted`, `unmounting`, `failed`, `foreign`,
  `orphaned`. `live` and `managed` travel alongside it so a consumer meeting a `state` it does
  not recognise still knows whether a filesystem is serving (`live`) and whether this service
  owns it (`managed`).
- `transfer` is `null` when the service has no reading for that mount. When present, an
  **absent or `null`** numeric field means *this could not be measured*, which is not the same
  as zero. `outstanding_known` is the flag to check before trusting `pending_*` as complete;
  `fidelity` names the tier that produced the reading, or is `null` when no source could total
  it. `capability_tier` on the service is a property of rclone, not of any one reading — do not
  render anything on the strength of it.

### Disconnected

When the client cannot reach the service, `status --json` still prints a document — and
`mounts` is **`null`**, never `[]`. An empty array would say "the service is up and has no
mounts", the one thing this must never claim when it simply could not ask.

```json
{
  "connected": false,
  "reason": "service not running",
  "detail": "The rclone-vfsmount-tray service is not running. Start it with: …",
  "start_hint": "systemctl --user start rclone-vfsmount-trayd",
  "mounts": null
}
```

`reason` is one of `service not running`, `no session bus`, `interface incompatible`,
`service too old`, `service did not answer`, `service unreachable`. `start_hint` is present
only for a stopped service.

`service did not answer` means the service took the call and did not reply — it is running,
and either still working or wedged. Only a session bus configured with a `reply_timeout`
produces it; the default configuration sets none, so a call to a wedged service waits.
`status --json` exits non-zero in this case, so a script can branch on the exit code or on
`connected` — the two agree.

## `rclone-vfsmount-trayd` — the service

```
rclone-vfsmount-trayd [--config PATH] [--log-level LEVEL] [--foreground]
```

`--config` overrides the configuration file (default
`$XDG_CONFIG_HOME/rclone-vfsmount-tray/config.toml`). `--foreground` is accepted for running
outside systemd while debugging. The service takes the well-known name on the session bus and
serves until stopped; stopping it leaves every mount exactly as it is.

## Man pages and shell completions

Both binaries ship a man page and bash/zsh/fish completions, generated from the same `clap`
definitions the binaries parse with, so they cannot describe options the binary does not have:

- Man pages: [`docs/rclone-vfsmount-tray.1`](rclone-vfsmount-tray.1) and
  [`docs/rclone-vfsmount-trayd.1`](rclone-vfsmount-trayd.1). Read one without installing with
  `man ./docs/rclone-vfsmount-tray.1`.
- Completions: `completions/{bash,zsh,fish}/`. For a quick try in the current shell, e.g.
  `source completions/bash/rclone-vfsmount-tray`; packaging installs them to each shell's
  completion directory.

They are committed, and a test regenerates and diffs them, so a CLI change that does not
update them fails CI rather than shipping stale docs (see CONTRIBUTING.md for the regenerate
command).
