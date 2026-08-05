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

/// rclone's **default** accounting group — everything not running under an explicit
/// rc job.
///
/// Transfers started by an rc job (`operations/copyfile`, `sync/copy`, …) are grouped
/// as `job/<n>` instead, so this does separate VFS activity from rc jobs.
///
/// **It does not indicate direction.** VFS cache *downloads* — reading a file through
/// a `--vfs-cache-mode full` mount — are also ungrouped and also appear in
/// `core/stats` `transferring[]` with this group. Measured, not assumed: see
/// `testdata/core-stats-vfs-download-midflight.json`, captured while pulling a file
/// down with an empty upload queue.
///
/// Treating this group as "uploads" therefore reports a download as a pending upload,
/// which is wrong in the direction that matters — it inflates the outstanding-bytes
/// figure that decides whether unmounting is safe. Use
/// [`Transfer::is_writeback_upload`], which additionally requires the source to be the
/// VFS cache.
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
/// reports all of their transfers here together, in both directions. Use
/// [`CoreStats::writeback_uploads`] to narrow to one mount's pending uploads — a
/// group filter alone is not enough, because VFS downloads share the group.
///
/// `core/stats` also accepts a `group` parameter, which filters server-side and is
/// cheaper than filtering here; and `short: true`, which omits `transferring`
/// entirely when only the totals are wanted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreStats {
    /// Bytes transferred so far across all active transfers.
    #[serde(default)]
    pub bytes: i64,
    /// Total bytes expected across all active transfers.
    ///
    /// Signed because rclone's field is a Go `int64` and is computed by subtraction
    /// (`stats.go`), so a transient negative is representable. Note it does *not*
    /// propagate the `-1` unknown-size sentinel — `transfermap.go` filters those out
    /// before summing — so this is signed for range safety, not because `-1` is
    /// expected here. See [`Transfer::size`], where `-1` genuinely does appear.
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

    /// Text of the most recent error, present only when `errors > 0`.
    ///
    /// Without this, [`Self::errors`] can say *3* and never say why.
    ///
    /// **Provenance differs from the rest of this file.** Unlike every other field
    /// here, this and [`Self::checking`] are modelled from rclone's source
    /// (`fs/accounting/stats.go`) rather than from a capture: neither appeared in any
    /// response the investigation in #9 collected, and neither could be provoked
    /// afterwards — rc job errors are accounted to `job/<n>`, not to the global
    /// group. The shapes are asserted by unit tests rather than by a fixture.
    #[serde(default)]
    pub last_error: Option<String>,

    /// Files currently being checked, emitted only while a check is in progress.
    ///
    /// This *is* reachable from a mount: rclone accounts a rename as a "moving"
    /// check (`operations.Move` → `NewCheckingTransfer`), so renaming a file inside a
    /// watched mount emits it. Checks never appear in [`Self::transferring`] — the
    /// two collections are separate — so this cannot pollute upload accounting.
    ///
    /// See [`Self::last_error`] for provenance.
    #[serde(default)]
    pub checking: Option<Vec<String>>,

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

    /// Active write-back **uploads** out of the given VFS cache.
    ///
    /// `cache_path` is that mount's [`DiskCache::path`]. Both conditions are needed:
    /// the group excludes rc jobs, and the cache-path match excludes VFS *downloads*,
    /// which share the group. See [`GLOBAL_STATS_GROUP`].
    ///
    /// Narrowing by `group` can also be pushed to rclone by passing
    /// `group: "global_stats"` to `core/stats`; the direction check cannot, and must
    /// happen here.
    pub fn writeback_uploads<'a>(
        &'a self,
        cache_path: &'a str,
    ) -> impl Iterator<Item = &'a Transfer> {
        self.transfers_slice()
            .iter()
            .filter(move |t| t.is_writeback_upload(cache_path))
    }

    /// Active transfers not started by an rc job.
    ///
    /// This is a *group* filter only, so it includes VFS downloads as well as
    /// write-back uploads. Prefer [`Self::writeback_uploads`] unless you genuinely
    /// want both directions.
    pub fn ungrouped_transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers_slice().iter().filter(|t| t.is_ungrouped())
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
    /// Total size of the file in bytes, or `-1` when the size is not known.
    ///
    /// **Signed deliberately.** rclone carries this as a Go `int64` and uses `-1`
    /// for objects of unknown size. A `u64` here does not merely lose that value —
    /// `serde_json` aborts the *entire* response, so one unusual transfer would
    /// blank every mount's state rather than degrading a single number. Use
    /// [`Self::known_size`] to get it as an option.
    #[serde(default)]
    pub size: i64,
    /// Bytes uploaded so far.
    ///
    /// Signed for the same range-safety reason as [`Self::size`]: rclone's field is
    /// a Go `int64`, and a `u64` would abort the whole response rather than one
    /// field if it ever went negative.
    #[serde(default)]
    pub bytes: i64,
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
    /// Accounting group: [`GLOBAL_STATS_GROUP`] for anything not started by an rc job
    /// — write-back uploads **and** cache downloads alike — or `job/<n>` for an
    /// explicit rc job. It does not indicate direction; see [`GLOBAL_STATS_GROUP`].
    ///
    /// Absent until rclone attaches accounting to the transfer: this field, along with
    /// `bytes`, `percentage`, `speed`, `speedAvg` and `eta`, comes from the `Account`,
    /// not from the transfer itself.
    #[serde(default)]
    pub group: Option<String>,
    /// Source filesystem. For a VFS write-back upload this is the VFS **cache**
    /// directory (behind a backend tag), equal to [`DiskCache::path`] for the owning
    /// VFS — that is how a transfer is attributed to a mount, and how its direction is
    /// established. For a cache *download* it is the remote instead.
    #[serde(default)]
    pub src_fs: Option<String>,
    /// Destination filesystem — the remote being uploaded to.
    #[serde(default)]
    pub dst_fs: Option<String>,
}

