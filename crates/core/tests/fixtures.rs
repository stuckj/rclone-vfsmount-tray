//! Parse the captured rclone v1.75.0 responses in `testdata/`, so a wire-format change
//! fails a test naming the field rather than silently breaking the tray.
//!
//! A round trip alone proves nothing — it runs the model's own codec both ways, so a
//! field the model fails to read defaults and compares equal. Each fixture is therefore
//! pinned by a [`FixtureSpec`]: every expected key must be present in every array
//! element, nothing outside `keys ∪ optional` may appear, and the keys the model
//! *dropped* must match `ignored`. Swaps between same-typed fields need value
//! assertions; the key set cannot see them.

use rvt_core::models::*;
use serde_json::Value;
use std::collections::BTreeSet;

/// What we expect a fixture to contain, and how much of it we model.
struct FixtureSpec<'a> {
    /// Key paths that must be present, array indices collapsed to `[]`.
    keys: &'a [&'a str],
    /// Key paths rclone emits *conditionally* — legal to be present or absent.
    ///
    /// Without this the guard would cry wolf on its own intended workflow:
    /// `core/stats` omits `lastError` unless `errors > 0` and `checking` unless a
    /// check is running, so recapturing a fixture from a busy rclone would fail as
    /// "the wire format changed" for keys that were always allowed. A guard that
    /// fires on correct input is a guard people learn to ignore.
    optional: &'a [&'a str],
    /// The subset of the above the model deliberately does not read.
    ignored: &'a [&'a str],
    /// Subtrees kept as raw JSON, whose children are therefore not enumerated.
    opaque: &'a [&'a str],
}

fn testdata(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/");
    let full = format!("{path}{name}");
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {full}: {e}"))
}

/// Collect dotted key paths from a JSON document, collapsing array indices to `[]`
/// and not descending into `opaque` subtrees.
///
/// `INTERSECT` controls how array elements combine. The union answers "what keys
/// exist anywhere", which is what detects an *added* key. The intersection answers
/// "what keys exist in *every* element", which is what detects a key removed from
/// one element of many — a union would hide that behind its 100 well-formed
/// siblings.
fn collect_keys<const INTERSECT: bool>(
    v: &Value,
    prefix: &str,
    opaque: &[&str],
    out: &mut BTreeSet<String>,
) {
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
                    collect_keys::<INTERSECT>(vv, &path, opaque, out);
                }
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}.[]");
            let mut per_element: Vec<BTreeSet<String>> = Vec::new();
            for item in items {
                let mut s = BTreeSet::new();
                collect_keys::<INTERSECT>(item, &path, opaque, &mut s);
                per_element.push(s);
            }
            let combined = if INTERSECT {
                per_element
                    .into_iter()
                    .reduce(|a, b| a.intersection(&b).cloned().collect())
                    .unwrap_or_default()
            } else {
                per_element.into_iter().flatten().collect()
            };
            out.extend(combined);
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
    let mut anywhere = BTreeSet::new();
    collect_keys::<false>(&doc, "", spec.opaque, &mut anywhere);
    let mut everywhere = BTreeSet::new();
    collect_keys::<true>(&doc, "", spec.opaque, &mut everywhere);

    let required: BTreeSet<String> = spec.keys.iter().map(|s| s.to_string()).collect();
    let optional: BTreeSet<String> = spec.optional.iter().map(|s| s.to_string()).collect();

    // Required keys must appear in EVERY array element, not merely somewhere.
    let missing: Vec<_> = required.difference(&everywhere).collect();
    let unexpected: Vec<_> = anywhere
        .difference(&required)
        .filter(|k| !optional.contains(*k))
        .collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{name}: the wire format changed.\n  \
         missing (rclone removed or renamed it): {missing:?}\n  \
         unexpected (rclone added it, or it belongs in `optional`): {unexpected:?}"
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

/// Keys `core/stats` emits only under some conditions: `lastError` when errors have
/// occurred, `checking` while a check is running. Legal to be absent — which every
/// fixture here is, having been captured from a healthy idle-or-uploading rclone.
// NB `checking` is a string array, and `collect_keys` only records paths for object
// keys — a `Vec<String>` produces no `checking.[]` entry, so listing one would be dead
// weight implying the element shape is pinned when it is not.
const CORE_STATS_OPTIONAL: &[&str] = &["lastError", "checking"];

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
            optional: CORE_STATS_OPTIONAL,
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
    optional: &[],
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
    optional: &[],
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
    optional: &[],
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
    optional: &[],
    ignored: &["goTags", "linking", "osArch", "osKernel", "osVersion"],
    opaque: &[],
};

const VFS_LIST_SPEC: FixtureSpec<'static> = FixtureSpec {
    keys: &["vfses"],
    optional: &[],
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
    optional: &[],
    ignored: &[],
    opaque: &[],
};

fn vfs_stats(name: &str) -> VfsStats {
    parse_fixture(name, &VFS_STATS_SPEC)
}
fn vfs_queue(name: &str) -> VfsQueue {
    parse_fixture(name, &VFS_QUEUE_SPEC)
}

