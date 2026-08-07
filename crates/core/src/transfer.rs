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
    /// Which source produced this, and therefore what may be shown.
    pub fidelity: Tier,
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
        s.pending = Pending::new(cache.uploads_in_progress + cache.uploads_queued, 0, 0);
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

    /// Add `core/stats` byte progress to a queue-derived state, lifting it to T1.
    ///
    /// The queue stays the source of *what* is outstanding: `transferring[]` lags it by
    /// `--vfs-write-back`, so a file written seconds ago is in the queue and absent here.
    /// Only files present in both gain progress; the join key is the name.
    pub fn with_progress(mut self, transferring: &[Transfer]) -> Self {
        if self.fidelity != Tier::T2 {
            return self;
        }
        for f in &mut self.files {
            if let Some(t) = transferring.iter().find(|t| t.name == f.name) {
                f.bytes_sent = u64::try_from(t.bytes).ok();
            }
        }
        self.fidelity = Tier::T1;
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
    pub fn is_idle(&self) -> bool {
        self.pending.files == 0
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
    fn progress_lifts_the_queue_to_t1_and_only_for_files_in_both() {
        let s = TransferState::from_queue("backup", &queue_fixture("vfs-queue-uploading.json"));
        let known = s.files[0].name.clone();
        let transferring = vec![Transfer {
            name: known.clone(),
            size: 1_000,
            bytes: 400,
            ..Default::default()
        }];
        let lifted = s.with_progress(&transferring);

        assert_eq!(lifted.fidelity, Tier::T1);
        let hit = lifted.files.iter().find(|f| f.name == known).unwrap();
        assert_eq!(hit.bytes_sent, Some(400));
        // `transferring[]` lags `vfs/queue` by --vfs-write-back, so a queued file that has
        // not started must stay unmeasured rather than reading as 0% sent.
        for f in lifted.files.iter().filter(|f| f.name != known) {
            assert_eq!(f.bytes_sent, None, "{} was not transferring", f.name);
        }
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