impl Transfer {
    /// Whether this transfer was **not** started by an explicit rc job.
    ///
    /// This says nothing about direction — VFS downloads are ungrouped too. It is a
    /// building block for [`Self::is_writeback_upload`], not a filter to use alone.
    ///
    /// A transfer with no `group` at all counts as *not* ungrouped: showing an
    /// unrelated `rclone copy` as a mount's pending upload is worse than briefly
    /// omitting a real one.
    pub fn is_ungrouped(&self) -> bool {
        self.group.as_deref() == Some(GLOBAL_STATS_GROUP)
    }

    /// Whether this is a write-back **upload** out of the given VFS cache.
    ///
    /// Requires both that the transfer is ungrouped (not an rc job) and that its
    /// source is that mount's cache directory. The second condition is what
    /// establishes *direction*: for an upload rclone reports `srcFs` as the cache and
    /// `dstFs` as the remote, whereas for a cache download `srcFs` is the remote and
    /// `dstFs` is absent entirely.
    ///
    /// Without it, reading a large file through a `--vfs-cache-mode full` mount would
    /// be reported as a pending upload — inflating the outstanding-bytes figure that
    /// gates the unmount safety check, in the unsafe direction.
    pub fn is_writeback_upload(&self, cache_path: &str) -> bool {
        self.is_ungrouped() && self.belongs_to_cache(cache_path)
    }

    /// The file size, or `None` when rclone reported it as unknown (`-1`).
    pub fn known_size(&self) -> Option<u64> {
        u64::try_from(self.size).ok()
    }

