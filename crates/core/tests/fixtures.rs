//! Parse the captured rclone responses in `testdata/`.
//!
//! These files are the literal bytes a live rclone v1.75.0 returned during the
//! investigation in issue #9. They exist so that a future rclone changing its wire
//! format is caught here, by a test naming the field that moved, rather than in the
//! field as a tray icon that silently stops showing progress.
//!
//! # Why a round trip is not enough
//!
//! The obvious check — deserialize, re-serialize, deserialize again, compare — is
//! nearly worthless on its own, and it is worth stating why so nobody reinstates it
//! as the only guard.
//!
//! It runs the model's *own* codec on both sides. If the model fails to read a
//! field, that field takes its default, serializes back as that default, and
//! re-parses to the same default. The comparison passes. So the round trip proves
//! the model agrees with itself; it proves nothing about whether the fixture was
//! read. Renaming a modelled field to something rclone never sends is invisible to
//! it — which is exactly how `RcCommand` shipped modelling a nonexistent
//! `AuthRequired` field with a green suite.
//!
//! `serde_ignored` closes half of that: it reports the keys the model *dropped from
//! the input*, so a model field renamed to something rclone never sends shows up as
//! a newly-dropped key. But it says nothing about a key vanishing from the input —
//! every field is `#[serde(default)]`, so a removed key just silently defaults.
//!
//! So each fixture is pinned by an explicit [`FixtureSpec`] and checked four ways:
//!
//! 1. it parses;
//! 2. the **full set of keys present in the JSON** equals `keys` — catching anything
//!    added to or removed from the wire format;
//! 3. the set the model **dropped** equals `ignored` — catching a model that stops
//!    reading a field it used to read;
//! 4. it survives a round trip (kept for the self-consistency it does prove).
//!
//! Together those make both directions fail loudly and name the path. The lists are
//! deliberately verbose: they are a snapshot of the wire contract, and having to
//! update one is the point — it is how you find out what changed when someone
//! recaptures the fixtures against a newer rclone.

use rvt_core::models::*;
use serde_json::Value;
use std::collections::BTreeSet;

/// What we expect a fixture to contain, and how much of it we model.
struct FixtureSpec<'a> {
    /// Every dotted key path present in the JSON, array indices collapsed to `[]`.
    keys: &'a [&'a str],
    /// The subset of `keys` the model deliberately does not read.
    ignored: &'a [&'a str],
    /// Subtrees kept as raw JSON, whose children are therefore not enumerated.
    opaque: &'a [&'a str],
}

fn testdata(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/");
    let full = format!("{path}{name}");
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {full}: {e}"))
}

/// Collect every dotted key path in a JSON document, collapsing array indices to
/// `[]` and not descending into `opaque` subtrees.
fn collect_keys(v: &Value, prefix: &str, opaque: &[&str], out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, vv) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.insert(path.clone());
                if !opaque.contains(&path.as_str()) {
                    collect_keys(vv, &path, opaque, out);
                }
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}.[]");
            for item in items {
                collect_keys(item, &path, opaque, out);
            }
        }
        _ => {}
    }
}

fn diff_report(label: &str, actual: &BTreeSet<String>, expected: &BTreeSet<String>) -> String {
    let added: Vec<_> = actual.difference(expected).collect();
    let removed: Vec<_> = expected.difference(actual).collect();
    format!("  {label}: unexpected {added:?}, missing {removed:?}")
}

/// Parse a fixture, pinning both the wire contract and how much of it we model.
fn parse_fixture<T>(name: &str, spec: &FixtureSpec<'_>) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
{
    let raw = testdata(name);

    // (2) Everything the JSON contains. Catches additions AND removals — the latter
    // is invisible to serde_ignored, because a missing key simply defaults.
    let doc: Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name} is not JSON: {e}"));
    let mut present = BTreeSet::new();
    collect_keys(&doc, "", spec.opaque, &mut present);
    let expected_keys: BTreeSet<String> = spec.keys.iter().map(|s| s.to_string()).collect();
    assert!(
        present == expected_keys,
        "{name}: the wire format changed.\n{}",
        diff_report("keys", &present, &expected_keys)
    );

    // (3) Everything the model declined to read.
    //
    // Note serde_ignored inserts a `?` segment for Option-wrapped values, so nested
    // paths read like `diskCache.?.someKey`. The panic prints the real path, so an
    // out-of-date list is self-correcting.
    let mut ignored: BTreeSet<String> = BTreeSet::new();
    let de = &mut serde_json::Deserializer::from_str(&raw);
    let parsed: T = serde_ignored::deserialize(de, |path| {
        ignored.insert(normalise_indices(&path.to_string()));
    })
    .unwrap_or_else(|e| panic!("parsing {name}: {e}"));

    let expected_ignored: BTreeSet<String> = spec.ignored.iter().map(|s| s.to_string()).collect();
    assert!(
        ignored == expected_ignored,
        "{name}: the set of unmodelled keys changed.\n{}",
        diff_report("ignored", &ignored, &expected_ignored)
    );

    // (4) Self-consistency. Weak on its own — see the module docs — but it does catch
    // a Serialize/Deserialize pair that disagree with each other.
    let reser = serde_json::to_string(&parsed).expect("serialize");
    let again: T =
        serde_json::from_str(&reser).unwrap_or_else(|e| panic!("re-parsing {name}: {e}"));
    assert_eq!(parsed, again, "{name} did not survive a round trip");

    parsed
}

