//! Typed models for rclone's remote-control (rc) API responses, and for the
//! on-disk VFS cache metadata.
//!
//! Every shape here was measured against a live rclone **v1.75.0** during the
//! investigation in issue #9, not transcribed from documentation. The fixtures in
//! `testdata/` are the exact bytes that came back, and `tests/fixtures.rs` asserts
//! these types still parse them.
//!
//! # Tolerance
//!
//! Unknown fields are ignored rather than rejected — rclone adds fields between
//! releases and a strict parser would turn that into a hard failure in the field.
//! Conversely, several fields that "obviously" always exist are modelled as
//! [`Option`], because the measurements showed otherwise. Those cases are called
//! out individually below; each one is a real behaviour that cost time to find.

use serde::{Deserialize, Serialize};

/// The accounting group that rclone assigns to VFS write-back uploads.
///
/// Transfers that originate from an explicit rc job (`operations/copyfile`,
/// `sync/copy`, …) are grouped as `job/<n>` instead. Filtering on this is how a
/// mount's upload progress is separated from unrelated work happening in the same
/// rclone process — see [`CoreStats::vfs_writeback_transfers`].
///
/// Note that `core/group-list` does **not** enumerate this group (it lists only
/// `job/*`), so it cannot be discovered at runtime. It is a constant.
pub const GLOBAL_STATS_GROUP: &str = "global_stats";

// ---------------------------------------------------------------------------
// core/stats
// ---------------------------------------------------------------------------

/// Response from `core/stats`.
///
/// These figures are **process-global**: one rclone process serving several VFSes
/// reports all of their transfers here together. Use
/// [`CoreStats::vfs_writeback_transfers`] and [`Transfer::belongs_to_cache`] to
/// narrow them down.
///
/// `core/stats` also accepts a `group` parameter, which filters server-side and is
/// cheaper than filtering here; and `short: true`, which omits `transferring`
/// entirely when only the totals are wanted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreStats {
    /// Bytes transferred so far across all active transfers.
    #[serde(default)]
    pub bytes: u64,
    /// Total bytes expected across all active transfers.
    #[serde(default)]
    pub total_bytes: u64,
    /// Aggregate throughput in bytes per second.
    #[serde(default)]
    pub speed: f64,
    /// Estimated seconds remaining.
    ///
    /// `null` early in a transfer, before rclone has enough samples to estimate.
    #[serde(default)]
    pub eta: Option<f64>,
    /// Transfers that have **completed**. In-flight transfers are not counted here,
    /// which is why this reads `0` throughout a single upload.
    #[serde(default)]
    pub transfers: u64,
    /// Transfers started, including those still running.
    #[serde(default)]
    pub total_transfers: u64,
    #[serde(default)]
    pub errors: u64,
    #[serde(default)]
    pub fatal_error: bool,
    #[serde(default)]
    pub retry_error: bool,
    #[serde(default)]
    pub checks: u64,
    #[serde(default)]
    pub total_checks: u64,
    #[serde(default)]
    pub elapsed_time: f64,
    #[serde(default)]
    pub transfer_time: f64,

    /// Currently active transfers.
    ///
    /// **This key is absent — not an empty array — when nothing is transferring.**
    /// Modelling it as a plain `Vec` with a default would erase the difference
    /// between "rclone told us there is nothing in flight" and "rclone did not tell
    /// us anything", which matters when deciding whether to trust a zero.
    #[serde(default)]
    pub transferring: Option<Vec<Transfer>>,
}

impl CoreStats {
    /// Active transfers, or an empty slice when the key was absent.
    pub fn transfers_slice(&self) -> &[Transfer] {
        self.transferring.as_deref().unwrap_or(&[])
    }

    /// Whether rclone reported the `transferring` key at all.
    ///
    /// `false` means "no information", which is not the same as "nothing in flight".
    pub fn reported_transferring(&self) -> bool {
        self.transferring.is_some()
    }

    /// Active transfers that are VFS write-back uploads, excluding explicit rc jobs.
    ///
    /// Prefer asking rclone to do this by passing `group: "global_stats"` to
    /// `core/stats`; this exists for responses that were fetched unfiltered.
    pub fn vfs_writeback_transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers_slice()
            .iter()
            .filter(|t| t.is_vfs_writeback())
    }
}

