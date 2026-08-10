//! Every scratch path in the workspace comes from `rvt_testutil::Scratch`.
//!
//! A test that builds its own path under the system temporary directory has to remember
//! to remove it, and cannot remove it at all if it panics first. This is the guard that
//! stops the habit coming back one call site at a time.

use std::path::{Path, PathBuf};

/// What a call site would have to write. `temp_dir` covers every spelling of the call —
/// qualified, via `use std::env`, or a bare import — and `TMPDIR` covers reading the
/// variable behind it directly.
///
/// A hardcoded `"/tmp/..."` is deliberately *not* here: two already exist and neither
/// creates anything (`config.rs` asserts on a path it never touches, `supervisor.rs` puts
/// one in a mountinfo fixture), so the pattern would need an allowlist to stay quiet, and
/// an allowlist is the thing that goes stale.
const NEEDLES: [&str; 2] = ["temp_dir", "TMPDIR"];

/// The helper, which is allowed to name it, and this guard, which has to.
const EXEMPT: [&str; 2] = [
    "crates/testutil/src/lib.rs",
    "crates/testutil/tests/no_stray_scratch.rs",
];

#[test]
fn nothing_outside_the_helper_builds_its_own_scratch_path() {
    let root = workspace_root();
    let mut offenders = Vec::new();

    for file in rust_sources(&root.join("crates")) {
        let rel = file.strip_prefix(&root).unwrap_or(&file).to_path_buf();
        if EXEMPT.iter().any(|e| rel == Path::new(e)) {
            continue;
        }
        let body = std::fs::read_to_string(&file).unwrap();
        for (n, line) in body.lines().enumerate() {
            if NEEDLES.iter().any(|needle| line.contains(needle)) {
                offenders.push(format!("{}:{}: {}", rel.display(), n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "use rvt_testutil::Scratch instead of naming the temporary directory:\n{}",
        offenders.join("\n")
    );
}

/// Files the walk must reach, one per crate, so a walk that quietly stopped early cannot
/// be mistaken for a clean workspace. `rust_sources` skips a directory it cannot list
/// rather than failing, which is what makes an unnamed count too weak to rely on.
const MUST_REACH: [&str; 5] = [
    "crates/core/src/rc.rs",
    "crates/service/src/supervisor.rs",
    "crates/tray/src/main.rs",
    "crates/gtk/src/main.rs",
    "crates/testutil/src/lib.rs",
];

/// The control for the search above. Both halves have to hold for an empty result to mean
/// anything: the pattern still matches text that is really there, and the walk still
/// reaches the files that would carry an offender.
#[test]
fn the_search_matches_the_files_it_exempts() {
    let root = workspace_root();
    for rel in EXEMPT {
        let path = root.join(rel);
        let body =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel} moved or gone: {e}"));
        assert!(
            NEEDLES.iter().any(|needle| body.contains(needle)),
            "{rel} no longer contains any pattern the guard searches for"
        );
    }

    let found = rust_sources(&root.join("crates"));
    for rel in MUST_REACH {
        let want = root.join(rel);
        assert!(
            found.contains(&want),
            "the walk never reached {rel}, so a clean result proves nothing"
        );
    }
}

fn workspace_root() -> PathBuf {
    // `<root>/crates/testutil` — this crate's manifest directory.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate is two levels below the workspace root")
        .to_path_buf()
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out
}
