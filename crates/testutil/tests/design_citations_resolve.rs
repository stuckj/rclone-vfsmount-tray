//! Every `DESIGN.md, "Heading"` citation in a Rust comment names a heading that exists.
//!
//! A pointer into a document that has been reorganised is worse than no pointer: it reads
//! as authoritative and sends the reader to a section that no longer says the thing.
//!
//! **One spelling is recognised, and that is the design.** Write `DESIGN.md, "Heading"` in
//! a comment — wrapping across lines if it is long — and the citation is checked. Name a
//! section any other way and this ignores it: unquoted, in a `/* */` block, trailing a line
//! of code, or from a `.md` file. Those are not gaps to close. Recognising every way
//! English can name a section costs a parser to maintain and buys what one line of
//! convention already gives, and an unrecognised form is only ever a citation left
//! unchecked — never a false report against a citation that is fine.

use std::path::{Path, PathBuf};

/// The only form recognised. Read the module comment before widening it.
const OPENER: &str = "DESIGN.md, \"";

/// This guard, whose own comments spell the form out and would otherwise be read as claims
/// about the tree.
const EXEMPT: &str = "crates/testutil/tests/design_citations_resolve.rs";

#[test]
fn every_cited_design_heading_exists() {
    let root = workspace_root();
    let headings = headings_of(&read(&root.join("DESIGN.md")));

    let broken: Vec<_> = citations(&root)
        .into_iter()
        .filter(|(_, title)| !resolves(title, &headings))
        .map(|(file, title)| {
            let rel = file.strip_prefix(&root).unwrap_or(&file).to_path_buf();
            format!("  {} cites \"{title}\"", rel.display())
        })
        .collect();

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

/// The control. An empty result above proves nothing unless the extractor still reads a
/// citation, the matcher still rejects a heading that is not there, a `#` inside a fence is
/// still not a heading, and the walk still reaches every crate.
#[test]
fn the_guard_still_detects_what_it_is_looking_for() {
    let root = workspace_root();
    let headings = headings_of(&read(&root.join("DESIGN.md")));

    assert!(
        headings.len() > 5,
        "parsed {} headings out of DESIGN.md, so the parser is broken",
        headings.len()
    );

    // Plain and wrapped, which is the only variation the one supported form has.
    let sample = concat!(
        "/// Something. See DESIGN.md, \"first\".\n",
        "/// More of it, per DESIGN.md, \"a wrapped\n",
        "/// heading\", and done.\n",
    );
    assert_eq!(
        cited_in(&comment_text(sample)),
        ["first", "a wrapped heading"].map(str::to_string),
        "the extractor no longer reads the form it claims to"
    );

    assert!(
        !citations(&root).is_empty(),
        "no citation found anywhere in the tree, so a clean result means nothing"
    );

    assert!(
        !resolves("a heading nobody has ever written", &headings),
        "the matcher accepts a heading that does not exist, so it would accept anything"
    );

    assert_eq!(
        headings_of("## Real\n\n```toml\n# Not a heading\n```\n\n### Also real\n"),
        ["Real", "Also real"].map(str::to_string),
        "a commented line in a code sample counts as a heading, so citations resolve to it"
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

/// Headings outside fenced blocks. A `#` inside a fence is a shell or TOML comment, and
/// counting one would let a citation resolve against a code sample instead of a section.
fn headings_of(body: &str) -> Vec<String> {
    let mut fenced = false;
    let mut out = Vec::new();
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced {
            if let Some(rest) = line.strip_prefix('#') {
                out.push(rest.trim_start_matches('#').trim().to_string());
            }
        }
    }
    out
}

/// Every `(file, heading)` pair cited from a comment under `crates/`.
fn citations(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for file in rust_sources(&root.join("crates")) {
        if file
            .strip_prefix(root)
            .is_ok_and(|r| r == Path::new(EXEMPT))
        {
            continue;
        }
        for title in cited_in(&comment_text(&read(&file))) {
            out.push((file.clone(), title));
        }
    }
    out
}

/// The comment lines of a source file, rejoined into one string so a citation that wraps
/// across two of them is a single run of text.
fn comment_text(body: &str) -> String {
    body.lines()
        .filter_map(|l| l.trim_start().strip_prefix("//"))
        .map(|l| l.trim_start_matches(['/', '!']).trim())
        .collect::<Vec<_>>()
        .join(" ")
}

fn cited_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPENER) {
        rest = &rest[at + OPENER.len()..];
        let Some(end) = rest.find('"') else { break };
        let title = rest[..end].trim();
        if !title.is_empty() {
            out.push(title.to_string());
        }
        rest = &rest[end + 1..];
    }
    out
}

/// Read, or fail naming the path. A file this cannot open is a hole in the guarantee
/// rather than a file to pass over.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
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