    /// Whether this transfer originates from the given VFS cache directory.
    ///
    /// Pass [`DiskCache::path`] for the mount in question.
    ///
    /// For a VFS write-back upload, `src_fs` is that VFS's cache root with a backend
    /// tag prepended (`:local{8un-i}:`). Once the tag is stripped the two strings are
    /// **equal**, so this compares for equality rather than treating one as a prefix
    /// of the other.
    ///
    /// Equality is deliberate. Prefix matching is looser than the data requires and
    /// misattributes in two directions: siblings (`…/srv/photos` claiming
    /// `…/srv/photos-backup`) and nesting — mounting both `remote:/srv` and
    /// `remote:/srv/photos` from one rclone process gives cache paths where one is a
    /// genuine prefix of the other, so the parent would absorb the child's transfers
    /// and report a byte total that is silently too large.
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

/// Strip rclone's `:backend{hash}:` prefix from a filesystem string, if present.
///
/// `":local{8un-i}:/var/cache/x"` becomes `"/var/cache/x"`. Anything not matching
/// that shape is returned untouched, so a plain path passes through unchanged.
///
/// Splits on the closing `}:`, not the first `:`. The tag rclone emits is
/// `:<backend>{<hash>}:`, where the hash is `base64.RawURLEncoding` — an alphabet of
/// `[A-Za-z0-9_-]` that cannot contain `}` or `:` — so the closing `}:` is
/// unambiguous, and anchoring on it is robust to whatever appears in the backend
/// name.
///
/// (An earlier version of this comment claimed connection-string options with
/// embedded colons could appear inside the braces. They cannot: `fs.ConfigString`
/// emits the brace form and `ConfigStringFull` the comma form, never both.)
fn strip_backend_tag(fs: &str) -> &str {
    fs.strip_prefix(':')
        .and_then(|rest| rest.find("}:").map(|i| &rest[i + 2..]))
        .unwrap_or(fs)
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
    /// What is still waiting to upload, including how much of it cannot be measured.
    pub fn pending(&self) -> Pending {
        let mut p = Pending::default();
        for item in &self.queue {
            p.files += 1;
            match u64::try_from(item.size) {
                Ok(bytes) => p.known_bytes += bytes,
                Err(_) => p.unknown_size_files += 1,
            }
        }
        p
    }

    /// Bytes still to upload, counting only files whose size rclone reported.
    ///
    /// **This is a lower bound, not a total.** Files of unknown size (`-1`)
    /// contribute nothing. Prefer [`Self::pending`] anywhere the number is shown to
    /// a user or used to decide whether unmounting is safe — presenting an
    /// understated figure as a total is precisely the faked precision the design
    /// forbids.
    pub fn pending_bytes(&self) -> u64 {
        self.pending().known_bytes
    }

    /// Items rclone is actively uploading right now.
    pub fn uploading(&self) -> impl Iterator<Item = &QueueItem> {
        self.queue.iter().filter(|i| i.uploading)
    }
}

/// A summary of outstanding write-back uploads.
///
/// Carries the unmeasurable remainder explicitly so callers cannot mistake a lower
/// bound for a total. "3 files, 0 bytes" reads as harmless; "3 files, ≥ 0 bytes,
/// 3 of unknown size" does not.
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
    /// Path within the VFS. The join key to [`Transfer::name`].
    ///
    /// Required, like [`Transfer::name`] and [`RcCommand::path`]: it is the item's
    /// identity, and a queue entry that cannot be named cannot be displayed, joined
    /// or acted on. Every other field is defaulted.
    pub name: String,
    /// Opaque id, required by `vfs/queue-set-expiry` to force an upload.
    #[serde(default)]
    pub id: u64,
    /// File size in bytes, or `-1` when unknown. Signed for the same reason as
    /// [`Transfer::size`].
    #[serde(default)]
    pub size: i64,
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
    ///
    /// Defaulted like every sibling: without this, rclone dropping or renaming the
    /// field would fail the whole `vfs/stats` parse and take [`Self::path`] and
    /// [`Self::path_meta`] with it — the two values the on-disk scanning tier needs
    /// precisely when the rc API cannot be trusted.
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
    /// Command path, e.g. `vfs/queue`. Required — an entry without one has no
    /// identity and nothing downstream could use it.
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Title", default)]
    pub title: String,
    /// **Mind the polarity.** rclone's wire field is `NoAuth`, so `true` means the
    /// command is callable *without* authentication. Prefer [`Self::auth_required`]
    /// over reading this directly — inverted booleans get misread.
    ///
    /// There is no `AuthRequired` field in rclone. Modelling one gives a value that
    /// is always `false`, i.e. "no authentication needed" for `config/dump` and
    /// `core/command`, which is the wrong way for a fail-open default to fail.
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
