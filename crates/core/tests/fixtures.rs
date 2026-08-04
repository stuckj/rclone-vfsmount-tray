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
//! So every fixture is checked three ways:
//!
//! 1. it parses;
//! 2. `serde_ignored` reports which keys the model *dropped*, and that set must
//!    equal an explicit per-fixture list of fields we knowingly do not model — so
//!    any added, removed or renamed field fails a test that names it;
//! 3. it survives a round trip (kept for the self-consistency it does prove).

use rvt_core::models::*;
use std::collections::BTreeSet;

fn testdata(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/");
    let full = format!("{path}{name}");
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {full}: {e}"))
}

/// Parse a fixture, asserting exactly which of its keys the model ignores.
///
/// `expected_ignored` is the complete set of dotted paths we deliberately do not
/// model. Anything outside it — a key we stopped reading, or a new key rclone added
/// — fails the test and names the path.
fn parse_fixture<T>(name: &str, expected_ignored: &[&str]) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
{
    let raw = testdata(name);

    let mut ignored: BTreeSet<String> = BTreeSet::new();
    let de = &mut serde_json::Deserializer::from_str(&raw);
    let parsed: T = serde_ignored::deserialize(de, |path| {
        ignored.insert(normalise_indices(&path.to_string()));
    })
    .unwrap_or_else(|e| panic!("parsing {name}: {e}"));

    let expected: BTreeSet<String> = expected_ignored.iter().map(|s| s.to_string()).collect();
    if ignored != expected {
        let newly_dropped: Vec<_> = ignored.difference(&expected).collect();
        let no_longer_present: Vec<_> = expected.difference(&ignored).collect();
        panic!(
            "{name}: the set of unmodelled keys changed.\n  \
             dropped but not expected (a field we stopped reading, or rclone added one): {newly_dropped:?}\n  \
             expected but not seen (rclone removed or renamed it): {no_longer_present:?}"
        );
    }

    // Self-consistency. Weak on its own — see the module docs — but it does catch a
    // Serialize/Deserialize pair that disagree with each other.
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

/// Keys of `core/stats` we do not model: counters for operations this tool never
/// performs. Listed rather than ignored silently so their disappearance is noticed.
const CORE_STATS_UNMODELLED: &[&str] = &[
    "deletedDirs",
    "deletes",
    "listed",
    "renames",
    "serverSideCopies",
    "serverSideCopyBytes",
    "serverSideMoveBytes",
    "serverSideMoves",
];

/// `rc/list` carries per-command help text and request/response plumbing flags that
/// are of no use to a tray applet.
const RC_LIST_UNMODELLED: &[&str] = &[
    "commands.[].Help",
    "commands.[].NeedsRequest",
    "commands.[].NeedsResponse",
];

/// Build details of the rclone binary; we gate on `decomposed`, not on these.
const CORE_VERSION_UNMODELLED: &[&str] = &["goTags", "linking", "osArch", "osKernel", "osVersion"];

fn core_stats(name: &str) -> CoreStats {
    parse_fixture(name, CORE_STATS_UNMODELLED)
}
fn vfs_stats(name: &str) -> VfsStats {
    parse_fixture(name, &[])
}
fn vfs_queue(name: &str) -> VfsQueue {
    parse_fixture(name, &[])
}

#[test]
fn core_stats_mid_upload_has_real_progress() {
    let s: CoreStats = core_stats("core-stats-vfs-upload-midflight.json");
    let t = &s.transfers_slice()[0];

    assert_eq!(t.name, "big.bin");
    assert_eq!(t.size, 134_217_728);
    assert_eq!(t.known_size(), Some(134_217_728));
    assert!(
        t.bytes > 0 && t.bytes < t.known_size().unwrap(),
        "mid-flight: {}",
        t.bytes
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
    let s: CoreStats = core_stats("core-stats-idle-no-transferring.json");
    assert!(
        !s.reported_transferring(),
        "the idle fixture must have NO transferring key — that is the behaviour \
         this model exists to represent"
    );
    assert!(s.transfers_slice().is_empty());
}

#[test]
fn vfs_and_job_transfers_are_separable() {
    let s: CoreStats = core_stats("core-stats-mixed-vfs-and-job.json");

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
    let stats: CoreStats = core_stats("core-stats-vfs-upload-midflight.json");
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
    let stats: CoreStats = core_stats("core-stats-vfs-upload-midflight.json");
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
    let m: VfsMetaItem = parse_fixture("vfsmeta-item-dirty.json", &[]);
    assert!(m.dirty, "a pending upload is Dirty on disk");
    assert_eq!(m.size, 100_663_296);
    assert_eq!(m.ranges.as_ref().map(Vec::len), Some(1));
    assert!(m.mod_time.ends_with('Z'), "RFC 3339: {}", m.mod_time);
    // Empty on the local backend. Other backends are checked in issue #10.
    assert_eq!(m.fingerprint, "");
}

#[test]
fn rc_list_supports_feature_detection() {
    let l: RcList = parse_fixture("rc-list-v1.75.0.json", RC_LIST_UNMODELLED);
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
    let v: CoreVersion = parse_fixture("core-version.json", CORE_VERSION_UNMODELLED);
    assert_eq!(v.version, "v1.75.0");
    assert_eq!(v.decomposed, vec![1, 75, 0]);
    assert!(!v.is_beta);
}

#[test]
fn vfs_list_names_are_canonical_not_configured() {
    let l: VfsList = parse_fixture("vfs-list.json", &[]);
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
