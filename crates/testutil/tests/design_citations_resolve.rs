//! Every quoted design-document citation in the code names a heading that still exists.
//!
//! A pointer into a document that has been reorganised is worse than no pointer: it reads
//! as authoritative and sends the reader to a section that no longer says the thing. The
//! citations wrap across rustdoc lines, so the text is rejoined before matching.

use std::path::{Path, PathBuf};

/// The two forms a citation takes here. Both are followed immediately by the quoted
/// heading, which may be a fragment of it rather than the whole line.
const OPENERS: [&str; 2] = ["DESIGN.md, \"", "DESIGN.md under \""];

/// This guard, so an example written in one of its own comments is not taken for a claim
/// about the tree.
const EXEMPT: [&str; 1] = ["crates/testutil/tests/design_citations_resolve.rs"];

#[test]
fn every_cited_design_heading_exists() {
    let root = workspace_root();
    let headings = headings(&root.join("DESIGN.md"));
    let mut broken = Vec::new();

    for (file, title) in citations(&root) {
        if !resolves(&title, &headings) {
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            broken.push(format!("  {rel} cites \"{title}\""));
        }
    }

    assert!(
        broken.is_empty(),
        "these name a DESIGN.md heading that does not exist:\n{}\n\nheadings present:\n  {}",
        broken.join("\n"),
        headings.join("\n  ")
    );
}

/// One file per crate the walk must reach, so a clean result cannot come from a walk that
/// stopped early: `rust_sources` skips a directory it cannot list rather than failing.
const MUST_REACH: [&str; 5] = [
    "crates/core/src/rc.rs",
    "crates/service/src/supervisor.rs",
    "crates/tray/src/main.rs",
    "crates/gtk/src/main.rs",
    "crates/testutil/src/lib.rs",
];

/// The control. An empty `broken` list above means nothing unless the extractor still
/// finds citations that are really there, the matcher still rejects one that is not, and
/// the walk still reaches the files that would carry an offender.
#[test]
fn the_guard_still_detects_what_it_is_looking_for() {
    let root = workspace_root();
    let headings = headings(&root.join("DESIGN.md"));
    assert!(
        headings.len() > 5,
        "parsed {} headings out of DESIGN.md, so the parser is broken",
        headings.len()
    );

    let found = citations(&root);
    assert!(
        found
            .iter()
            .any(|(_, t)| t == "Delegated restart needs a pre-start hook to work at all"),
        "the extractor no longer finds a citation that is present in the tree; it found: {:?}",
        found.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );

    // Both forms against known input, the second wrapped across two rustdoc lines — the
    // case that makes rejoining necessary.
    let sample = concat!(
        "/// Something. See DESIGN.md, \"first title\".\n",
        "/// More of it, per DESIGN.md under \"second\n",
        "/// title\", and done.\n",
    );
    assert_eq!(
        cited_in(&comment_text(sample)),
        vec!["first title".to_string(), "second title".to_string()],
        "the extractor no longer reads the citation forms it claims to"
    );

    assert!(
        !resolves("a heading nobody has ever written", &headings),
        "the matcher accepts a heading that does not exist, so it would accept anything"
    );

    let walked = rust_sources(&root.join("crates"));
    for rel in MUST_REACH {
        assert!(
            walked.contains(&root.join(rel)),
            "the walk never reached {rel}, so a clean result proves nothing"
        );
    }
}

/// A citation resolves if some heading contains it: `"capability ladder"` is a fragment of
/// `## The capability ladder`, and both are compared without case, backticks or run-on
/// whitespace.
fn resolves(title: &str, headings: &[String]) -> bool {
    let want = normalise(title);
    !want.is_empty() && headings.iter().any(|h| normalise(h).contains(&want))
}

fn normalise(s: &str) -> String {
    s.to_lowercase()
        .replace(['`', '*'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn headings(design: &Path) -> Vec<String> {
    let body = std::fs::read_to_string(design).expect("DESIGN.md is missing");
    body.lines()
        .filter_map(|l| l.strip_prefix('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .collect()
}

/// Every `(file, heading)` pair cited from a comment under `crates/`.
fn citations(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for file in rust_sources(&root.join("crates")) {
        let rel = file.strip_prefix(root).unwrap_or(&file).to_path_buf();
        if EXEMPT.iter().any(|e| rel == Path::new(e)) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&file) else {
            continue;
        };
        for title in cited_in(&comment_text(&body)) {
            out.push((file.clone(), title));
        }
    }
    out
}

/// The comment lines of a source file, rejoined into one string so a citation that wraps
/// across two rustdoc lines is still one run of text.
fn comment_text(body: &str) -> String {
    body.lines()
        .map(str::trim_start)
        .filter_map(|l| l.strip_prefix("//"))
        .map(|l| l.trim_start_matches(['/', '!']).trim())
        .collect::<Vec<_>>()
        .join(" ")
}

fn cited_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for opener in OPENERS {
        let mut rest = text;
        while let Some(at) = rest.find(opener) {
            rest = &rest[at + opener.len()..];
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].trim().to_string());
                rest = &rest[end + 1..];
            }
        }
    }
    out
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
