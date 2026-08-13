//! Every quoted design-document citation in the code names a heading that still exists.
//!
//! A pointer into a document that has been reorganised is worse than no pointer: it reads
//! as authoritative and sends the reader to a section that no longer says the thing. The
//! citations wrap across rustdoc lines, so the text is rejoined before matching.
//!
//! Three kinds of citation are out of reach and would pass unchecked: one in a `/* */`
//! block comment; one in a `.md` file, since only `crates/**/*.rs` is walked; and one that
//! names a section without quoting it, which there is no way to tell from prose. None
//! exists today — quote the section name and this guard covers it.

use std::path::{Path, PathBuf};

/// How far from a `DESIGN.md` mention a quoted heading may sit and still be a citation of
/// it. Wide enough for every spelling in the tree and the obvious neighbours — `DESIGN.md,
/// "…"`, `DESIGN.md under "…"`, `DESIGN.md's "…"`, `the "…" section of DESIGN.md` — and
/// short enough that a quoted term in the next sentence is out of reach. Anything with a
/// sentence boundary between the two is rejected regardless of distance.
const WINDOW: usize = 48;

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
        "the extractor no longer finds a citation that is present in the tree — update this \
         literal if that section was deliberately renamed; it found: {:?}",
        found.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );

    // Every spelling against known input: the two in the tree, the two obvious neighbours,
    // one wrapped across rustdoc lines, and one trailing a line of code.
    let sample = concat!(
        "/// Something. See DESIGN.md, \"first\".\n",
        "/// More of it, per DESIGN.md under \"a wrapped\n",
        "/// heading\", and done.\n",
        "/// Then DESIGN.md's \"third\".\n",
        "/// Then the \"fourth\" section of DESIGN.md.\n",
        // Longer than WINDOW: real headings usually are, and only the closing quote is
        // near the mention.
        "/// And the \"a heading far longer than the window is wide\" section of DESIGN.md.\n",
        "const N: usize = 1; // see DESIGN.md, \"fifth\"\n",
        "const U: &str = \"unix://sock\"; // and DESIGN.md, \"sixth\"\n",
    );
    assert_eq!(
        cited_in(&comment_text(sample)),
        [
            "first",
            "a wrapped heading",
            "third",
            "fourth",
            "a heading far longer than the window is wide",
            "fifth",
            "sixth"
        ]
        .map(str::to_string),
        "the extractor no longer reads every citation spelling it claims to"
    );

    // A quote the mention does not reach, and one a sentence boundary cuts off.
    let noise = concat!(
        "/// See DESIGN.md. The \"name\" field is separate.\n",
        "/// A \"quoted term\" with no mention anywhere near it at all, padded out here.\n",
    );
    assert!(
        cited_in(&comment_text(noise)).is_empty(),
        "the extractor reads ordinary quoted prose as a citation: {:?}",
        cited_in(&comment_text(noise))
    );

    // An unbalanced quote earlier in the file must not hide a citation after it, which is
    // what pairing quotes across the whole text instead of anchoring on the mention did.
    let odd = concat!(
        "/// The kernel says \"Device or resource busy.\n",
        "/// See DESIGN.md, \"a heading that does not exist\".\n",
    );
    assert_eq!(
        cited_in(&comment_text(odd)),
        ["a heading that does not exist"].map(str::to_string),
        "a stray quote upstream hides the citation below it"
    );

    assert!(
        !resolves("a heading nobody has ever written", &headings),
        "the matcher accepts a heading that does not exist, so it would accept anything"
    );

    // A `#` line inside a fence is a comment in a sample, not a section to cite.
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

fn headings(design: &Path) -> Vec<String> {
    headings_of(&std::fs::read_to_string(design).expect("DESIGN.md is missing"))
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

/// The comment text of a source file, rejoined into one string so a citation that wraps
/// across two rustdoc lines is still one run of text.
fn comment_text(body: &str) -> String {
    body.lines()
        .filter_map(|line| Some(&line[comment_start(line)? + 2..]))
        .map(|l| l.trim_start_matches(['/', '!']).trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where a line's comment starts. The first `//` is not always it — `"unix://…"` occurs in
/// this tree — so string literals are skipped, honouring backslash escapes. A `'"'` char
/// literal would fool this and hide a comment on that line: a citation missed, never one
/// invented.
fn comment_start(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'/' if !in_string && b.get(i + 1) == Some(&b'/') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

const MENTION: &str = "DESIGN.md";

/// The longest a heading may be. A quote that runs past this is unbalanced prose, not a
/// section name.
const LONGEST: usize = 120;

/// Every heading cited by a `DESIGN.md` mention, looking either side of it.
///
/// Anchored on the mention rather than on quotes, so a stray `"` elsewhere in the file
/// cannot shift the pairing and hide a citation further down.
fn cited_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(MENTION) {
        let at = from + rel;
        let end = at + MENTION.len();
        if let Some(title) = quoted_after(text, end).or_else(|| quoted_before(text, at)) {
            out.push(title);
        }
        from = end;
    }
    out
}

/// `DESIGN.md, "…"` — the quote follows the mention.
fn quoted_after(text: &str, from: usize) -> Option<String> {
    let window = &text[from..ceil(text, (from + WINDOW).min(text.len()))];
    let open = window.find('"')?;
    joined(&window[..open]).then_some(())?;
    let rest = &text[from + open + 1..];
    heading(&rest[..rest.find('"')?])
}

/// `the "…" section of DESIGN.md` — the quote precedes the mention.
///
/// Only the *closing* quote has to be within `WINDOW`; the opening one is sought back as
/// far as `LONGEST`, since it is the heading's own length that separates them and most
/// real headings are longer than the window.
fn quoted_before(text: &str, to: usize) -> Option<String> {
    let near = floor(text, to.saturating_sub(WINDOW));
    let close = near + text[near..to].rfind('"')?;
    joined(&text[close + 1..to]).then_some(())?;
    let far = floor(text, close.saturating_sub(LONGEST + 1));
    let open = far + text[far..close].rfind('"')?;
    heading(&text[open + 1..close])
}

fn heading(run: &str) -> Option<String> {
    let t = run.trim();
    (!t.is_empty() && t.len() <= LONGEST).then(|| t.to_string())
}

/// Whether a gap between a quote and a `DESIGN.md` mention still joins them. A sentence
/// ends the association: `See DESIGN.md. The "name" field …` cites nothing.
fn joined(gap: &str) -> bool {
    !gap.contains(". ") && !gap.trim_end().ends_with('.')
}

fn floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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
