//! Typed models for rclone's rc API responses and its on-disk VFS cache metadata.
//!
//! Shapes were measured against a live rclone v1.75.0 (issue #9); `testdata/` holds the
//! captured bytes and `tests/fixtures.rs` pins them. `last_error` and `checking` are the
//! exceptions — modelled from rclone's source, since neither could be captured.
//!
//! Unknown fields are ignored, since rclone adds them between releases.

use serde::{Deserialize, Serialize};

/// rclone's default accounting group — anything not run under an rc job (`job/<n>`).
///
/// Does **not** indicate direction: VFS cache downloads are ungrouped too. Use
/// [`Transfer::is_writeback_upload`]. See DESIGN.md, "capability ladder".
pub const GLOBAL_STATS_GROUP: &str = "global_stats";

// ---------------------------------------------------------------------------
// core/stats
// ---------------------------------------------------------------------------

/// Response from `core/stats`. Process-global: one rclone serving several VFSes reports
/// them all here, in both directions. Narrow with [`CoreStats::writeback_uploads`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreStats {
    /// Bytes transferred so far across all active transfers.
    #[serde(default)]
    pub bytes: i64,
    /// Total bytes expected. Signed: rclone's field is int64 and computed by subtraction.
    #[serde(default)]
    pub total_bytes: i64,
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
    pub transfers: i64,
    /// Transfers started, including those still running.
    #[serde(default)]
    pub total_transfers: i64,
    #[serde(default)]
    pub errors: i64,
    #[serde(default)]
    pub fatal_error: bool,
    #[serde(default)]
    pub retry_error: bool,
    #[serde(default)]
    pub checks: i64,
    #[serde(default)]
    pub total_checks: i64,
    #[serde(default)]
    pub elapsed_time: f64,
    #[serde(default)]
    pub transfer_time: f64,

    /// Most recent error text; present only when `errors > 0`.
    ///
    /// This and [`Self::checking`] are modelled from rclone's source rather than captured —
    /// see the module docs. Covered by unit tests, not fixtures.
    #[serde(default)]
    pub last_error: Option<String>,

    /// Files being checked. Reachable from a mount: rclone accounts a rename as a "moving"
    /// check. Checks never appear in [`Self::transferring`], so they cannot skew uploads.
    #[serde(default)]
    pub checking: Option<Vec<String>>,

    /// Active transfers. **Absent, not `[]`, when nothing is in flight** — the
    /// distinction matters when deciding whether to trust a zero.
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

    /// Write-back uploads out of the given VFS cache (`cache_path` = that mount's
    /// [`DiskCache::path`]). Needs both conditions — the group alone admits downloads.
    pub fn writeback_uploads<'a>(
        &'a self,
        cache_path: &'a str,
    ) -> impl Iterator<Item = &'a Transfer> {
        self.transfers_slice()
            .iter()
            .filter(move |t| t.is_writeback_upload(cache_path))
    }

    /// Transfers not started by an rc job. Includes downloads; prefer
    /// [`Self::writeback_uploads`] unless you want both directions.
    pub fn ungrouped_transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers_slice().iter().filter(|t| t.is_ungrouped())
    }
}

/// One entry of `core/stats` `transferring[]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// Path within the VFS. The join key to [`QueueItem::name`] — identical strings.
    pub name: String,
    /// Size in bytes, or `-1` when unknown. Signed: a `u64` would abort the whole
    /// response, not just this field. See [`Self::known_size`].
    #[serde(default)]
    pub size: i64,
    /// Bytes uploaded so far. Signed for the same reason as [`Self::size`].
    #[serde(default)]
    pub bytes: i64,
    /// Completion percentage. Tops out at 98 — completion is the entry disappearing.
    #[serde(default)]
    pub percentage: Option<u32>,
    /// Instantaneous throughput. The **first reading after a transfer starts is
    /// unreliable** — suppress it.
    #[serde(default)]
    pub speed: f64,
    /// Smoothed average throughput in bytes per second.
    #[serde(default)]
    pub speed_avg: f64,
    /// Estimated seconds remaining; `null` until rclone can estimate.
    #[serde(default)]
    pub eta: Option<f64>,
    /// Accounting group. Absent until rclone attaches accounting; see
    /// [`Self::has_accounting`] and [`GLOBAL_STATS_GROUP`].
    #[serde(default)]
    pub group: Option<String>,
    /// Source filesystem: the VFS cache for an upload, the remote for a download.
    #[serde(default)]
    pub src_fs: Option<String>,
    /// Destination filesystem — the remote being uploaded to.
    #[serde(default)]
    pub dst_fs: Option<String>,
}

