//! What is outstanding for a mount, and how much of that answer can be trusted.
//!
//! One type for the tray, D-Bus and GTK to consume whichever tier produced it. The tier
//! travels with the data, because the same struct means different things depending on
//! where it came from: see DESIGN.md, "The capability ladder".

use crate::capabilities::Tier;
use crate::models::{DiskCache, Pending, Transfer, VfsQueue};
use crate::scan::CacheScan;

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
    /// Upload attempts, counted from the moment each one starts.
    ///
    /// On its own it cannot show a stuck file: 1 is both a healthy upload in flight and
    /// one that has already failed once and is backing off. Pair it with
    /// [`Self::in_flight`] — rclone sets `uploading` before it increments `tries`, so
    /// `tries >= 1` with `in_flight == Some(false)` is exactly "an attempt has been made
    /// and is not running now". `errored_files` stays 0 through all of it.
    pub tries: Option<u64>,
    /// Bytes already sent for this file. Supplied by `core/stats` alone, and only for the
    /// files it is currently transferring; see [`TransferState::has_progress`].
    pub bytes_sent: Option<u64>,
}

/// What is outstanding for one mount.
///
/// Built through the per-tier constructors rather than by struct literal, so a tier
/// cannot populate a field it has no data for.
///
/// `#[non_exhaustive]` only blocks *construction* by literal from outside the crate. The
/// fields are `pub`, so it does not stop a caller assigning to one afterwards, and the
/// combinations below are a convention the constructors keep rather than a guarantee the
/// type makes. Code that rebuilds a state from the wire — #40 — has to re-establish them
/// deliberately; `fidelity: None` with `outstanding_known: true` is nonsense the compiler
/// will not catch.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TransferState {
    pub mount: String,
    /// Which source produced the *outstanding total*, not "the richest endpoint this
    /// rclone has". Per-file progress is an enrichment on top, reported separately by
    /// [`Self::has_progress`].
    ///
    /// `None` when no source produced a total, so that a caller asking
    /// [`Tier::meets_the_bar`] cannot be told yes about figures nothing stands behind.
    pub fidelity: Option<Tier>,
    /// Whether the figures below are the whole story.
    ///
    /// False when rclone could not be reached; when the mount has no write-back cache at
    /// all, because a `--vfs-cache-mode off` mount streams writes straight to the remote
    /// where nothing can see them; and when only *some* writes reach the queue, which is
    /// `minimal` — there the entries present are real but a write-only open bypasses them.
    ///
    /// In every case a zero means "we cannot tell", not "nothing to send", and the
    /// difference decides whether unmounting is safe.
    ///
    /// `true` is still not a guarantee that nothing is being written — see
    /// [`Self::safe_to_unmount`] for the one case no rc endpoint can see.
    pub outstanding_known: bool,
    /// Whether any file carries byte progress. Only `core/stats` supplies it.
    pub has_progress: bool,
    pub pending: Pending,
    /// Uploads in flight. `None` where the tier cannot distinguish queued from uploading.
    pub uploading: Option<u64>,
    /// `vfs/stats`'s errored-file count, or `None` where `vfs/stats` could not be asked —
    /// a bare `0` there would report a full, failing cache as healthy.
    ///
    /// Not the whole picture even when present: a file rclone is still retrying leaves it
    /// at zero however many attempts have failed. See [`TransferFile::tries`].
    pub errored_files: Option<u64>,
    /// The cache is full. Actionable, and silently breaks uploads. `None` where
    /// `vfs/stats` could not be asked, for the same reason as above.
    pub out_of_space: Option<bool>,
    /// Derived by differencing totals across polls, never reported by rclone.
    pub rate_bytes_per_sec: Option<u64>,
    /// Empty where the tier reports counts but not files.
    pub files: Vec<TransferFile>,
    /// Set when this is a fallback rather than the answer that was asked for.
    pub degraded_reason: Option<String>,
}

