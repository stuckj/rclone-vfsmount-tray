//! Parse the captured rclone responses in `testdata/`.
//!
//! These files are the literal bytes a live rclone v1.75.0 returned during the
//! investigation in issue #9. They exist so that a future rclone changing its
//! wire format is caught here, by a test naming the field that moved, rather than
//! in the field as a tray icon that silently stops showing progress.
//!
//! Every fixture is checked two ways: it must parse, and it must survive a
//! serialize/deserialize round trip unchanged. The round trip is compared as
//! structs rather than as JSON text, because field order and float formatting are
//! not stable and comparing text would fail for reasons nobody cares about.

use rvt_core::models::*;

fn testdata(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/");
    let full = format!("{path}{name}");
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {full}: {e}"))
}

fn round_trip<T>(name: &str) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
{
    let raw = testdata(name);
    let parsed: T = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {name}: {e}"));
    let reser = serde_json::to_string(&parsed).expect("serialize");
    let again: T =
        serde_json::from_str(&reser).unwrap_or_else(|e| panic!("re-parsing {name}: {e}"));
    assert_eq!(parsed, again, "{name} did not survive a round trip");
    parsed
}

#[test]
fn core_stats_mid_upload_has_real_progress() {
    let s: CoreStats = round_trip("core-stats-vfs-upload-midflight.json");
    let t = &s.transfers_slice()[0];

    assert_eq!(t.name, "big.bin");
    assert_eq!(t.size, 134_217_728);
    assert!(t.bytes > 0 && t.bytes < t.size, "mid-flight: {}", t.bytes);
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
    let s: CoreStats = round_trip("core-stats-idle-no-transferring.json");
    assert!(
        !s.reported_transferring(),
        "the idle fixture must have NO transferring key — that is the behaviour \
         this model exists to represent"
    );
    assert!(s.transfers_slice().is_empty());
}

#[test]
fn vfs_and_job_transfers_are_separable() {
    let s: CoreStats = round_trip("core-stats-mixed-vfs-and-job.json");

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
    let stats: CoreStats = round_trip("core-stats-vfs-upload-midflight.json");
    let vfs: VfsStats = round_trip("vfs-stats-upload-in-progress.json");

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
    let stats: CoreStats = round_trip("core-stats-vfs-upload-midflight.json");
    let queue: VfsQueue = round_trip("vfs-queue-uploading.json");

    let tname = &stats.transfers_slice()[0].name;
    let qname = &queue.queue[0].name;
    assert_eq!(
        tname, qname,
        "the join key between core/stats and vfs/queue is an exact name match"
    );
}

#[test]
fn queue_queued_versus_uploading() {
    let queued: VfsQueue = round_trip("vfs-queue-queued-not-uploading.json");
    let item = &queued.queue[0];
    assert!(!item.uploading, "still waiting out --vfs-write-back");
    assert!(item.expiry > 0.0, "expiry counts down to zero");
    assert_eq!(queued.uploading().count(), 0);
    assert_eq!(queued.pending_bytes(), item.size);

    let live: VfsQueue = round_trip("vfs-queue-uploading.json");
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
    let s: VfsStats = round_trip("vfs-stats-upload-in-progress.json");
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
    let s: VfsStats = round_trip("vfs-stats-idle.json");
    let dc = s.disk_cache.expect("diskCache");
    assert_eq!(dc.uploads_in_progress, 0);
    assert!(!dc.out_of_space);
    assert_eq!(dc.errored_files, 0);
}

#[test]
fn vfsmeta_dirty_item_is_the_offline_signal() {
    let m: VfsMetaItem = round_trip("vfsmeta-item-dirty.json");
    assert!(m.dirty, "a pending upload is Dirty on disk");
    assert_eq!(m.size, 100_663_296);
    assert_eq!(m.ranges.as_ref().map(Vec::len), Some(1));
    assert!(m.mod_time.ends_with('Z'), "RFC 3339: {}", m.mod_time);
    // Empty on the local backend. Other backends are checked in issue #10.
    assert_eq!(m.fingerprint, "");
}

#[test]
fn rc_list_supports_feature_detection() {
    let l: RcList = round_trip("rc-list-v1.75.0.json");
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
    let v: CoreVersion = round_trip("core-version.json");
    assert_eq!(v.version, "v1.75.0");
    assert_eq!(v.decomposed, vec![1, 75, 0]);
    assert!(!v.is_beta);
}

#[test]
fn vfs_list_names_are_canonical_not_configured() {
    let l: VfsList = round_trip("vfs-list.json");
    assert_eq!(l.vfses.len(), 1);
    // The fixture was captured from an `alias` remote configured as `dst:`, yet
    // rclone reports the resolved path. Anything matching on this string must not
    // assume it equals what the user configured.
    assert!(
        !l.vfses[0].starts_with("dst:"),
        "expected the resolved form, got {:?}",
        l.vfses[0]
    );
}
