//! What is outstanding for a mount, and how much of that answer can be trusted.
//!
//! One type for the tray, D-Bus and GTK to consume whichever tier produced it. The tier
//! travels with the data, because the same struct means different things depending on
//! where it came from: see DESIGN.md's capability ladder.

use crate::capabilities::Tier;
use crate::models::{DiskCache, Pending, Transfer, VfsQueue};

/// One outstanding file.
///
/// Every field but `name` is optional, and absent means *this tier cannot say* rather
/// than zero. A caller that renders `None` as 0 turns "unknown" into a confident claim,
/// which is the failure the ladder exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransferFile {
    /// Path within the VFS.
    pub name: String,
    /// `None` when rclone reported the size as unknown.
    pub size: Option<u64>,
    /// Whether this file is uploading right now, as opposed to merely queued.
    pub in_flight: Option<bool>,
    /// Failed upload attempts. Climbing values mean repeated failure, not just slowness.
    pub tries: Option<u64>,
    /// Bytes already sent for this file. Only ever `Some` at T1.
    pub bytes_sent: Option<u64>,
}

/// What is outstanding for one mount.
///
/// Built through the per-tier constructors rather than by struct literal, so a tier
/// cannot populate a field it has no data for. `#[non_exhaustive]` enforces that from
/// outside the crate; the constructors are the only way in from inside it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TransferState {
    pub mount: String,
    /// Which source produced the *outstanding total*, and therefore what may be claimed
    /// about it. Deliberately not "the richest endpoint this rclone has": per-file
    /// progress is an enrichment on top, reported by [`Self::has_progress`], and folding
    /// the two together would label a `vfs/queue` total with a tier that cannot answer
    /// what the total is for.
    pub fidelity: Tier,
    /// Whether the figures below reflect reality at all.
    ///
    /// False when rclone could not be reached, and when the mount has no write-back cache
    /// to observe — a `--vfs-cache-mode off` mount streams writes straight to the remote,
    /// where nothing can see them. Zero outstanding then means "we cannot tell", not
    /// "nothing to send", and the difference decides whether unmounting is safe.
    pub outstanding_known: bool,
    /// Whether any file carries byte progress. Only `core/stats` supplies it.
    pub has_progress: bool,
    pub pending: Pending,
    /// Uploads in flight. `None` where the tier cannot distinguish queued from uploading.
    pub uploading: Option<u64>,
    /// Files whose upload has failed. Actionable.
    pub errored_files: u64,
    /// The cache is full. Actionable, and silently breaks uploads.
    pub out_of_space: bool,
    /// Derived by differencing totals across polls, never reported by rclone.
    pub rate_bytes_per_sec: Option<u64>,
    /// Empty where the tier reports counts but not files.
    pub files: Vec<TransferFile>,
    /// Set when this is a fallback rather than the answer that was asked for.
    pub degraded_reason: Option<String>,
}

impl TransferState {
    fn empty(mount: &str, fidelity: Tier) -> Self {
        Self {
            mount: mount.to_string(),
            fidelity,
            outstanding_known: true,
            has_progress: false,
            pending: Pending::default(),
            uploading: None,
            errored_files: 0,
            out_of_space: false,
            rate_bytes_per_sec: None,
            files: Vec::new(),
            degraded_reason: None,
        }
    }

    /// From `vfs/queue` — the minimum bar. Per-file sizes and an in-flight flag, no
    /// per-file byte progress.
    pub fn from_queue(mount: &str, queue: &VfsQueue) -> Self {
        let mut s = Self::empty(mount, Tier::T2);
        s.pending = queue.pending();
        s.uploading = Some(queue.queue.iter().filter(|i| i.uploading).count() as u64);
        s.files = queue
            .queue
            .iter()
            .map(|i| TransferFile {
                name: i.name.clone(),
                size: u64::try_from(i.size).ok(),
                in_flight: Some(i.uploading),
                tries: Some(i.tries),
                // `vfs/queue` says nothing about how far along a file is.
                bytes_sent: None,
            })
            .collect();
        s
    }