impl Transfer {
    /// Whether rclone has attached accounting yet. Until it has, `bytes`, `percentage`,
    /// `speed`, `speed_avg`, `eta` and `group` are absent and the numeric ones read as 0.
    /// Both accessors above already exclude such transfers; this is for raw iteration.
    pub fn has_accounting(&self) -> bool {
        self.group.is_some()
    }

    /// Not started by an rc job. Says nothing about direction — see
    /// [`GLOBAL_STATS_GROUP`]. A transfer with no group counts as `false`.
    pub fn is_ungrouped(&self) -> bool {
        self.group.as_deref() == Some(GLOBAL_STATS_GROUP)
    }

    /// A write-back upload out of `cache_path`: ungrouped **and** sourced from that cache.
    /// The cache check is what establishes direction — a download's `srcFs` is the remote.
    pub fn is_writeback_upload(&self, cache_path: &str) -> bool {
        self.is_ungrouped() && self.belongs_to_cache(cache_path)
    }

    /// The file size, or `None` when rclone reported it as unknown (`-1`).
    pub fn known_size(&self) -> Option<u64> {
        u64::try_from(self.size).ok()
    }

    /// Whether this transfer came out of `cache_path` (a mount's [`DiskCache::path`]).
    ///
    /// Compares for equality after stripping rclone's `:backend{hash}:` tag. Looser matching
    /// misattributes between sibling and nested mounts — see DESIGN.md.
    pub fn belongs_to_cache(&self, cache_path: &str) -> bool {
        if cache_path.is_empty() {
            return false;
        }
        let Some(src) = self.src_fs.as_deref() else {
            return false;
        };
        strip_backend_tag(src) == cache_path
    }
}

/// Strip rclone's `:backend{hash}:` prefix, if present.
///
/// Splits on the closing `}:`; the hash is base64url, so it cannot contain `}` or `:`.
fn strip_backend_tag(fs: &str) -> &str {
    fs.strip_prefix(':')
        .and_then(|rest| rest.find("}:").map(|i| &rest[i + 2..]))
        .unwrap_or(fs)
}

// ---------------------------------------------------------------------------
// vfs/queue
// ---------------------------------------------------------------------------

/// Response from `vfs/queue` for one VFS. Summing sizes answers "how much is left"
/// even with no per-file progress.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VfsQueue {
    /// **Absent, not `[]`, when this VFS has no write-back cache** — measured against
    /// rclone v1.75.0, which answers a `--vfs-cache-mode off` mount with `{}` and HTTP
    /// 200 rather than an error. Reading that as an empty queue turns "this mount cannot
    /// be observed" into "this mount has nothing outstanding".
    #[serde(default)]
    pub queue: Option<Vec<QueueItem>>,
}

impl VfsQueue {
    /// The queued items, or an empty slice when the key was absent.
    pub fn items(&self) -> &[QueueItem] {
        self.queue.as_deref().unwrap_or(&[])
    }

    /// Whether rclone reported the `queue` key at all.
    ///
    /// `false` means "this VFS has no write-back queue", which is not the same as
    /// "the queue is empty".
    pub fn reported_queue(&self) -> bool {
        self.queue.is_some()
    }

    /// What is still waiting to upload, including how much of it cannot be measured.
    pub fn pending(&self) -> Pending {
        let mut p = Pending::default();
        for item in self.items() {
            p.files += 1;
            match u64::try_from(item.size) {
                Ok(bytes) => p.known_bytes += bytes,
                Err(_) => p.unknown_size_files += 1,
            }
        }
        p
    }

    /// Bytes still to upload, **counting only files whose size is known** — a lower
    /// bound. Prefer [`Self::pending`] anywhere a user sees the number.
    pub fn pending_bytes(&self) -> u64 {
        self.pending().known_bytes
    }

    /// Items rclone is actively uploading right now.
    pub fn uploading(&self) -> impl Iterator<Item = &QueueItem> {
        self.items().iter().filter(|i| i.uploading)
    }
}

/// Outstanding uploads, carrying the unmeasurable remainder so a floor cannot be
/// mistaken for a total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Pending {
    /// Files waiting to upload.
    pub files: u64,
    /// Bytes across files whose size is known.
    pub known_bytes: u64,
    /// Files whose size rclone reported as unknown, and which therefore contribute
    /// nothing to `known_bytes`.
    pub unknown_size_files: u64,
}

impl Pending {
    /// Construct a summary. Needed because this type is `#[non_exhaustive]`.
    pub fn new(files: u64, known_bytes: u64, unknown_size_files: u64) -> Self {
        Self {
            files,
            known_bytes,
            unknown_size_files,
        }
    }

