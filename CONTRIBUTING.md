# Contributing

Thanks for looking. The project is early — the crates and CI are real, the applet is not yet
— so most of what follows is about how to work on it.

For what the project is and how to use it, see [README.md](README.md). For how it is meant to
fit together, see [DESIGN.md](DESIGN.md); read that before changing anything structural.

## Development environment

```sh
git clone https://github.com/stuckj/rclone-vfsmount-tray
cd rclone-vfsmount-tray
cargo test
```

That is the whole setup for every crate except the GTK client: they link no system C
libraries, so nothing needs installing beyond a Rust toolchain.

`rust-toolchain.toml` pins `stable`, so rustup will fetch it regardless of what you have
installed. The declared MSRV of **1.87** comes from `zbus` and is checked by its own CI job,
not by local builds — so a change can pass everything locally and still fail MSRV.

The GTK client is excluded from the workspace's default members and built explicitly:

```sh
cargo build -p rclone-vfsmount-tray-gtk
```

It will need the GTK4 development headers once it gains its `gtk4` dependency. Today it has
none and builds anywhere.

To exercise anything against a real rclone you will want `rclone` (v1.75.0 is what the
behaviour here has been measured against; the floor is 1.61) and `fuse3` on `PATH`. Note
`fusermount3` is not optional for the service: unmounting a live mount goes through it.

To run the service against your own config while developing:

```sh
cargo run -p rclone-vfsmount-trayd -- --log-level debug
```

It reconciles, takes `io.github.stuckj.RcloneVfsmountTray` on the session bus and stays up
serving it. It mounts nothing on its own — `auto_mount` is not acted on yet (#98) — so it
waits to be asked. If a service is already running it exits saying the name is taken, so
stop that one first. `--foreground` is accepted but does nothing yet: there is no background
mode for it to opt out of.

To ask it something, use the client subcommands in another terminal — `cargo run -p
rclone-vfsmount-tray -- list`, `… mount <name>`, `… status --json`. They drive it over the
same D-Bus interface a front end will (see [docs/CLI.md](docs/CLI.md)), and work with no tray
running.

## Before opening a pull request

```sh
cargo fmt --all
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --workspace
```

All four run in CI and all four are blocking. `--locked` matters: the lockfile is committed,
and without it you can pass locally against dependencies CI will not use. `--workspace` on
the last one matters for the same reason — a broken intra-doc link in the service crate is
invisible to a `-p rvt-core` run and fails CI. Read clippy's output rather than trusting its
exit status.

The man pages under `docs/` and the completions under `completions/` are generated from the
binaries' `clap` definitions, and a test diffs the committed copies against a fresh render, so
changing a subcommand, flag, help string, or the crate version without regenerating fails the
test suite. Regenerate and commit the result:

```sh
REGENERATE_CLI_DOCS=1 cargo test --workspace committed_man_and_completions_match_the_cli
```

## Testing

Tests live next to what they test — `#[cfg(test)] mod tests` in the same file for unit tests,
`crates/core/tests/` for anything that has to compile as a separate crate.

Three conventions matter more than coverage:

**Verify a guard by breaking the thing it guards.** If you add a check, confirm it fails when
the property it protects is violated. This is not pedantry — this repo has shipped several
tests that could not fail, including a fixture suite whose whole purpose was catching
wire-format drift and which could not detect a renamed field.

**Fix the class, not the reported instance.** When you fix a bug, look for the same mistake
elsewhere before you commit. The most common defect in this project's history is a fix applied
only to the input someone happened to report.

**Size a sample to the effect you are measuring.** If you are chasing a flaky test, 25 runs
cannot detect a sub-1% failure rate. Run the compiled test binary in a loop rather than
`cargo test`, and run it enough times to mean something.

### Scratch directories

Anything a test needs on disk comes from `rvt_testutil::Scratch`, never from
`std::env::temp_dir()` directly:

```rust
use rvt_testutil::Scratch;

let dir = Scratch::new("my-test");   // unique, empty, removed on drop
let cfg  = dir.write("config.toml", b"...");
let sub  = dir.dir("cache");
```

It removes itself however the test leaves the stack, including while a panic unwinds — which
is when a failing test would otherwise leave the most behind — and including when the test
chmodded a directory unreadable and never got to the line restoring it. A test that builds its
own path under the temporary directory is rejected by
`crates/testutil/tests/no_stray_scratch.rs`.

Set `RVT_KEEP_SCRATCH=1` to keep the directories, for when a failure is easier to read from
what the test wrote than from the assertion. Each kept path is printed, so pass `--nocapture`
to see the paths of tests that passed — libtest swallows the output of those that did.

### Fixtures

`testdata/` holds JSON captured from a live rclone, not hand-written examples. When you need a
new one, capture it — a tidy fake will agree with whatever assumption you already had, which
is exactly what the fixture exists to challenge.

`crates/core/tests/fixtures.rs` pins each file's full key set in both directions, so an added,
removed or renamed field fails a test naming the path. Updating those lists when rclone
changes is the point, not a chore.

## Code style

`rustfmt` defaults, enforced. Beyond that:

**Comments earn their place or go.** The code should generally speak for itself. Add a comment
where something is genuinely not derivable from the code in front of you — a non-obvious
ordering constraint, an external behaviour being relied on, an alternative that looks right
and is not — and keep it short. Do not restate what the code says: a second description of the
mechanism is a second thing to keep true, and in this repo a stale copy has already caused a
real bug.

**Comments describe the code, not the project's history.** What broke while building it, which
review caught it, what an earlier design assumed — that belongs in the commit message, the PR
or the issue, where it is permanent and searchable and out of the reader's way.

**Do not claim more than you verified.** If a comment asserts something about rclone, check it
against rclone's source or output first. Several comments here have been wrong in ways that
survived review because they sounded plausible.

## Documentation

Four places, and it matters which one you reach for:

| | |
|---|---|
| `README.md` | For users, most of whom are not engineers. What it does, how to install it, how to use it. |
| `CONTRIBUTING.md` | This file. How to build, test and submit. No design content. |
| `DESIGN.md` | Directional only — components, boundaries, who owns what, and decisions that constrain future work. It changes when the *shape* of the design changes, not when its details do. |
| the code | The authority on how anything actually works. |

If your change alters what a user can expect, update the README in the same pull request. If
it moves a boundary, adds or removes a component, or reassigns a responsibility, update
`DESIGN.md` in the same pull request. Most changes do neither and need no documentation change
at all — a bug fix, or a different way of computing the same answer, is detail, and detail
lives in the code.

Measurements are the common mistake. A fact about rclone's behaviour belongs in a test that
fails when it stops being true, and in the pull request or issue that recorded it — both
dated. `DESIGN.md` cannot fail, so a measurement written there quietly outlives its truth.
The exception is a measured fact a design decision rests on, which stays in `DESIGN.md` as a
clause naming the issue that took it — without it the decision cannot be read.

## Pull requests

Keep them reviewable. The first PR here was 5,600 lines and took seven review rounds to
settle, largely because it bundled unrelated work.

Commit messages should say **why**, not what — the diff already says what. If a change
corrects an earlier decision, say so and say what was wrong with it.

Reference the issue you are closing. CI must be green, and branch protection requires an
approving review and a resolved conversation on every thread.

## Filing issues

For a bug, the most useful thing is `rclone version`, your `config.toml` with any secrets
removed, and what the service logged. `--log-level debug` is usually enough.
