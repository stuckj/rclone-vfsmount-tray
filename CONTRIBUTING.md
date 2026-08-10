# Contributing

Thanks for looking. The project is early — the crates and CI are real, the applet is not
yet — so most of what follows is about how to work on it rather than how to use it.

## Development environment

```sh
git clone https://github.com/stuckj/rclone-vfsmount-tray
cd rclone-vfsmount-tray
cargo test
```

That is the whole setup for every crate except the GTK client: they link no system C
libraries, so nothing needs installing beyond a Rust toolchain.

`rust-toolchain.toml` pins `stable`, so rustup will fetch it regardless of what you have.
The declared MSRV of **1.87** comes from `zbus` and is checked by its own CI job, not by
local builds.

The GTK client is excluded from the workspace's default members and built explicitly:

```sh
cargo build -p rclone-vfsmount-tray-gtk
```

It will need the GTK4 development headers once it gains its `gtk4` dependency. Today it
has none and builds anywhere.

To exercise anything against a real rclone you will want `rclone` (v1.75.0 is what the
behaviour here has been measured against; the floor is 1.61) and `fuse3` on `PATH`. Note
`fusermount3` is not optional for the service: unmounting a live mount goes through it.

## Before opening a pull request

```sh
cargo fmt --all
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

All three run in CI and all three are blocking. `--locked` matters: the lockfile is
committed, and without it you can pass locally against dependencies CI will not use.

If you touched a public item's docs:

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p rvt-core
```

Broken intra-doc links are an error, and the crate cross-references itself heavily.

## Testing

Tests live next to what they test — `#[cfg(test)] mod tests` in the same file for unit
tests, `crates/core/tests/` for anything that has to compile as a separate crate.

Three conventions matter more than coverage:

**Verify a guard by breaking the thing it guards.** If you add a check, confirm it fails
when the property it protects is violated. This is not pedantry — this repo has shipped
several tests that could not fail, including a fixture suite whose whole purpose was
catching wire-format drift and which could not detect a renamed field.

**Fix the class, not the reported instance.** When you fix a bug, look for the same
mistake elsewhere before you commit. The most common defect in this project's history is a
fix applied only to the input someone happened to report.

**Size a sample to the effect you are measuring.** If you are chasing a flaky test, 25
runs cannot detect a sub-1% failure rate. Run the compiled test binary in a loop rather
than `cargo test`, and run it enough times to mean something.

### Scratch directories

Anything a test needs on disk comes from `rvt_testutil::Scratch`, never from
`std::env::temp_dir()` directly:

```rust
use rvt_testutil::Scratch;

let dir = Scratch::new("my-test");   // unique, empty, removed on drop
let cfg  = dir.write("config.toml", b"...");
let sub  = dir.dir("cache");
```

It removes itself however the test leaves the stack, including while a panic unwinds —
which is when a failing test would otherwise leave the most behind, and including when the
test chmodded a directory unreadable and never got to the line restoring it. A test that
builds its own path under the temporary directory is rejected by
`crates/testutil/tests/no_stray_scratch.rs`.

Set `RVT_KEEP_SCRATCH=1` to keep the directories, for when a failure is easier to read from
what the test wrote than from the assertion. Each kept path is printed, so pass
`--nocapture` to see the paths of tests that passed — libtest swallows the output of those
that did.

### Fixtures

`testdata/` holds JSON captured from a live rclone, not hand-written examples. When you
need a new one, capture it — a tidy fake will agree with whatever assumption you already
had, which is exactly what the fixture exists to challenge.

`crates/core/tests/fixtures.rs` pins each file's full key set in both directions, so an
added, removed or renamed field fails a test naming the path. Updating those lists when
rclone changes is the point, not a chore.

## Code style

`rustfmt` defaults, enforced. Beyond that:

**Comments earn their place or go.** Explain what would otherwise be re-derived — a
measured rclone behaviour, why a type is signed, why a check exists. Do not restate what
the code says. If the same rationale belongs in several places, put it in `DESIGN.md` once
and reference it; duplicated explanations drift, and in this repo a stale copy has already
caused a real bug.

**Do not claim more than you verified.** If a comment asserts something about rclone,
check it against rclone's source or output first. Several comments here have been wrong in
ways that survived review because they sounded plausible.

## Invariants you must not break

These are load-bearing. `DESIGN.md` has the reasoning; this is the short form.

**Mounts belong to the service.** Nothing a client does — including exiting, crashing, or
never starting — may unmount anything. Restarting the service must not unmount either; a
package upgrade restarts it. No unmounting from a `Drop` impl, and rclone must not live in
the service's own cgroup.

**`rvt-core`, the service, the tray and `rvt-testutil` link no system C libraries.** Only
the GTK crate will, once it gains `gtk4`; today nothing does, so CI runs the whole
workspace on a bare runner. That boundary is what keeps those four testable there
afterwards. If a dependency drags one in, CI breaking is the intended alarm — do not work
around it.

**Never fake precision the data source cannot support.** How much can be said about
pending uploads depends on which rclone endpoints are reachable. Rendering a progress bar
that actually means "we have no idea" is worse than showing nothing. Carry the fidelity
tier with the data and let the UI degrade visibly.

**The rc endpoint is a UNIX socket, never a TCP bind**, and the D-Bus surface is a curated
set of methods, never a generic rc passthrough. rclone's rc API is equivalent to shell
access as the rclone user.

## Pull requests

Keep them reviewable. The first PR here was 5,600 lines and took seven review rounds to
settle, largely because it bundled unrelated work.

Commit messages should say **why**, not what — the diff already says what. If a change
corrects an earlier decision, say so and say what was wrong with it.

Reference the issue you are closing. CI must be green, and the branch protection requires
an approving review and a resolved conversation on every thread.

## Filing issues

For a bug, the most useful thing is `rclone version`, your `config.toml` with any secrets
removed, and what the service logged. `--log-level debug` is usually enough.
