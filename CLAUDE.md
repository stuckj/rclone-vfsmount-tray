# Repository conventions

Notes for agent sessions working in this repo. `CONTRIBUTING.md` covers the same ground
for humans; this adds the things that have actually gone wrong here.

## Layout

```
crates/core/      rvt-core — rc API models, rclone discovery, config, MountSupervisor.
                  Pure Rust. Everything else depends on this; it depends on nothing local.
crates/service/   rclone-vfsmount-trayd — owns mounts, polls, serves D-Bus. Pure Rust.
crates/tray/      rclone-vfsmount-tray — ksni SNI client over D-Bus. Pure Rust.
crates/gtk/       rclone-vfsmount-tray-gtk — GTK4 client. The only crate that links C.
```

Dependencies flow one way: the three binaries depend on `rvt-core`, never on each other.
The service and the clients communicate over D-Bus, not by linking.

`crates/gtk` is excluded from `default-members`, so a bare `cargo build` works without GTK
headers.

## Invariants

**Mounts belong to the service.** No client action — exiting, crashing, never starting —
unmounts anything, and neither does restarting the service. Enforced by
`crates/core/tests/external_impl.rs` and specified by the lifetime matrix in `DESIGN.md`.

**No system C libraries outside `crates/gtk`.** Tier-1 CI runs on a bare runner with no
`apt-get` step; that is the point. If a new dependency breaks it, that is the alarm
working.

**Never fake precision.** What can be said about pending uploads depends on which rclone
endpoints are reachable. Carry the fidelity tier with the data; degrade visibly rather
than showing a confident number you do not have.

**The rc socket is a UNIX socket, never TCP.** The D-Bus surface is a curated method set,
never an rc passthrough. rclone's rc API is shell-equivalent.

## Things that have gone wrong here, repeatedly

Read this before fixing anything.

**Fixing the reported instance instead of the class.** The single most common defect in
this repo's history — seven occurrences across two PRs, twice inside the very commit whose
message identified it as the problem. After any fix, search for the same mistake
elsewhere. Concretely: a validator that trimmed its input but stored the untrimmed value
was fixed for one field while the identical bug sat four lines below on the next field;
a false claim about rclone was corrected in the code and left standing in the user-facing
doc that repeated it.

**Tests that cannot fail.** A fixture suite existed to catch rclone wire-format drift and
could not detect a renamed field, because it round-tripped through the model's own codec
on both sides. Prove a guard works by breaking what it guards.

**Under-sized samples for flaky tests.** "0 failures in 25 runs" was reported as evidence
a race was fixed. At the real rate that sample had an ~86% chance of seeing nothing. Run
the compiled test binary directly, hundreds of times, and compute whether the sample could
have detected the effect.

**Claims about rclone that were never checked.** Several comments asserted rclone
behaviour that turned out to be wrong — a cache mode that does have a write-back queue
described as not having one, a config-name rule transcribed from an error message rather
than the regex that enforces it. Check `rclone` source or real output before writing a
comment about it.

**Documentation restated in many places.** The same rationale appeared in up to eight
locations; one copy went stale and produced a real bug. `DESIGN.md` is the single home for
reasoning. Code comments state the rule and point there.

## Conventions

Comments are restrained — roughly 10-20% of lines. Explain what would otherwise be
re-derived, never what the code already says.

`testdata/` is captured from a live rclone, never hand-written. Capture new fixtures; a
fake will agree with whatever assumption you already had.

Verified rclone behaviour is v1.75.0 (issue #9). The version floor of 1.60 is a sanity
check, not a compatibility claim — feature availability is detected with `rc/list`.

Commit messages explain why. When a change corrects an earlier decision, say what was
wrong with it.

## Checks

```sh
cargo fmt --all
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p rvt-core
```

All blocking in CI. Read clippy's output rather than its exit status — a failure has been
pushed here by checking the latter and not the former.