    /// Whether anything is outstanding.
    pub fn is_empty(&self) -> bool {
        self.files == 0
    }

    /// Whether `known_bytes` is the whole story, or merely a floor.
    pub fn is_exact(&self) -> bool {
        self.unknown_size_files == 0
    }
}

/// One queued write-back upload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Path within the VFS. The join key to [`Transfer::name`]; required, unlike the rest.
    pub name: String,
    /// Opaque id, required by `vfs/queue-set-expiry` to force an upload.
    #[serde(default)]
    pub id: u64,
    /// File size in bytes, or `-1` when unknown. Signed for the same reason as
    /// [`Transfer::size`].
    #[serde(default)]
    pub size: i64,
    /// Seconds until upload starts. **Signed** — goes negative once due.
    #[serde(default)]
    pub expiry: f64,
    /// Current back-off before the next attempt, in seconds — **not** the configured
    /// `--vfs-write-back`, though it starts there. rclone doubles it on every failure,
    /// capped at 5 minutes: measured 3 → 6 → 12 → 24 → 48 with `--vfs-write-back 3s`.
    /// Reset to the configured value when an upload is cancelled by the file being
    /// modified again.
    #[serde(default)]
    pub delay: f64,
    /// Upload attempts so far, counted from the moment each one starts, so an attempt in
    /// flight already reads 1. Writing to a file that is already queued **resets** this,
    /// along with [`Self::delay`] — rclone drops the entry and re-queues it — so a value
    /// above 1 is evidence of failure rather than of a file being re-saved. Measured
    /// 4 → 1 across a modify.
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

impl VfsStats {
    /// The cache mode this VFS is actually running with, from `opt.CacheMode`.
    ///
    /// The running mount's own answer rather than the configured one, so it holds for a
    /// mount somebody else started. `None` when the key is absent or unrecognised, which
    /// callers must treat as "unknown", never as `Off`.
    ///
    /// rclone encodes the mode as its `vfs.CacheMode` ordinal; the values below were read
    /// back from a live rclone v1.75.0 mounted at each setting.
    pub fn cache_mode(&self) -> Option<crate::config::CacheMode> {
        use crate::config::CacheMode;
        match self.opt.as_ref()?.get("CacheMode")?.as_u64()? {
            0 => Some(CacheMode::Off),
            1 => Some(CacheMode::Minimal),
            2 => Some(CacheMode::Writes),
            3 => Some(CacheMode::Full),
            _ => None,
        }
    }
}

/// Disk cache portion of `vfs/stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCache {
    /// **Not pending bytes, and not reliably cache size** — measured as 0 throughout a
    /// 128 MiB upload. Do not surface it. Use [`VfsQueue::pending`].
    #[serde(default)]
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

/// Response from `vfs/list`. Names are rclone's canonical form, which is not
/// necessarily the configured remote name — an alias reports its resolved target.
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

/// Response from `rc/list`. The right primitive for feature detection: it reflects
/// how rclone was built, where a version comparison only guesses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RcList {
    /// Deliberately **not** `#[serde(default)]`. This is the field capability detection
    /// reads, so if a future rclone renames or wraps it, defaulting would produce an empty
    /// command set that is indistinguishable from a genuine answer — every mount silently
    /// at T4, reporting nothing wrong. Without the default the same drift is a `Decode`
    /// error, which is surfaced. The fixture guard cannot catch this: it replays the
    /// captured payload, not the new one.
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
    /// Command path, e.g. `vfs/queue`. Required — an entry without one has no
    /// identity and nothing downstream could use it.
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Title", default)]
    pub title: String,
    /// **Polarity:** rclone's field is `NoAuth`, so `true` means auth is *not* required.
    /// Prefer [`Self::auth_required`]. There is no `AuthRequired` field.
    #[serde(rename = "NoAuth", default)]
    pub no_auth: bool,
}

impl RcCommand {
    /// Whether calling this command requires authentication.
    pub fn auth_required(&self) -> bool {
        !self.no_auth
    }
}

// ---------------------------------------------------------------------------
// On-disk VFS cache metadata
// ---------------------------------------------------------------------------