    /// From `vfs/stats` alone — counts, no byte total and no file list.
    ///
    /// Does not meet the bar on its own: `diskCache.bytesUsed` is not pending bytes, so
    /// there is no honest total to report here. Use it to enrich a queue-derived state,
    /// or as the signal that the cache is full.
    pub fn from_stats(mount: &str, cache: &DiskCache) -> Self {
        let mut s = Self::empty(mount, Tier::T3);
        s.uploading = Some(cache.uploads_in_progress);
        s.errored_files = cache.errored_files;
        s.out_of_space = cache.out_of_space;
        // Every size is unknown at this tier, which is what the third argument records.
        // `Pending::new(n, 0, 0)` would claim "n files totalling exactly zero bytes".
        let files = cache.uploads_in_progress + cache.uploads_queued;
        s.pending = Pending::new(files, 0, files);
        s
    }

    /// From the on-disk cache scan. Sizes and a total, but `Dirty` stays true until an
    /// upload completes, so queued and uploading are indistinguishable.
    pub fn from_scan(mount: &str, files: Vec<(String, u64)>) -> Self {
        let mut s = Self::empty(mount, Tier::T4);
        s.pending = Pending::new(files.len() as u64, files.iter().map(|(_, b)| b).sum(), 0);
        s.files = files
            .into_iter()
            .map(|(name, size)| TransferFile {
                name,
                size: Some(size),
                // The disk cannot tell these apart.
                in_flight: None,
                tries: None,
                bytes_sent: None,
            })
            .collect();
        s
    }

    /// A mount whose outstanding work cannot be observed at all.
    ///
    /// Two causes, both of which must read as "unknown" rather than "nothing": rclone is
    /// unreachable, or the mount has no write-back cache because its cache mode is `off`
    /// (or `minimal` for write-only opens). In the second case writes stream straight to
    /// the remote, so there is genuinely nothing on disk holding them — the one situation
    /// where an interrupted write really is lost, and the one where reporting "idle" is
    /// most harmful.
    pub fn unmonitored(mount: &str, reason: impl Into<String>) -> Self {
        let mut s = Self::empty(mount, Tier::T4);
        s.outstanding_known = false;
        s.degraded_reason = Some(reason.into());
        s
    }

    /// Add `core/stats` byte progress to a queue-derived state.
    ///
    /// The queue stays the source of *what* is outstanding: `transferring[]` lags it by
    /// `--vfs-write-back`, so a file written seconds ago is in the queue and absent here.
    /// Only files present in both gain progress; the join key is the name.
    pub fn with_progress(mut self, transferring: &[Transfer]) -> Self {
        if self.fidelity != Tier::T2 {
            return self;
        }
        let mut any = false;
        for f in &mut self.files {
            if let Some(t) = transferring.iter().find(|t| t.name == f.name) {
                f.bytes_sent = u64::try_from(t.bytes).ok();
                any |= f.bytes_sent.is_some();
            }
        }
        // `fidelity` stays T2: the queue produced the total, and the queue is what can
        // answer whether unmounting is safe. Promoting to T1 here would relabel an exact
        // total with the one tier that cannot vouch for it, and would claim per-file
        // progress for the whole `--vfs-write-back` window, during which `transferring[]`
        // is empty and no file has any.
        self.has_progress = any;
        self
    }

    /// Fold `vfs/stats`'s actionable flags into a state derived from another source.
    ///
    /// Deliberately does not touch `pending`: `vfs/stats` has no honest byte total, and
    /// overwriting a queue-derived one with its counts would lose the bytes.
    pub fn with_cache_health(mut self, cache: &DiskCache) -> Self {
        self.errored_files = cache.errored_files;
        self.out_of_space = cache.out_of_space;
        self
    }

    /// Record that this is a fallback and why, so the UI can say so rather than quietly
    /// losing precision when rc goes away mid-session.
    pub fn degraded(mut self, reason: impl Into<String>) -> Self {
        self.degraded_reason = Some(reason.into());
        self
    }

    pub fn with_rate(mut self, bytes_per_sec: Option<u64>) -> Self {
        self.rate_bytes_per_sec = bytes_per_sec;
        self
    }

    /// Whether anything is outstanding.
    ///
    /// False when the answer is unknown: a mount we cannot observe is not an idle one,
    /// and this is what an unmount check and the poll cadence both read.
    pub fn is_idle(&self) -> bool {
        self.outstanding_known && self.pending.files == 0
    }

    /// Whether the outstanding *byte* total means anything. `vfs/stats` reports counts
    /// with no sizes, so a rate derived from its total would be a confident zero.
    pub fn has_byte_total(&self) -> bool {
        self.outstanding_known && self.fidelity != Tier::T3
    }