/// Collapse array indices in a `serde_ignored` path so the expected sets do not
/// depend on how many elements a fixture happens to contain:
/// `commands.0.Help` and `commands.57.Help` both become `commands.[].Help`.
fn normalise_indices(path: &str) -> String {
    path.split('.')
        .map(|seg| {
            if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
                "[]"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// `core/stats` group totals. The `ignored` entries are counters for operations this
/// tool never performs — listed rather than dropped silently, so that their
/// disappearance is still noticed.
const CORE_STATS_TOTALS: &[&str] = &[
    "bytes",
    "checks",
    "deletedDirs",
    "deletes",
    "elapsedTime",
    "errors",
    "eta",
    "fatalError",
    "listed",
    "renames",
    "retryError",
    "serverSideCopies",
    "serverSideCopyBytes",
    "serverSideMoveBytes",
    "serverSideMoves",
    "speed",
    "totalBytes",
    "totalChecks",
    "totalTransfers",
    "transferTime",
    "transfers",
];

const CORE_STATS_TRANSFER: &[&str] = &[
    "transferring",
    "transferring.[].bytes",
    "transferring.[].dstFs",
    "transferring.[].eta",
    "transferring.[].group",
    "transferring.[].name",
    "transferring.[].percentage",
    "transferring.[].size",
    "transferring.[].speed",
    "transferring.[].speedAvg",
    "transferring.[].srcFs",
];

const CORE_STATS_IGNORED: &[&str] = &[
    "deletedDirs",
    "deletes",
    "listed",
    "renames",
    "serverSideCopies",
    "serverSideCopyBytes",
    "serverSideMoveBytes",
    "serverSideMoves",
];

/// Concatenate at test time — `const` slice concatenation is not available.
fn core_stats_keys(with_transfers: bool) -> Vec<&'static str> {
    let mut v = CORE_STATS_TOTALS.to_vec();
    if with_transfers {
        v.extend_from_slice(CORE_STATS_TRANSFER);
    }
    v
}

fn core_stats(name: &str, with_transfers: bool) -> CoreStats {
    let keys = core_stats_keys(with_transfers);
    parse_fixture(
        name,
        &FixtureSpec {
            keys: &keys,
            ignored: CORE_STATS_IGNORED,
            opaque: &[],
        },
    )
}

/// `opt` is the full VFS option block — 30+ fields that change between rclone
/// releases and that nothing here needs typed. It is kept as raw JSON, so its
/// children are deliberately not pinned.
const VFS_STATS_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &[
        "diskCache",
        "diskCache.bytesUsed",
        "diskCache.erroredFiles",
        "diskCache.files",
        "diskCache.hashType",
        "diskCache.outOfSpace",
        "diskCache.path",
        "diskCache.pathMeta",
        "diskCache.uploadsInProgress",
        "diskCache.uploadsQueued",
        "fs",
        "inUse",
        "metadataCache",
        "metadataCache.dirs",
        "metadataCache.files",
        "opt",
    ],
    ignored: &[],
    opaque: &["opt"],
};

const VFS_QUEUE_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &[
        "queue",
        "queue.[].delay",
        "queue.[].expiry",
        "queue.[].id",
        "queue.[].name",
        "queue.[].size",
        "queue.[].tries",
        "queue.[].uploading",
    ],
    ignored: &[],
    opaque: &[],
};

/// `rc/list` carries per-command help text and request/response plumbing flags that
/// are of no use to a tray applet.
const RC_LIST_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &[
        "commands",
        "commands.[].Help",
        "commands.[].NeedsRequest",
        "commands.[].NeedsResponse",
        "commands.[].NoAuth",
        "commands.[].Path",
        "commands.[].Title",
    ],
    ignored: &[
        "commands.[].Help",
        "commands.[].NeedsRequest",
        "commands.[].NeedsResponse",
    ],
    opaque: &[],
};