/// One entry of `core/stats` `transferring[]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// Path of the file within the VFS — e.g. `photos/img.raw`.
    ///
    /// This is the **join key** to [`QueueItem::name`]: for VFS write-back uploads
    /// the two strings are identical, with no remote or cache-path prefix.
    pub name: String,
    /// Total size of the file in bytes.
    #[serde(default)]
    pub size: u64,
    /// Bytes uploaded so far.
    #[serde(default)]
    pub bytes: u64,
    /// Completion percentage.
    ///
    /// Observed to top out at 98 — a transfer's entry disappears from
    /// `transferring[]` on completion rather than reaching 100, so do not wait for
    /// this to hit 100 to consider an upload finished.
    #[serde(default)]
    pub percentage: Option<u32>,
    /// Instantaneous throughput in bytes per second.
    ///
    /// The **first** reading after a transfer starts is unreliable — it averages
    /// over a very short window and can be several times the true rate. Discard or
    /// suppress it rather than showing it.
    #[serde(default)]
    pub speed: f64,
    /// Smoothed average throughput in bytes per second.
    #[serde(default)]
    pub speed_avg: f64,
    /// Estimated seconds remaining; `null` until rclone can estimate.
    #[serde(default)]
    pub eta: Option<f64>,
    /// Accounting group. [`GLOBAL_STATS_GROUP`] for VFS write-back uploads,
    /// `job/<n>` for explicit rc jobs.
    #[serde(default)]
    pub group: Option<String>,
    /// Source filesystem. For a VFS write-back upload this contains the VFS **cache**
    /// directory, which matches [`DiskCache::path`] for the owning VFS — that is how
    /// a transfer is attributed to a specific mount.
    #[serde(default)]
    pub src_fs: Option<String>,
    /// Destination filesystem — the remote being uploaded to.
    #[serde(default)]
    pub dst_fs: Option<String>,
}

impl Transfer {
    /// Whether this transfer is a VFS write-back upload rather than an explicit job.
    ///
    /// A transfer with no `group` at all is treated as *not* a write-back upload:
    /// showing an unrelated `rclone copy` as a mount's pending upload is worse than
    /// briefly omitting a real one.
    pub fn is_vfs_writeback(&self) -> bool {
        self.group.as_deref() == Some(GLOBAL_STATS_GROUP)
    }

    /// Whether this transfer originates from the given VFS cache directory.
    ///
    /// Pass [`DiskCache::path`] for the mount in question. `src_fs` embeds the cache
    /// path but is not equal to it — rclone prefixes a backend tag such as
    /// `:local{8un-i}:` — so this is a containment test, not equality.
    pub fn belongs_to_cache(&self, cache_path: &str) -> bool {
        !cache_path.is_empty()
            && self
                .src_fs
                .as_deref()
                .is_some_and(|s| s.contains(cache_path))
    }
}

// ---------------------------------------------------------------------------
// vfs/queue
// ---------------------------------------------------------------------------

/// Response from `vfs/queue` for one VFS (selected with the `fs` parameter).
///
/// This is the minimum viable source of "how much is left to send": summing
/// [`QueueItem::size`] gives the outstanding bytes even when no per-file progress
/// is available.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VfsQueue {
    #[serde(default)]
    pub queue: Vec<QueueItem>,
}

impl VfsQueue {
    /// Total bytes still to upload.
    pub fn pending_bytes(&self) -> u64 {
        self.queue.iter().map(|i| i.size).sum()
    }

    /// Items rclone is actively uploading right now.
    pub fn uploading(&self) -> impl Iterator<Item = &QueueItem> {
        self.queue.iter().filter(|i| i.uploading)
    }
}

/// One queued write-back upload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Path within the VFS. The join key to [`Transfer::name`].
    pub name: String,
    /// Opaque id, required by `vfs/queue-set-expiry` to force an upload.
    pub id: u64,
    /// File size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Seconds until rclone will start the upload.
    ///
    /// **Signed** — it goes negative once the item is due, and was observed at
    /// `-0.32` while uploading. An unsigned type here would fail to parse.
    #[serde(default)]
    pub expiry: f64,
    /// The configured `--vfs-write-back` delay, in seconds.
    #[serde(default)]
    pub delay: f64,
    /// Upload attempts so far. A climbing value means repeated failure.
    #[serde(default)]
    pub tries: u64,
    /// Whether the upload is in flight right now.
    #[serde(default)]
    pub uploading: bool,
}