    /// Bytes still to send, discounting what is already on the wire.
    ///
    /// Without this a rate derived by differencing the queue total reads zero for the
    /// whole upload of a large file — the total only moves when a file *leaves* the
    /// queue — and then spikes to the file's entire size in one interval.
    pub fn remaining_bytes(&self) -> u64 {
        let sent: u64 = self.files.iter().filter_map(|f| f.bytes_sent).sum();
        self.pending.known_bytes.saturating_sub(sent)
    }
}

/// Derives a transfer rate by differencing pending bytes across polls.
///
/// rclone reports no rate for write-back uploads, so it has to be inferred. Only
/// decreases count: a file being added to the queue raises the total without anything
/// having been sent, and treating that as negative throughput produces a rate that
/// swings wildly whenever someone is still writing.
#[derive(Debug, Clone, Default)]
pub struct RateEstimator {
    last: Option<(std::time::Instant, u64)>,
    smoothed: Option<f64>,
}

/// Weight of each new sample. Low, because a large file leaving the queue in one poll
/// otherwise reads as a burst several times the real throughput.
const SMOOTHING: f64 = 0.3;

impl RateEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the current pending total. Returns the smoothed rate, or `None` until two
    /// samples have been seen.
    pub fn sample(&mut self, pending_bytes: u64) -> Option<u64> {
        self.sample_at(std::time::Instant::now(), pending_bytes)
    }

    /// As [`sample`](Self::sample), with the clock supplied. Tests cannot wait out real
    /// poll intervals, and a rate estimator that is only exercised at whatever speed the
    /// test machine runs is not tested at all.
    pub fn sample_at(&mut self, now: std::time::Instant, pending_bytes: u64) -> Option<u64> {
        let previous = self.last.replace((now, pending_bytes));
        let (then, before) = previous?;
        let secs = now.saturating_duration_since(then).as_secs_f64();
        if secs <= 0.0 {
            return self.smoothed.map(|r| r as u64);
        }
        // Only a fall in the total means bytes left for the remote.
        let sent = before.saturating_sub(pending_bytes) as f64;
        let instant = sent / secs;
        let next = match self.smoothed {
            Some(prev) => prev * (1.0 - SMOOTHING) + instant * SMOOTHING,
            None => instant,
        };
        self.smoothed = Some(next);
        Some(next as u64)
    }

    /// Forget history — after an unmount, or when the queue empties, so the next upload
    /// does not inherit a stale rate.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn queue_fixture(name: &str) -> VfsQueue {
        let raw = std::fs::read_to_string(format!(
            "{}/../../testdata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("captured fixture");
        serde_json::from_str(&raw).expect("fixture should parse")
    }

    fn stats_fixture(name: &str) -> DiskCache {
        let raw = std::fs::read_to_string(format!(
            "{}/../../testdata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("captured fixture");
        let stats: crate::models::VfsStats = serde_json::from_str(&raw).expect("parses");
        stats.disk_cache.expect("the capture has a disk cache")
    }

    #[test]
    fn a_queue_gives_sizes_and_an_in_flight_flag_but_no_progress() {
        let s = TransferState::from_queue("backup", &queue_fixture("vfs-queue-uploading.json"));
        assert_eq!(s.fidelity, Tier::T2);
        assert!(!s.files.is_empty());
        assert!(s.files.iter().all(|f| f.in_flight.is_some()));
        assert!(
            s.files.iter().all(|f| f.bytes_sent.is_none()),
            "`vfs/queue` carries no byte progress, so nothing may claim it"
        );
        assert_eq!(s.uploading, Some(1), "the capture has one file in flight");
        assert!(
            s.files.iter().all(|f| f.size.is_some()),
            "the queue carries a size per file"
        );
        assert!(
            s.files.iter().all(|f| f.tries.is_some()),
            "#21 wants `tries` surfaced: a climbing value is repeated upload failure"
        );
    }

    #[test]
    fn stats_alone_reports_counts_and_no_file_list() {
        let s = TransferState::from_stats(
            "backup",
            &stats_fixture("vfs-stats-upload-in-progress.json"),
        );
        assert_eq!(s.fidelity, Tier::T3);
        assert!(
            s.files.is_empty(),
            "`vfs/stats` never names a file, so there is nothing to list"
        );
        assert_eq!(
            s.pending.known_bytes, 0,
            "`diskCache.bytesUsed` is not pending bytes; claiming a total here would be a lie"
        );
        assert!(s.pending.files > 0);
    }

    #[test]
    fn a_disk_scan_cannot_say_what_is_in_flight() {
        let s = TransferState::from_scan(
            "backup",
            vec![("a.bin".into(), 1024), ("b.bin".into(), 2048)],
        );
        assert_eq!(s.fidelity, Tier::T4);
        assert_eq!(s.pending.known_bytes, 3072);
        assert!(
            s.files.iter().all(|f| f.in_flight.is_none()),
            "`Dirty` stays true until an upload completes, so the disk cannot tell them apart"
        );
        assert_eq!(s.uploading, None);
    }

    #[test]
    fn progress_attaches_only_to_files_present_in_both_sources() {
        // Two entries, captured from a live rclone, because with one the "everything
        // else stays unmeasured" assertion never executes and the join key is unguarded.
        let q = queue_fixture("vfs-queue-two-items.json");
        assert!(q.queue.len() >= 2, "this test needs a multi-item queue");
        let s = TransferState::from_queue("backup", &q);
        let known = s.files[0].name.clone();
        let other = s.files[1].name.clone();

        let transferring = vec![Transfer {
            name: known.clone(),
            size: 1_000,
            bytes: 400,
            ..Default::default()
        }];
        let lifted = s.with_progress(&transferring);

        assert_eq!(
            lifted
                .files
                .iter()
                .find(|f| f.name == known)
                .unwrap()
                .bytes_sent,
            Some(400)
        );
        // `transferring[]` lags `vfs/queue` by --vfs-write-back, so a queued file that
        // has not started must stay unmeasured rather than reading as 0% sent — or, if
        // the join key were dropped, as the *other* file's byte count.
        assert_eq!(
            lifted
                .files
                .iter()
                .find(|f| f.name == other)
                .unwrap()
                .bytes_sent,
            None,
            "{other} was not transferring, so it has no progress to show"
        );
        assert!(lifted.has_progress, "one file did gain progress");
    }

    #[test]
    fn progress_does_not_relabel_the_source_of_the_total() {
        // The total came from `vfs/queue` and is exact. Promoting the state to T1 would
        // stamp it with the one tier that cannot vouch for an outstanding total, so an
        // unmount check would refuse to trust a figure that is in fact reliable.
        let q = queue_fixture("vfs-queue-two-items.json");
        let s = TransferState::from_queue("backup", &q);
        let bytes = s.pending.known_bytes;
        let lifted = s.with_progress(&[Transfer {
            name: "one.bin".into(),
            size: 262_144,
            bytes: 1_000,
            ..Default::default()
        }]);

        assert_eq!(lifted.fidelity, Tier::T2, "the queue produced the total");
        assert!(lifted.fidelity.meets_the_bar());
        assert_eq!(lifted.pending.known_bytes, bytes);
        assert!(lifted.has_progress);
    }

    #[test]
    fn a_queue_with_nothing_transferring_reports_no_progress() {
        // The whole --vfs-write-back window looks like this: files queued, none started.
        // Claiming per-file progress here is what draws a bar with no data behind it.
        let s = TransferState::from_queue("backup", &queue_fixture("vfs-queue-two-items.json"))
            .with_progress(&[]);
        assert!(!s.has_progress);
        assert!(s.files.iter().all(|f| f.bytes_sent.is_none()));
    }

    #[test]
    fn progress_does_not_promote_a_disk_scan() {
        // T4 has no queue behind it, so `transferring[]` cannot be joined to it and the
        // result would be a T1 claim over data that cannot support one.
        let s = TransferState::from_scan("backup", vec![("a.bin".into(), 10)]);
        let lifted = s.with_progress(&[Transfer {
            name: "a.bin".into(),
            size: 10,
            bytes: 5,
            ..Default::default()
        }]);
        assert_eq!(lifted.fidelity, Tier::T4);
        assert!(lifted.files.iter().all(|f| f.bytes_sent.is_none()));
    }

    /// A cache in trouble. Captured fixtures are all healthy, so the states a user must
    /// act on cannot be reached from `testdata/`.
    fn unhealthy_cache() -> DiskCache {
        let mut c = stats_fixture("vfs-stats-upload-in-progress.json");
        c.errored_files = 3;
        c.out_of_space = true;
        // Non-zero, so the "bytesUsed is not pending bytes" guard can actually detect the
        // mistake it is named for. rclone reports 0 here throughout an upload.
        c.bytes_used = 999_999;
        c
    }

    #[test]
    fn the_actionable_flags_survive_the_merge() {
        // `erroredFiles` and `outOfSpace` are the two states #21 says the user must act
        // on, and both are only in `vfs/stats`.
        let s = TransferState::from_queue("backup", &queue_fixture("vfs-queue-uploading.json"))
            .with_cache_health(&unhealthy_cache());
        assert_eq!(
            s.errored_files, 3,
            "repeated upload failure must reach the user"
        );
        assert!(s.out_of_space, "a full cache silently breaks uploads");
    }

    #[test]
    fn counts_only_never_borrows_a_byte_total_from_the_cache_size() {
        // `diskCache.bytesUsed` is cache size, not pending bytes — the one trap DESIGN
        // names for this endpoint.
        let s = TransferState::from_stats("backup", &unhealthy_cache());
        assert_eq!(
            s.pending.known_bytes, 0,
            "bytesUsed is not what is outstanding, however tempting the number is"
        );
        assert!(
            !s.pending.is_exact(),
            "counts with no sizes must not claim an exact zero-byte total"
        );
        assert_eq!(s.pending.unknown_size_files, s.pending.files);
        assert!(!s.has_byte_total());
    }

    #[test]
    fn cache_health_does_not_overwrite_the_byte_total() {
        let s = TransferState::from_queue("backup", &queue_fixture("vfs-queue-uploading.json"));
        let bytes = s.pending.known_bytes;
        assert!(bytes > 0);
        let merged = s.with_cache_health(&stats_fixture("vfs-stats-upload-in-progress.json"));
        assert_eq!(
            merged.pending.known_bytes, bytes,
            "`vfs/stats` has no byte total, so folding it in must not lose the queue's"
        );
        assert_eq!(merged.fidelity, Tier::T2);
    }

    #[test]
    fn degradation_is_recorded_rather_than_silent() {
        let s = TransferState::from_scan("backup", vec![]).degraded("rc socket is not private");
        assert!(s.degraded_reason.unwrap().contains("not private"));
    }

    #[test]
    fn a_rate_needs_two_samples_and_ignores_a_growing_queue() {
        let mut r = RateEstimator::new();
        let t0 = std::time::Instant::now();
        assert_eq!(r.sample_at(t0, 1_000), None, "one sample is not a rate");

        // 400 bytes left the queue in 2s.
        let rate = r.sample_at(t0 + Duration::from_secs(2), 600).unwrap();
        assert_eq!(rate, 200);

        // Someone writes more: the total rises. That is not negative throughput, and
        // treating it as such makes the figure swing wildly while a copy is in progress.
        let after_growth = r.sample_at(t0 + Duration::from_secs(4), 5_000).unwrap();
        assert!(
            after_growth < rate,
            "a growing queue should decay the rate toward zero, not invert it: {after_growth}"
        );
    }

    #[test]
    fn a_rate_is_smoothed_rather_than_tracking_one_poll() {
        // A large file completing in a single poll would otherwise read as a burst many
        // times the real throughput.
        let mut r = RateEstimator::new();
        let t0 = std::time::Instant::now();
        r.sample_at(t0, 10_000);
        let steady = r.sample_at(t0 + Duration::from_secs(1), 9_000).unwrap();
        let spike = r.sample_at(t0 + Duration::from_secs(2), 0).unwrap();
        assert!(
            spike < 9_000,
            "an unsmoothed estimator would report the full 9000 B/s burst, got {spike}"
        );
        assert!(
            spike > steady,
            "but it must still move toward the new sample"
        );
    }

    #[test]
    fn resetting_forgets_the_previous_upload() {
        let mut r = RateEstimator::new();
        let t0 = std::time::Instant::now();
        r.sample_at(t0, 1_000);
        r.sample_at(t0 + Duration::from_secs(1), 500);
        r.reset();
        assert_eq!(
            r.sample_at(t0 + Duration::from_secs(2), 400),
            None,
            "after a reset the next sample is a first sample again"
        );
    }
}