/// Build details of the rclone binary; we gate on `decomposed`, not on these.
const CORE_VERSION_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &[
        "arch",
        "decomposed",
        "goTags",
        "goVersion",
        "isBeta",
        "isGit",
        "linking",
        "os",
        "osArch",
        "osKernel",
        "osVersion",
        "version",
    ],
    ignored: &["goTags", "linking", "osArch", "osKernel", "osVersion"],
    opaque: &[],
};

const VFS_LIST_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &["vfses"],
    ignored: &[],
    opaque: &[],
};

const VFSMETA_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &[
        "ATime",
        "Dirty",
        "Fingerprint",
        "ModTime",
        "Rs",
        "Rs.[].Pos",
        "Rs.[].Size",
        "Size",
    ],
    ignored: &[],
    opaque: &[],
};

fn vfs_stats(name: &str) -> VfsStats {
    parse_fixture(name, &VFS_STATS_SPEC)
}
fn vfs_queue(name: &str) -> VfsQueue {
    parse_fixture(name, &VFS_QUEUE_SPEC)
}

#[test]
fn core_stats_mid_upload_has_real_progress() {
    let s: CoreStats = core_stats("core-stats-vfs-upload-midflight.json", true);
    let t = &s.transfers_slice()[0];

    assert_eq!(t.name, "big.bin");
    assert_eq!(t.size, 134_217_728);
    assert_eq!(t.known_size(), Some(134_217_728));
    assert!(
        t.bytes > 0 && t.bytes < t.size,
        "mid-flight: {} of {}",
        t.bytes,
        t.size
    );
    assert_eq!(t.percentage, Some(50));

    // The whole point of issue #9: these are populated for a VFS write-back upload,
    // which was the open question the project's feasibility hinged on.
    assert!(t.speed > 0.0);
    assert!(t.speed_avg > 0.0);
    assert_eq!(t.eta, Some(14.0));

    // --bwlimit was 4M; the reported rate should be in that neighbourhood rather
    // than a placeholder.
    assert!(
        (3.5e6..5.0e6).contains(&t.speed),
        "speed {} should reflect the 4M bwlimit",
        t.speed
    );
}

#[test]
fn core_stats_transferring_absent_when_idle() {
    let s: CoreStats = core_stats("core-stats-idle-no-transferring.json", false);
    assert!(
        !s.reported_transferring(),
        "the idle fixture must have NO transferring key — that is the behaviour \
         this model exists to represent"
    );
    assert!(s.transfers_slice().is_empty());
}

#[test]
fn vfs_and_job_transfers_are_separable() {
    let s: CoreStats = core_stats("core-stats-mixed-vfs-and-job.json", true);

    // rclone really does mix a VFS write-back upload and an unrelated rc job into
    // one array, so filtering is not optional.
    assert_eq!(s.transfers_slice().len(), 2, "fixture should hold both");

    let vfs: Vec<_> = s.vfs_writeback_transfers().map(|t| &*t.name).collect();
    assert_eq!(
        vfs,
        ["vfsfile.bin"],
        "only the write-back upload belongs to a mount"
    );

    let job = s
        .transfers_slice()
        .iter()
        .find(|t| !t.is_vfs_writeback())
        .expect("the job transfer");
    assert!(
        job.group.as_deref().is_some_and(|g| g.starts_with("job/")),
        "explicit jobs are grouped job/<n>, got {:?}",
        job.group
    );
}

#[test]
fn transfer_attributes_to_its_vfs_cache() {
    let stats: CoreStats = core_stats("core-stats-vfs-upload-midflight.json", true);
    let vfs: VfsStats = vfs_stats("vfs-stats-upload-in-progress.json");

    let cache_path = &vfs.disk_cache.as_ref().expect("diskCache").path;
    let t = &stats.transfers_slice()[0];

    // This is the mechanism that replaces path-prefix guessing: srcFs embeds the
    // VFS cache directory, so a transfer can be tied to the mount that owns it.
    assert!(
        t.belongs_to_cache(cache_path),
        "srcFs {:?} should contain diskCache.path {cache_path:?}",
        t.src_fs
    );
}

#[test]
fn queue_names_join_to_transfer_names() {
    let stats: CoreStats = core_stats("core-stats-vfs-upload-midflight.json", true);
    let queue: VfsQueue = vfs_queue("vfs-queue-uploading.json");

    let tname = &stats.transfers_slice()[0].name;
    let qname = &queue.queue[0].name;
    assert_eq!(
        tname, qname,
        "the join key between core/stats and vfs/queue is an exact name match"
    );
}