// ---------------------------------------------------------------------------
// vfs/stats
// ---------------------------------------------------------------------------

/// Response from `vfs/stats` for one VFS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VfsStats {
    /// The VFS this describes.
    #[serde(default)]
    pub fs: Option<String>,
    #[serde(default)]
    pub in_use: Option<u64>,
    /// Only present when `--vfs-cache-mode` is greater than `off`.
    #[serde(default)]
    pub disk_cache: Option<DiskCache>,
    #[serde(default)]
    pub metadata_cache: Option<MetadataCache>,
    /// The full VFS option block. Kept as raw JSON deliberately: it carries 30+
    /// fields that change between rclone releases and nothing here needs them
    /// typed. Anything that does should pull out the specific key it wants.
    #[serde(default)]
    pub opt: Option<serde_json::Value>,
}

/// Disk cache portion of `vfs/stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCache {
    /// **Not** the number of bytes pending upload, and not reliably the cache size
    /// either — it was measured as `0` throughout a 128 MiB upload with the data
    /// sitting in the cache. Do not put this in front of a user. Use
    /// [`VfsQueue::pending_bytes`] for outstanding bytes.
    pub bytes_used: i64,
    /// Files whose upload has failed. Needs surfacing — it is actionable.
    #[serde(default)]
    pub errored_files: u64,
    /// Number of files in the cache.
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub hash_type: i64,
    /// The cache has run out of room. Actionable, and silently breaks uploads.
    #[serde(default)]
    pub out_of_space: bool,
    /// Absolute path to the cache data tree, given verbatim. This is what makes the
    /// on-disk scanning tier possible without guessing at paths.
    #[serde(default)]
    pub path: String,
    /// Absolute path to the cache metadata (`vfsMeta`) tree.
    #[serde(default)]
    pub path_meta: String,
    /// Uploads currently in flight.
    #[serde(default)]
    pub uploads_in_progress: u64,
    /// Uploads waiting to start.
    #[serde(default)]
    pub uploads_queued: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetadataCache {
    #[serde(default)]
    pub dirs: u64,
    #[serde(default)]
    pub files: u64,
}

// ---------------------------------------------------------------------------
// vfs/list, core/version, rc/list
// ---------------------------------------------------------------------------

/// Response from `vfs/list` — the VFSes this rclone process is serving.
///
/// The names returned are rclone's canonical form, which is **not** necessarily the
/// remote name that was configured: an `alias` remote reports the resolved target
/// path. Both forms are accepted as the `fs` parameter elsewhere, but do not assume
/// the string here round-trips to what the user typed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VfsList {
    #[serde(default)]
    pub vfses: Vec<String>,
}

/// Response from `core/version`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreVersion {
    /// Human-readable version, e.g. `v1.75.0`.
    #[serde(default)]
    pub version: String,
    /// Version split into components, e.g. `[1, 75, 0]`. Prefer this for comparisons.
    #[serde(default)]
    pub decomposed: Vec<u64>,
    #[serde(default)]
    pub is_beta: bool,
    #[serde(default)]
    pub is_git: bool,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub go_version: String,
}

/// Response from `rc/list` — every rc command this rclone build registers.
///
/// This is the right primitive for feature detection: it reflects how rclone was
/// actually built and flagged, where a version comparison only guesses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RcList {
    #[serde(default)]
    pub commands: Vec<RcCommand>,
}

impl RcList {
    /// Whether the given rc command (e.g. `vfs/queue`) is available.
    pub fn has(&self, path: &str) -> bool {
        self.commands.iter().any(|c| c.path == path)
    }
}

/// One entry of `rc/list`. Note the PascalCase field names in the wire format.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RcCommand {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Title", default)]
    pub title: String,
    #[serde(rename = "AuthRequired", default)]
    pub auth_required: bool,
}

// ---------------------------------------------------------------------------
// On-disk VFS cache metadata
// ---------------------------------------------------------------------------