/// Two files queued at once, captured from a live rclone v1.75.0.
///
/// A single-entry queue cannot exercise anything that joins per file: an assertion that
/// "every other file stays unmeasured" iterates zero times and passes whatever the code
/// does. This is the fixture the join in `TransferState::with_progress` is checked with.
#[test]
fn a_multi_item_queue_carries_a_distinct_entry_per_file() {
    let q: VfsQueue = vfs_queue("vfs-queue-two-items.json");
    assert_eq!(q.queue.len(), 2, "captured with two files queued");

    let names: Vec<_> = q.queue.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["one.bin", "two.bin"]);

    let ids: std::collections::BTreeSet<_> = q.queue.iter().map(|i| i.id).collect();
    assert_eq!(ids.len(), 2, "ids are what vfs/queue-set-expiry addresses");

    // Distinct sizes, so a test that mixed the two entries up would show it.
    assert_eq!(q.queue[0].size, 262_144);
    assert_eq!(q.queue[1].size, 131_072);
    assert_eq!(q.pending().known_bytes, 393_216);
    assert!(
        q.queue.iter().all(|i| !i.uploading),
        "captured before upload"
    );
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

    // Pinned exactly, and they differ: a key-set check cannot notice `speed` and
    // `speedAvg` being transposed, because the set is unchanged and the round trip is
    // self-consistent. A swap would show the user the wrong throughput.
    assert_eq!(t.speed, 4_471_532.801_626_206);
    assert_eq!(t.speed_avg, 4_475_791.878_358_628);
    assert_ne!(t.speed, t.speed_avg);
    // Same argument for the check counters.
    assert_eq!((s.checks, s.total_checks), (0, 0));

    // --bwlimit was 4M; the reported rate should be in that neighbourhood rather
    // than a placeholder.
    assert!(
        (3.5e6..5.0e6).contains(&t.speed),
        "speed {} should reflect the 4M bwlimit",
        t.speed
    );
}

#[test]
fn a_vfs_download_is_not_a_pending_upload() {
    // Captured while reading a file back through a --vfs-cache-mode full mount, with
    // the upload queue empty. rclone puts this in the SAME `global_stats` group as a
    // write-back upload, so a group-only filter reports it as pending upload bytes —
    // inflating the figure that decides whether unmounting is safe.
    let mut keys = core_stats_keys(true);
    // A download reports no dstFs: rclone omits it when the destination is nil.
    keys.retain(|k| *k != "transferring.[].dstFs");

    let s: CoreStats = parse_fixture(
        "core-stats-vfs-download-midflight.json",
        &FixtureSpec {
            keys: &keys,
            optional: CORE_STATS_OPTIONAL,
            ignored: CORE_STATS_IGNORED,
            opaque: &[],
        },
    );

    let t = &s.transfers_slice()[0];
    assert_eq!(t.group.as_deref(), Some("global_stats"));
    assert!(
        t.dst_fs.is_none(),
        "a download reports no dstFs — that asymmetry is the direction signal"
    );
    assert!(t.is_ungrouped(), "it really does share the uploads' group");

    // srcFs here is the REMOTE, not the cache, so the cache-path test rejects it.
    let cache = vfs_stats("vfs-stats-upload-in-progress.json")
        .disk_cache
        .expect("diskCache")
        .path;
    assert!(
        !t.is_writeback_upload(&cache),
        "a download must never count as a pending upload"
    );
    assert_eq!(s.writeback_uploads(&cache).count(), 0);
    assert_eq!(
        s.ungrouped_transfers().count(),
        1,
        "the group filter sees it"
    );
}

/// Every file in `testdata/` must be pinned by a test.
///
/// Without this the guard is opt-in: dropping in a new fixture and forgetting to
/// write a spec for it passes silently, and the file looks like coverage it is not.
#[test]
fn every_fixture_is_pinned() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata");
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .expect("testdata/")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    on_disk.sort();

    let mut pinned: Vec<String> = PINNED_FIXTURES.iter().map(|s| s.to_string()).collect();
    pinned.sort();

    assert_eq!(
        on_disk, pinned,
        "testdata/ and PINNED_FIXTURES disagree — a fixture with no spec is not \
         tested, and a spec with no fixture is dead"
    );
}

/// The complete set of fixtures, asserted against `testdata/` above.
const PINNED_FIXTURES: &[&str] = &[
    "core-stats-idle-no-transferring.json",
    "core-stats-mixed-vfs-and-job.json",
    "core-stats-vfs-download-midflight.json",
    "core-stats-vfs-upload-midflight.json",
    "core-version.json",
    "rc-list-v1.75.0.json",
    "vfs-list.json",
    "vfs-queue-queued-not-uploading.json",
    "vfs-queue-two-items.json",
    "vfs-queue-uploading.json",
    "vfs-stats-idle.json",
    "vfs-stats-upload-in-progress.json",
    "vfsmeta-item-dirty.json",
];

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

    let cache = vfs_stats("vfs-stats-upload-in-progress.json")
        .disk_cache
        .expect("diskCache")
        .path;
    let vfs: Vec<_> = s.writeback_uploads(&cache).map(|t| &*t.name).collect();
    assert_eq!(
        vfs,
        ["vfsfile.bin"],
        "only the write-back upload belongs to a mount"
    );

    let job = s
        .transfers_slice()
        .iter()
        .find(|t| !t.is_ungrouped())
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
    // Pinned to the captured values rather than compared to each other: asserting
    // pending_bytes() == item.size is tautological for a one-item queue, and `id`
    // and `size` being transposed would leave the key set unchanged.
    assert_eq!(item.id, 1);
    assert_eq!(item.size, 134_217_728);
    assert_eq!(queued.pending_bytes(), 134_217_728);
    assert!(queued.pending().is_exact());

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
