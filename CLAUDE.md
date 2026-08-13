# Repository conventions

Notes for agent sessions working in this repo. `CONTRIBUTING.md` covers the same ground
for humans; this adds the things that have actually gone wrong here.

## Layout

```
crates/core/      rvt-core — rc API models, rclone discovery, config, MountSupervisor.
                  Pure Rust. Everything else depends on this; it depends on nothing local.
crates/service/   rclone-vfsmount-trayd — owns mounts, polls, serves D-Bus. Pure Rust.
crates/tray/      rclone-vfsmount-tray — ksni SNI client over D-Bus. Pure Rust.
crates/gtk/       rclone-vfsmount-tray-gtk — GTK4 client. The only crate that will link
                  C, once it gains `gtk4`; today it has none and nothing here links one.
crates/testutil/  rvt-testutil — scratch directories for tests. A dev-dependency of the
                  others, never linked into a binary, and depends on nothing itself.
```

Dependencies flow one way: the three binaries depend on `rvt-core`, never on each other.
The service and the clients communicate over D-Bus, not by linking.

`crates/gtk` is excluded from `default-members`, so a bare `cargo build` works without GTK
headers.

## What goes in which document

**`README.md`** — for a user, who is probably not an engineer. What the project does, how
to install it, how to use it. It links to the others rather than repeating them.

**`CONTRIBUTING.md`** — how to work on the project: checkout, the checks, the toolchain
specifics that trip people up, how to submit. No design content.

**`DESIGN.md`** — directional only: how the application *should* work. Components,
boundaries, who owns what across them, and the decisions that constrain future work,
including ones not built yet. It changes when that direction changes — a component added
or removed, a boundary moved, a responsibility reassigned — and not otherwise. A thousand
code changes can leave it untouched.

**The code** — the authority on how the application *actually* works.

Detail belongs one layer below wherever it is tempting to put it. A measurement goes in a
test that fails when it stops being true, and in the dated PR or issue — not in
`DESIGN.md`, which cannot fail and will outlive the measurement. A mechanism goes in the
code — not in `DESIGN.md`, which will drift from it. `DESIGN.md` says a thing can fail and
gives an example or two *as examples*; it does not catalogue the ways.

The corollary for both prose and comments: do not describe the same mechanism twice. See
"Documentation restated in many places" below for what that has already cost here.

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
locations; one copy went stale and produced a real bug. State a rule once, in the layer
that owns it — see "What goes in which document" above — and do not restate it elsewhere.
A cross-reference is only worth adding if the thing it points at actually says more than
the comment already does; a pointer to a section that has since moved is its own defect.

**A search that found nothing, believed.** Three rules for checking a claim about this
codebase, each learned from a wrong answer:

1. When independent surfaces appear to fail identically, suspect the checker before
   believing you found several separate faults.
2. Sanity-check a pattern against a known-present control before trusting a negative. A
   suspiciously clean "nothing found" is usually a bad pattern.
3. Parse the source of truth, not prose about it. Grepping documentation produces false
   gaps; reading the dispatch table, the manifest or the real output finds true ones.

## Conventions

Comments describe the code, not the project's history. Explain what a reader would
otherwise re-derive — a non-obvious ordering constraint, an external behaviour depended
on, an alternative that looks right and is not.

Two things never belong in a comment: general best-practice advice, and development
history. What broke while building it, which review caught it, what an earlier design
assumed, how often a test flaked — all of that goes in the commit message, the PR, or
the issue, where it is permanent and searchable and out of the reader's way. Prefer
naming the constraint over narrating the discovery.

**10-20% of lines is normal, as a guide rather than a limit.** Above that, re-read the
comments against the rules above and check each one is still about the code: the usual
cause of a high ratio is narrating consequences at length. Keep the constraint, drop the
essay around it.

Two kinds of file legitimately sit higher. One documents an external format or behaviour
nothing in the code implies — `mountinfo.rs` and the kernel's mountinfo layout. The other
is a public API surface, where rustdoc on every item is expected and the bodies are often
one-liners: `capabilities.rs` is 84 lines of code to 56 of rustdoc because eight public
predicates each have to say what distinction they encode. In both cases the ratio is high
because the code is dense, not because the prose is.

So the number is a prompt to look, never a reason to cut a comment that is doing real work
or to pad toward a figure.

Anything a test needs on disk comes from `rvt_testutil::Scratch`, never from
`std::env::temp_dir()`. It removes itself on drop, including while a panic unwinds.
`crates/testutil/tests/no_stray_scratch.rs` rejects `temp_dir` and `TMPDIR` in any
crate — not a hardcoded `/tmp`, which two tests legitimately name without creating
anything. See `CONTRIBUTING.md`.

`testdata/` is captured from a live rclone, never hand-written. Capture new fixtures; a
fake will agree with whatever assumption you already had.

Verified rclone behaviour is v1.75.0 (issue #9). The version floor of 1.61 is the oldest
rclone on which `--rc-addr unix://` works at all; feature availability above that is
detected with `rc/list`, not inferred from a version.

Commit messages explain why. When a change corrects an earlier decision, say what was
wrong with it.

## Tooling gotchas

**Put git worktrees beside the repo, never inside it.** The convention is a sibling
directory named after the topic — `../test-scratch-cleanup`, `../unmount-ordering`. A
worktree nested under the checkout is a second clone living inside the repo, which is
confusing to browse and to search. Claude Code's `EnterWorktree` defaults to
`.claude/worktrees/<name>` *inside* the repo, so do not create one with it: run
`git worktree add ../<topic> -b <branch>` yourself, then pass that path to `EnterWorktree`.

Remove a worktree once its PR merges, and decide that on content rather than commits: this
repo squash-merges, so a merged branch is never an ancestor of `main` and
`git merge-base --is-ancestor` will say it is unmerged. An empty `git diff main..<branch>`
is the test.

**`gh issue view`, `gh pr view` and `gh pr edit` fail against this repo.** All three hit
the deprecated projects-classic GraphQL field, which `.github/workflows/project-add.yml`
puts issues and PRs on. Each exits 1 printing only the deprecation notice, and `gh pr edit`
**discards the edit** — measured on #78, where the title was unchanged afterwards. Use
REST:

```sh
gh api repos/stuckj/rclone-vfsmount-tray/issues/<n>
gh api -X PATCH repos/stuckj/rclone-vfsmount-tray/pulls/<n> -f title=... --input body.json
```

`gh pr view <n> --json <fields>` *is* fine — that query omits `projectCards` — as are
`gh pr create`, `gh pr list` and `gh api`.

**Never write a bracketed skip-ci marker in a commit message, not even to describe one.**
GitHub scans the whole message, body included, for `[skip ci]`, `[ci skip]`, `[no ci]`,
`[skip actions]` and `[actions skip]`, so a commit that merely *explains* them disables CI
on itself. The runs are never created, so nothing reports as skipped and the PR still looks
green from whatever is not gated. It survives the squash, since GitHub seeds the squash
message from the commits. Write the regex form `\[(skip ci|...)\]`, or "skip-ci marker" in
prose. After pushing, confirm the workflows actually appeared:
`gh api "repos/stuckj/rclone-vfsmount-tray/actions/runs?branch=<branch>"` — a green PR is
not evidence they ran.

**Only the repo owner can request a Copilot review.** A bot account's request returns
success but creates no timeline event and no review.

## Checks

```sh
cargo fmt --all
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --workspace
```

All blocking in CI. Read clippy's output rather than its exit status — a failure has been
pushed here by checking the latter and not the former.