impl TransferState {
    fn empty(mount: &str, fidelity: Option<Tier>) -> Self {
        Self {
            mount: mount.to_string(),
            fidelity,
            outstanding_known: true,
            has_progress: false,
            pending: Pending::default(),
            uploading: None,
            errored_files: None,
            out_of_space: None,
            rate_bytes_per_sec: None,
            files: Vec::new(),
            degraded_reason: None,
        }
    }

    /// From `vfs/queue` — the minimum bar. Per-file sizes and an in-flight flag, no
    /// per-file byte progress.
    ///
    /// [`Self::unmonitored`] when rclone reported no `queue` key at all, which is how a
    /// VFS with no write-back cache answers. The decision belongs here rather than at the
    /// call site: a `VfsQueue` whose key was absent is indistinguishable from an empty one
    /// once it reaches this function, and reading it as empty yields an exact,
    /// bar-meeting, safe-to-unmount zero for a mount whose writes nothing can see.
    pub fn from_queue(mount: &str, queue: &VfsQueue) -> Self {
        if !queue.reported_queue() {
            return Self::unmonitored(
                mount,
                "this mount has no write-back queue (--vfs-cache-mode off), so writes \
                 stream straight to the remote and nothing outstanding can be observed",
            );
        }
        let mut s = Self::empty(mount, Some(Tier::T2));
        s.pending = queue.pending();
        s.uploading = Some(queue.items().iter().filter(|i| i.uploading).count() as u64);
        s.files = queue
            .items()
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
        let mut s = Self::empty(mount, Some(Tier::T3));
        s.uploading = Some(cache.uploads_in_progress);
        s.errored_files = Some(cache.errored_files);
        s.out_of_space = Some(cache.out_of_space);
        // Every size is unknown at this tier, which is what the third argument records.
        // `Pending::new(n, 0, 0)` would claim "n files totalling exactly zero bytes".
        let files = cache.uploads_in_progress + cache.uploads_queued;
        s.pending = Pending::new(files, 0, files);
        s
    }