#[test]
fn queue_queued_versus_uploading() {
    let queued: VfsQueue = vfs_queue("vfs-queue-queued-not-uploading.json");
    let item = &queued.queue[0];
    assert!(!item.uploading, "still waiting out --vfs-write-back");
    assert!(item.expiry > 0.0, "expiry counts down to zero");
    assert_eq!(queued.uploading().count(), 0);
    assert_eq!(queued.pending_bytes(), item.size as u64);

    let live: VfsQueue = vfs_queue("vfs-queue-uploading.json");
    let item = &live.queue[0];
    assert!(item.uploading);
    assert!(
        item.expiry < 0.0,
        "expiry goes NEGATIVE once due — {} — which is why the field is signed",
        item.expiry
    );
    assert_eq!(live.uploading().count(), 1);
}

#[test]
fn vfs_stats_hands_over_cache_paths() {
    let s: VfsStats = vfs_stats("vfs-stats-upload-in-progress.json");
    let dc = s.disk_cache.expect("diskCache present when caching is on");

    // These two make on-disk cache scanning possible without guessing paths.
    assert!(dc.path.contains("/vfs/"), "{}", dc.path);
    assert!(dc.path_meta.contains("/vfsMeta/"), "{}", dc.path_meta);

    assert_eq!(dc.uploads_in_progress, 1);
    assert_eq!(dc.uploads_queued, 0);

    // Measured as 0 with 128 MiB sitting in the cache. Asserted so that anyone
    // tempted to show it as "pending bytes" trips over this test first.
    assert_eq!(
        dc.bytes_used, 0,
        "bytesUsed does not track pending or cached bytes — do not surface it"
    );
}

#[test]
fn vfs_stats_idle_still_reports_queue_counters() {
    let s: VfsStats = vfs_stats("vfs-stats-idle.json");
    let dc = s.disk_cache.expect("diskCache");
    assert_eq!(dc.uploads_in_progress, 0);
    assert!(!dc.out_of_space);
    assert_eq!(dc.errored_files, 0);
}

#[test]
fn vfsmeta_dirty_item_is_the_offline_signal() {
    let m: VfsMetaItem = parse_fixture("vfsmeta-item-dirty.json", &VFSMETA_SPEC);
    assert!(m.dirty, "a pending upload is Dirty on disk");
    assert_eq!(m.size, 100_663_296);
    assert_eq!(m.ranges.as_ref().map(Vec::len), Some(1));
    assert!(m.mod_time.ends_with('Z'), "RFC 3339: {}", m.mod_time);
    // Empty on the local backend. Other backends are checked in issue #10.
    assert_eq!(m.fingerprint, "");
}

#[test]
fn rc_list_supports_feature_detection() {
    let l: RcList = parse_fixture("rc-list-v1.75.0.json", &RC_LIST_SPEC);
    assert!(l.commands.len() > 50, "got {}", l.commands.len());

    // Every endpoint the capability ladder depends on, confirmed present in v1.75.0.
    for cmd in [
        "core/stats",
        "core/version",
        "vfs/queue",
        "vfs/queue-set-expiry",
        "vfs/stats",
        "vfs/list",
        "config/listremotes",
        "operations/list",
        "mount/listmounts",
    ] {
        assert!(l.has(cmd), "rclone v1.75.0 should register {cmd}");
    }
    assert!(!l.has("vfs/definitely-not-a-real-command"));
}

#[test]
fn core_version_decomposes() {
    let v: CoreVersion = parse_fixture("core-version.json", &CORE_VERSION_SPEC);
    assert_eq!(v.version, "v1.75.0");
    assert_eq!(v.decomposed, vec![1, 75, 0]);
    assert!(!v.is_beta);
}

#[test]
fn vfs_list_names_are_canonical_not_configured() {
    let l: VfsList = parse_fixture("vfs-list.json", &VFS_LIST_SPEC);
    assert_eq!(l.vfses.len(), 1);
    // The fixture was captured from an `alias` remote configured as `dst:`, yet
    // rclone reports the resolved target path. Anything matching on this string must
    // not assume it equals what the user configured.
    //
    // Asserted as the exact value: `!starts_with("dst:")` would pass for any string
    // at all, which is no assertion.
    assert_eq!(
        l.vfses[0], "/home/claude/.claude/jobs/f70669bc/tmp/spike9/remote-store",
        "rclone reports the resolved path, not the configured remote name"
    );
}