/// One item of rclone's on-disk VFS cache metadata, at
/// `<cache>/vfsMeta/<backend>/<path>`. Readable when the rc API is not, and it survives
/// an rclone crash. Note the PascalCase wire format.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VfsMetaItem {
    /// Last modification time, RFC 3339. Left as a string; nothing needs it typed yet.
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
    /// Modified locally and not yet uploaded. Summing sizes over dirty items gives bytes
    /// still to send. Stays true until upload completes, so it cannot distinguish
    /// "queued" from "uploading".
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
        assert!(q.items()[0].expiry < 0.0);
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
    fn writeback_uploads_exclude_jobs_and_downloads() {
        const CACHE: &str = "/c/vfs/local/srv/data";
        let s: CoreStats = serde_json::from_str(
            r#"{"transferring":[
                 {"name":"upload","group":"global_stats",
                  "srcFs":":local{x}:/c/vfs/local/srv/data","dstFs":"/srv/data"},
                 {"name":"download","group":"global_stats","srcFs":"/srv/data"},
                 {"name":"job","group":"job/12","srcFs":"/elsewhere"},
                 {"name":"nogroup","srcFs":":local{x}:/c/vfs/local/srv/data"}
               ]}"#,
        )
        .unwrap();

        let names: Vec<_> = s.writeback_uploads(CACHE).map(|t| &*t.name).collect();
        assert_eq!(
            names,
            ["upload"],
            "a cache DOWNLOAD shares the global_stats group and must not be counted \
             as a pending upload — it would inflate the figure that gates unmounting"
        );

        // The group filter alone sees both directions; that is why it is not enough.
        let ungrouped: Vec<_> = s.ungrouped_transfers().map(|t| &*t.name).collect();
        assert_eq!(ungrouped, ["upload", "download"]);
    }

    #[test]
    fn cache_attribution_strips_the_backend_tag() {
        let t = Transfer {
            src_fs: Some(":local{8un-i}:/cache/vfs/local/srv/data".into()),
            ..Default::default()
        };
        assert!(t.belongs_to_cache("/cache/vfs/local/srv/data"));
        assert!(!t.belongs_to_cache("/cache/vfs/local/other"));
        // An empty path must never match everything.
        assert!(!t.belongs_to_cache(""));
        // Nor must a root path.
        assert!(!t.belongs_to_cache("/"));
    }

    #[test]
    fn nested_cache_paths_do_not_cross_attribute() {
        // Mounting both `remote:/srv` and `remote:/srv/photos` from one rclone
        // process gives cache paths where one is a genuine prefix of the other. A
        // prefix match would let the parent absorb the child's transfers and report
        // a byte total silently too large.
        let child = Transfer {
            src_fs: Some(":local{x}:/c/vfs/local/srv/photos".into()),
            ..Default::default()
        };
        assert!(child.belongs_to_cache("/c/vfs/local/srv/photos"));
        assert!(
            !child.belongs_to_cache("/c/vfs/local/srv"),
            "the parent mount must not claim the child's transfer"
        );
    }

    #[test]
    fn backend_tag_is_stripped() {
        // The real shapes: `:<backend>{<base64url hash>}:`. The hash alphabet is
        // [A-Za-z0-9_-], so the closing `}:` is an unambiguous anchor.
        for src in [
            ":local{8un-i}:/c/vfs/http/data",
            ":http{_a-Z09}:/c/vfs/http/data",
        ] {
            let t = Transfer {
                src_fs: Some(src.into()),
                ..Default::default()
            };
            assert!(
                t.belongs_to_cache("/c/vfs/http/data"),
                "failed to strip tag from {src}"
            );
        }
    }

    #[test]
    fn pending_reports_what_it_cannot_measure() {
        let q: VfsQueue = serde_json::from_str(
            r#"{"queue":[{"name":"a","size":10},{"name":"b","size":-1},{"name":"c","size":-1}]}"#,
        )
        .unwrap();
        let p = q.pending();
        assert_eq!(p.files, 3);
        assert_eq!(p.known_bytes, 10);
        assert_eq!(p.unknown_size_files, 2);
        assert!(
            !p.is_exact(),
            "callers must be able to tell this is a floor, not a total — it gates \
             whether unmounting is safe"
        );
        assert!(!p.is_empty());

        let q: VfsQueue = serde_json::from_str(r#"{"queue":[{"name":"a","size":10}]}"#).unwrap();
        assert!(q.pending().is_exact());
        assert!(VfsQueue::default().pending().is_empty());
    }

    #[test]
    fn cache_attribution_respects_path_boundaries() {
        // The bug a substring test has: two mounts whose cache paths share a prefix
        // would each claim the other's transfers, so both show wrong byte totals.
        let backup = Transfer {
            src_fs: Some(":local{x}:/cache/vfs/local/srv/photos-backup".into()),
            ..Default::default()
        };
        assert!(
            !backup.belongs_to_cache("/cache/vfs/local/srv/photos"),
            "'photos' must not claim a transfer belonging to 'photos-backup'"
        );
        assert!(backup.belongs_to_cache("/cache/vfs/local/srv/photos-backup"));
    }

    #[test]
    fn untagged_src_fs_still_matches() {
        let t = Transfer {
            src_fs: Some("/cache/vfs/local/srv/data".into()),
            ..Default::default()
        };
        assert!(t.belongs_to_cache("/cache/vfs/local/srv/data"));
    }

    #[test]
    fn unknown_size_sentinel_does_not_abort_the_parse() {
        // rclone reports -1 for objects of unknown size. Failing here would blank
        // every mount's state over one odd transfer, not just this field.
        let t: Transfer = serde_json::from_str(r#"{"name":"a","size":-1}"#).unwrap();
        assert_eq!(t.size, -1);
        assert_eq!(t.known_size(), None);

        let t: Transfer = serde_json::from_str(r#"{"name":"a","size":42}"#).unwrap();
        assert_eq!(t.known_size(), Some(42));

        let s: CoreStats = serde_json::from_str(r#"{"totalBytes":-1}"#).unwrap();
        assert_eq!(s.total_bytes, -1);

        let q: VfsQueue =
            serde_json::from_str(r#"{"queue":[{"name":"a","size":-1},{"name":"b","size":10}]}"#)
                .unwrap();
        assert_eq!(
            q.pending_bytes(),
            10,
            "an unknown size contributes 0, never a negative"
        );
    }

    #[test]
    fn vfs_stats_survives_a_missing_bytes_used() {
        // path/pathMeta must keep parsing even if rclone drops bytesUsed, because
        // they are what the on-disk tier needs when rc cannot be trusted.
        let s: VfsStats =
            serde_json::from_str(r#"{"diskCache":{"path":"/p","pathMeta":"/m"}}"#).unwrap();
        let dc = s.disk_cache.expect("diskCache");
        assert_eq!(dc.path, "/p");
        assert_eq!(dc.path_meta, "/m");
    }

    #[test]
    fn conditional_core_stats_fields_are_modelled() {
        // No fixture carries these — see the provenance note on `last_error`. Their
        // shapes come from rclone's stats.go, so they are asserted here instead;
        // without this a rename or a type change would pass the whole suite.
        let s: CoreStats = serde_json::from_str(
            r#"{"errors":2,"lastError":"failed to upload: quota exceeded",
                "checking":["a/b.txt","c.txt"]}"#,
        )
        .unwrap();
        assert_eq!(s.errors, 2);
        assert_eq!(
            s.last_error.as_deref(),
            Some("failed to upload: quota exceeded")
        );
        assert_eq!(
            s.checking.as_deref(),
            Some(&["a/b.txt".to_string(), "c.txt".to_string()][..]),
            "checking is a list of remotes, not a count or a single string"
        );

        // Both absent is the common case and must stay distinguishable from empty.
        let quiet: CoreStats = serde_json::from_str(r#"{"errors":0}"#).unwrap();
        assert_eq!(quiet.last_error, None);
        assert_eq!(quiet.checking, None);
    }

    #[test]
    fn degenerate_cache_path_matches_nothing() {
        // Exercises the empty-path guard for real: if src_fs strips to empty too,
        // equality alone would match and every transfer would be claimed.
        let t = Transfer {
            src_fs: Some(":local{x}:".into()),
            ..Default::default()
        };
        assert!(
            !t.belongs_to_cache(""),
            "an unset cache path must claim nothing"
        );
        assert!(!t.belongs_to_cache("/"));
    }

    #[test]
    fn rc_list_feature_detection() {
        let l: RcList =
            serde_json::from_str(r#"{"commands":[{"Path":"vfs/queue","Title":"Queue info"}]}"#)
                .unwrap();
        assert!(l.has("vfs/queue"));
        assert!(!l.has("vfs/stats"));
    }

    #[test]
    fn no_auth_polarity_is_not_inverted() {
        // The wire field is NoAuth, and it means the opposite of "auth required".
        // Getting this backwards means reporting config/dump as unauthenticated.
        let l: RcList = serde_json::from_str(
            r#"{"commands":[
                 {"Path":"rc/noop","NoAuth":true},
                 {"Path":"config/dump","NoAuth":false}
               ]}"#,
        )
        .unwrap();
        let noop = &l.commands[0];
        let dump = &l.commands[1];
        assert!(noop.no_auth && !noop.auth_required());
        assert!(!dump.no_auth && dump.auth_required());
    }
}