    /// From the on-disk cache scan. Sizes and a total, but `Dirty` stays true until an
    /// upload completes, so queued and uploading are indistinguishable.
    ///
    /// [`Self::outstanding_known`] needs [`CacheScan::is_complete`] *and*
    /// [`CacheScan::root_present`]: a walk that could not read an entry, stopped at its
    /// cap, or found no tree at all has not established that nothing else is waiting.
    ///
    /// [`TransferFile::in_flight`] stays `None`: `Dirty` is set from the write until the
    /// upload completes, so the disk cannot tell a queued file from one being sent.
    pub fn from_scan(mount: &str, found: &CacheScan) -> Self {
        let mut s = Self::empty(mount, Some(Tier::T4));
        // `root_present` as well as `is_complete`: an absent tree is completely read and
        // finds nothing, which is only an answer if the caller expected no cache. The type
        // decides rather than leaving it to whoever calls next.
        s.outstanding_known = found.is_complete() && found.root_present;
        s.pending = Pending::new(
            found.files.len() as u64,
            found.known_bytes(),
            found.files.iter().filter(|f| f.bytes.is_none()).count() as u64,
        );
        s.files = found
            .files
            .iter()
            .map(|f| TransferFile {
                name: f.name.clone(),
                size: f.bytes,
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
    /// Several causes, all of which must read as "unknown" rather than "nothing": rclone
    /// is unreachable, it answered with a fault, it registers neither endpoint, or the
    /// mount's cache mode is `off`. In that last case writes stream straight to the
    /// remote, so there is genuinely nothing on disk holding them — the one situation
    /// where an interrupted write really is lost, and the one where reporting "idle" is
    /// most harmful. A `minimal` mount, whose queue is real but incomplete, is
    /// [`Self::partially_observed`] instead.
    pub fn unmonitored(mount: &str, reason: impl Into<String>) -> Self {
        let mut s = Self::empty(mount, None);
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
        if self.fidelity != Some(Tier::T2) {
            return self;
        }
        let mut any = false;
        for f in &mut self.files {
            // `has_accounting` matters as much as the name: `bytes` defaults to 0, so a
            // transfer rclone has not yet attached accounting to would set `Some(0)` and
            // draw a 0% bar for a file that is in fact part-way sent.
            if let Some(t) = transferring
                .iter()
                .find(|t| t.name == f.name && t.has_accounting())
            {
                f.bytes_sent = u64::try_from(t.bytes).ok();
                any |= f.bytes_sent.is_some();
            }
        }
        // `fidelity` deliberately stays T2: the queue produced the total.
        self.has_progress = any;
        self
    }

    /// Fold `vfs/stats`'s actionable flags into a state derived from another source.
    ///
    /// Deliberately does not touch `pending`: `vfs/stats` has no honest byte total, and
    /// overwriting a queue-derived one with its counts would lose the bytes.
    pub fn with_cache_health(mut self, cache: &DiskCache) -> Self {
        self.errored_files = Some(cache.errored_files);
        self.out_of_space = Some(cache.out_of_space);
        self
    }

    /// Keep what was observed, but stop it being read as exhaustive.
    ///
    /// For a `minimal` mount: a read-write open goes through the write-back cache and
    /// appears in the queue, while a write-only open of an uncached file streams straight
    /// past it. The entries are real and worth showing; an empty queue still cannot be
    /// read as "nothing outstanding", which is the reading that calls a busy mount idle.
    pub fn partially_observed(mut self, reason: impl Into<String>) -> Self {
        self.outstanding_known = false;
        self.degraded_reason = Some(reason.into());
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

    /// Whether nothing *observable* is outstanding, and that is known rather than
    /// assumed.
    ///
    /// False when the answer is unknown: a mount we cannot observe is not an idle one.
    /// [`Self::safe_to_unmount`] is the name to reach for when that is the question, and
    /// carries the caveat that applies to both.
    pub fn is_idle(&self) -> bool {
        self.outstanding_known && self.pending.files == 0
    }

    /// Whether anything *observable* is still outstanding.
    ///
    /// The single answer to that much. [`Tier::meets_the_bar`] asks only whether a byte
    /// *total* can be trusted, and the two disagree: a T3 reading has no byte total yet
    /// its counts answer this perfectly well, while an unobserved mount has no counts
    /// worth anything at any tier. Reading the tier predicate here would refuse to ever
    /// offer unmount on a `vfs/stats`-only build.
    ///
    /// **Necessary, not sufficient.** rclone enqueues a file when it is closed, so a write
    /// still in progress is invisible to every rc endpoint and this returns `true` while
    /// it runs — measured in #73. Use it to decide what to *offer*: a
    /// mount it calls idle can still be refused by
    /// [`crate::supervisor::MountSupervisor::unmount`], which is where the kernel gets
    /// asked. #22 would let this see an open write itself.
    pub fn safe_to_unmount(&self) -> bool {
        self.is_idle()
    }

    /// Whether the outstanding *byte* total means anything. `vfs/stats` reports counts
    /// with no sizes, so a rate derived from its total would be a confident zero.
    ///
    /// Deliberately not conditioned on [`Self::outstanding_known`]: a partially observed
    /// mount's queue entries are real, only possibly incomplete, and differencing a floor
    /// is a sound rate. Requiring certainty here would leave a `minimal` mount showing
    /// "10 files, 5GB pending" and never a throughput for the whole transfer.
    pub fn has_byte_total(&self) -> bool {
        self.fidelity.is_some_and(|t| t != Tier::T3)
    }

    /// Bytes still to send, discounting what is already on the wire.
    ///
    /// Without this a rate derived by differencing the queue total reads zero for the
    /// whole upload of a large file — the total only moves when a file *leaves* the
    /// queue — and then spikes to the file's entire size in one interval.
    pub fn remaining_bytes(&self) -> u64 {
        // Only files of known size, because only those contributed to `known_bytes`.
        // Subtracting an unknown-size file's progress from a total it was never in
        // understates the remainder, and at small totals floors it to a false zero.
        let sent: u64 = self
            .files
            .iter()
            .filter(|f| f.size.is_some())
            .filter_map(|f| f.bytes_sent)
            .sum();
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

    /// A scan result, as `scan::scan` would return one. `None` is a file whose data file
    /// could not be measured — the scanner reports that rather than guessing zero.
    fn scanned(files: &[(&str, Option<u64>)]) -> CacheScan {
        CacheScan {
            files: files
                .iter()
                .map(|(name, bytes)| crate::scan::DirtyFile {
                    name: (*name).to_string(),
                    bytes: *bytes,
                })
                .collect(),
            unreadable: 0,
            truncated: false,
            root_present: true,
        }
    }

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
        assert_eq!(s.fidelity, Some(Tier::T2));
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
        assert_eq!(s.fidelity, Some(Tier::T3));
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
    fn a_scan_that_could_not_finish_looking_does_not_claim_a_total() {
        // The scanner counts what it could not read rather than folding it into "clean",
        // and that has to survive into the state: a walk that skipped an entry has not
        // established that nothing else is waiting, so its zero is not an answer.
        let mut partial = scanned(&[("a.bin", Some(10))]);
        partial.unreadable = 1;
        let s = TransferState::from_scan("backup", &partial);
        assert!(!s.outstanding_known);
        assert!(!s.is_idle(), "and it must not read as safe to unmount");

        let mut capped = scanned(&[]);
        capped.truncated = true;
        assert!(
            !TransferState::from_scan("backup", &capped).outstanding_known,
            "a walk stopped at its cap found no files because it stopped, not because \
             there are none"
        );

        let mut gone = scanned(&[]);
        gone.root_present = false;
        assert!(
            !TransferState::from_scan("backup", &gone).outstanding_known,
            "an absent tree is read completely and finds nothing, which is only an answer \
             if no cache was expected — the type must not decide that for the caller"
        );

        // The complete case still reports a total, or the tier would be useless.
        assert!(
            TransferState::from_scan("backup", &scanned(&[("a.bin", Some(10))])).outstanding_known
        );
    }

    #[test]
    fn a_scan_reports_a_file_it_could_not_measure_as_unknown_rather_than_zero() {
        // The descriptor and the data file are written separately, so a walk can land
        // between them. Counting the file but not its bytes is the honest reading.
        let s =
            TransferState::from_scan("backup", &scanned(&[("a.bin", Some(10)), ("b.bin", None)]));
        assert_eq!(s.pending.files, 2);
        assert_eq!(s.pending.known_bytes, 10);
        assert!(
            !s.pending.is_exact(),
            "one of the two sizes is unknown, so the total is a floor"
        );
    }

    #[test]
    fn a_disk_scan_cannot_say_what_is_in_flight() {
        let s = TransferState::from_scan(
            "backup",
            &scanned(&[("a.bin", Some(1024)), ("b.bin", Some(2048))]),
        );
        assert_eq!(s.fidelity, Some(Tier::T4));
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
        assert!(q.items().len() >= 2, "this test needs a multi-item queue");
        let s = TransferState::from_queue("backup", &q);
        let known = s.files[0].name.clone();
        let other = s.files[1].name.clone();

        let transferring = vec![Transfer {
            name: known.clone(),
            size: 1_000,
            bytes: 400,
            group: Some(crate::models::GLOBAL_STATS_GROUP.into()),
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
            group: Some(crate::models::GLOBAL_STATS_GROUP.into()),
            ..Default::default()
        }]);

        assert_eq!(
            lifted.fidelity,
            Some(Tier::T2),
            "the queue produced the total"
        );
        assert!(lifted.fidelity.is_some_and(Tier::meets_the_bar));
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
    fn a_transfer_without_accounting_yet_is_not_progress() {
        // rclone lists a transfer before attaching accounting to it: no `group`, and no
        // `bytes` on the wire. `bytes` is `#[serde(default)]`, so it arrives as 0 —
        // indistinguishable from "nothing sent yet" unless the group is checked. The file
        // really is mid-flight, so a 0% bar there is a claim rather than a reading.
        let t: Transfer = serde_json::from_str(r#"{"name":"one.bin","size":262144}"#).unwrap();
        assert!(!t.has_accounting());
        assert_eq!(t.bytes, 0);

        let s = TransferState::from_queue("backup", &queue_fixture("vfs-queue-two-items.json"))
            .with_progress(&[t]);
        assert!(
            !s.has_progress,
            "0 bytes from an unaccounted transfer is not progress"
        );
        assert!(s.files.iter().all(|f| f.bytes_sent.is_none()));
    }

    #[test]
    fn progress_on_an_unsized_file_does_not_eat_into_the_known_total() {
        // rclone reports `-1` for a size it does not know, so that file contributes
        // nothing to `known_bytes`. Subtracting its progress from a total it was never
        // part of understates the remainder — here to 300 rather than 600 — and a rate
        // differenced from that reads as throughput nobody achieved.
        let q: VfsQueue = serde_json::from_str(
            r#"{"queue":[{"name":"sized.bin","size":1000,"uploading":true},
                         {"name":"unsized.bin","size":-1,"uploading":true}]}"#,
        )
        .unwrap();
        let s = TransferState::from_queue("backup", &q);
        assert_eq!(s.pending.known_bytes, 1000, "only the sized file counts");

        let lifted = s.with_progress(&[
            Transfer {
                name: "sized.bin".into(),
                bytes: 400,
                group: Some(crate::models::GLOBAL_STATS_GROUP.into()),
                ..Default::default()
            },
            Transfer {
                name: "unsized.bin".into(),
                bytes: 300,
                group: Some(crate::models::GLOBAL_STATS_GROUP.into()),
                ..Default::default()
            },
        ]);
        assert_eq!(lifted.remaining_bytes(), 600);
    }

    #[test]
    fn progress_does_not_promote_a_disk_scan() {
        // T4 has no queue behind it, so `transferring[]` cannot be joined to it and the
        // result would be a T1 claim over data that cannot support one.
        let s = TransferState::from_scan("backup", &scanned(&[("a.bin", Some(10))]));
        let lifted = s.with_progress(&[Transfer {
            name: "a.bin".into(),
            size: 10,
            bytes: 5,
            group: Some(crate::models::GLOBAL_STATS_GROUP.into()),
            ..Default::default()
        }]);
        assert_eq!(lifted.fidelity, Some(Tier::T4));
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
        let queued =
            TransferState::from_queue("backup", &queue_fixture("vfs-queue-uploading.json"));
        // Before the merge neither is known, and neither may read as "fine": a bare 0 and
        // false would report a full, failing cache as healthy on any build that cannot
        // answer `vfs/stats`.
        assert_eq!(queued.errored_files, None);
        assert_eq!(queued.out_of_space, None);

        let s = queued.with_cache_health(&unhealthy_cache());
        assert_eq!(
            s.errored_files,
            Some(3),
            "repeated upload failure must reach the user"
        );
        assert_eq!(
            s.out_of_space,
            Some(true),
            "a full cache silently breaks uploads"
        );
    }

    #[test]
    fn counts_only_never_borrows_a_byte_total_from_the_cache_size() {
        // `diskCache.bytesUsed` is cache size, not pending bytes (#9).
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
        assert_eq!(merged.fidelity, Some(Tier::T2));
    }

    #[test]
    fn degradation_is_recorded_rather_than_silent() {
        let s =
            TransferState::from_scan("backup", &scanned(&[])).degraded("rc socket is not private");
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

        // The reported figure alone does not prove the clamp: dropping `saturating_sub`
        // for a signed subtraction still reports 0 here, because `f64 as u64` saturates.
        // What it leaves behind is a negative *smoothed* value, which only shows up on
        // the next poll — 1000 bytes really moving must read as throughput, not a stall.
        let recovered = r.sample_at(t0 + Duration::from_secs(5), 4_000).unwrap();
        assert!(
            recovered > 0,
            "a real transfer after a queue grew must not read as stalled: {recovered}"
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