/// One item of rclone's on-disk VFS cache metadata.
///
/// These live under `<cache>/vfsMeta/<backend>/<path>`, mirroring the data tree at
/// `<cache>/vfs/<backend>/<path>`. Reading them is what allows pending-upload state
/// to be reported when the rc API is unreachable — and, unlike the rc endpoints,
/// they survive an rclone crash, because a dead process's dirty items are still on
/// disk.
///
/// Note the PascalCase wire format, unlike every rc response above.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VfsMetaItem {
    /// Last modification time, RFC 3339.
    ///
    /// Left as a string deliberately: parsing it would pull a date/time crate into
    /// this dependency-minimal crate, and nothing needs it typed yet.
    #[serde(rename = "ModTime", default)]
    pub mod_time: String,
    /// Last access time, RFC 3339.
    #[serde(rename = "ATime", default)]
    pub atime: String,
    /// Full size of the file in bytes.
    #[serde(rename = "Size", default)]
    pub size: u64,
    /// Byte ranges present in the local cache. `null` when none are.
    #[serde(rename = "Rs", default)]
    pub ranges: Option<Vec<Range>>,
    /// Fingerprint of the remote object. Observed empty on the local backend.
    #[serde(rename = "Fingerprint", default)]
    pub fingerprint: String,
    /// **The field that matters**: the item has been modified locally and not yet
    /// uploaded. Summing [`Self::size`] over dirty items gives the bytes still to
    /// send.
    ///
    /// It stays `true` until the upload completes, so it cannot distinguish
    /// "queued" from "uploading" — that distinction needs [`QueueItem::uploading`].
    #[serde(rename = "Dirty", default)]
    pub dirty: bool,
}

/// A byte range present in the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    #[serde(rename = "Pos", default)]
    pub pos: u64,
    #[serde(rename = "Size", default)]
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transferring_absent_is_distinct_from_empty() {
        let absent: CoreStats = serde_json::from_str(r#"{"bytes":0}"#).unwrap();
        assert!(!absent.reported_transferring());
        assert!(absent.transfers_slice().is_empty());

        let empty: CoreStats = serde_json::from_str(r#"{"bytes":0,"transferring":[]}"#).unwrap();
        assert!(empty.reported_transferring());
        assert!(empty.transfers_slice().is_empty());
    }

    #[test]
    fn eta_is_nullable() {
        let s: CoreStats = serde_json::from_str(r#"{"eta":null}"#).unwrap();
        assert_eq!(s.eta, None);
        let s: CoreStats = serde_json::from_str(r#"{"eta":14}"#).unwrap();
        assert_eq!(s.eta, Some(14.0));
    }

    #[test]
    fn queue_expiry_may_be_negative() {
        let q: VfsQueue = serde_json::from_str(
            r#"{"queue":[{"name":"a","id":1,"size":10,"expiry":-0.32414583,"uploading":true}]}"#,
        )
        .unwrap();
        assert!(q.queue[0].expiry < 0.0);
        assert_eq!(q.pending_bytes(), 10);
        assert_eq!(q.uploading().count(), 1);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let s: CoreStats =
            serde_json::from_str(r#"{"bytes":1,"somethingRcloneAddedIn1_80":true}"#).unwrap();
        assert_eq!(s.bytes, 1);
    }

    #[test]
    fn writeback_transfers_exclude_jobs() {
        let s: CoreStats = serde_json::from_str(
            r#"{"transferring":[
                 {"name":"a","group":"global_stats"},
                 {"name":"b","group":"job/12"},
                 {"name":"c"}
               ]}"#,
        )
        .unwrap();
        let names: Vec<_> = s.vfs_writeback_transfers().map(|t| &t.name).collect();
        assert_eq!(
            names,
            ["a"],
            "jobs and group-less transfers must be excluded"
        );
    }

    #[test]
    fn cache_attribution_is_containment_not_equality() {
        let t = Transfer {
            src_fs: Some(":local{8un-i}:/cache/vfs/local/srv/data".into()),
            ..Default::default()
        };
        assert!(t.belongs_to_cache("/cache/vfs/local/srv/data"));
        assert!(!t.belongs_to_cache("/cache/vfs/local/other"));
        // An empty path must never match everything.
        assert!(!t.belongs_to_cache(""));
    }

    #[test]
    fn rc_list_feature_detection() {
        let l: RcList =
            serde_json::from_str(r#"{"commands":[{"Path":"vfs/queue","Title":"Queue info"}]}"#)
                .unwrap();
        assert!(l.has("vfs/queue"));
        assert!(!l.has("vfs/stats"));
    }
}
