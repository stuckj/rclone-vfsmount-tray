//! Asking one mount what is outstanding, at a rate that matches what it is doing.
//!
//! The tier is resolved per mount rather than once for the process: one mount may answer
//! over rc while another was started by somebody else and can only be scanned on disk.

use rvt_core::capabilities::{Capabilities, Tier};
use rvt_core::config::CacheMode;
use rvt_core::models::{CoreStats, DiskCache, VfsQueue, VfsStats};
use rvt_core::rc::{RcClient, RcError};
use rvt_core::transfer::{RateEstimator, TransferState};
use std::time::Duration;

/// Poll interval while something is outstanding.
const ACTIVE: Duration = Duration::from_secs(1);
/// Poll interval while the queue is empty. An idle mount should cost nothing.
const IDLE: Duration = Duration::from_secs(15);

/// What `vfs/stats` was able to say about this mount's write-back cache.
///
/// [`Self::NotAsked`] and [`Self::Absent`] both yield no `DiskCache`, and collapsing them
/// into one `Option` makes "this rclone does not offer `vfs/stats`" indistinguishable from
/// "this mount has no cache" — which are a missing endpoint and a missing cache, and call
/// for opposite handling.
enum CacheProbe {
    /// This rclone does not register `vfs/stats`, so the cache was never asked about.
    NotAsked,
    /// Asked, and this mount has no write-back cache.
    Absent,
    /// A cache, and the mode the VFS is running with — `None` where rclone did not say.
    Present(DiskCache, Option<CacheMode>),
}

/// Polls one mount.
pub struct MountPoller {
    name: String,
    client: RcClient,
    caps: Capabilities,
    rate: RateEstimator,
    /// Cache path from `vfs/stats`, which is what attributes a `core/stats` transfer to
    /// this mount and what the on-disk tier would scan.
    cache_path: Option<String>,
}

impl MountPoller {
    /// Probe what this rclone supports. Cheap enough to redo when a mount is remounted,
    /// and necessary then: it is a different process with a different socket.
    ///
    /// Never fails. A probe that could not answer leaves the capability set empty, and
    /// `poll` re-probes and reports the mount unmonitored — which keeps it on screen with
    /// a reason attached. Returning an error here instead would drop the mount from the
    /// report altogether, the one outcome worse than saying "cannot tell".
    pub async fn connect(name: &str, client: RcClient) -> Self {
        let caps = Capabilities::probe(&client)
            .await
            .unwrap_or_else(|e| Capabilities::from_refusal(e.to_string()));
        Self {
            name: name.to_string(),
            client,
            caps,
            rate: RateEstimator::new(),
            cache_path: None,
        }
    }

    pub fn tier(&self) -> Tier {
        self.caps.tier()
    }

    /// How long to wait before polling again.
    ///
    /// Driven by whether anything is known to be outstanding, not by whether the mount is
    /// idle. A mount that cannot be observed has nothing to re-derive every second, and a
    /// partially observed one with real entries in its queue still has to be watched.
    pub fn interval(state: &TransferState) -> Duration {
        if state.pending.files > 0 {
            ACTIVE
        } else {
            IDLE
        }
    }

    /// One poll.
    ///
    /// Never fails for a mount that has gone away: an unreachable rclone is the case the
    /// on-disk tier exists for, so it degrades rather than erroring and says why. A real
    /// fault from a live rclone is returned.
    pub async fn poll(&mut self) -> Result<TransferState, RcError> {
        // An empty set issues no rc calls at all, so without re-probing here a mount that
        // was down at connect stays unobservable for the life of the service. Covers both
        // the service starting before its mount units and a unit restarting after a crash.
        if self.caps.is_empty() {
            if let Ok(caps) = Capabilities::probe(&self.client).await {
                self.caps = caps;
            }
        }
        if self.caps.is_empty() {
            // Say what actually happened. `probe` kept the reason; inventing one about
            // the rclone build sends the user to look in the wrong place entirely.
            return Ok(TransferState::unmonitored(
                &self.name,
                self.caps
                    .degraded_reason()
                    .unwrap_or("rclone reported no rc commands"),
            ));
        }

        // `vfs/stats` first: it carries the cache paths and the two actionable flags, and
        // it is the only source for either.
        let cache = match self.fetch_stats().await {
            Ok(c) => c,
            Err(e) if e.is_unreachable() => {
                self.rate.reset();
                return Ok(self.unreachable(&e));
            }
            Err(e) => return Err(e),
        };

        let mut state = match self.fetch_queue().await {
            // A cacheless VFS answers `{}` with HTTP 200, so the *absence* of the key —
            // not an empty list — is what says there is no queue. `from_queue` makes that
            // call and returns an unmonitored state; this is the one signal that does not
            // depend on `vfs/stats` being registered too.
            Ok(queue) => TransferState::from_queue(&self.name, &queue),
            Err(e) if e.is_unreachable() => {
                self.rate.reset();
                return Ok(self.unreachable(&e));
            }
            // No `vfs/queue` on this build: counts are all there is over rc.
            Err(RcError::Failed { status: 404, .. }) => match &cache {
                CacheProbe::Present(c, _) => TransferState::from_stats(&self.name, c),
                CacheProbe::Absent => {
                    return Ok(TransferState::unmonitored(
                        &self.name,
                        "this rclone registers no vfs/queue and this mount has no \
                         write-back cache, so nothing outstanding can be observed",
                    ))
                }
                CacheProbe::NotAsked => {
                    return Ok(TransferState::unmonitored(
                        &self.name,
                        "this rclone registers neither vfs/queue nor vfs/stats, so \
                         nothing outstanding can be observed over rc",
                    ))
                }
            },
            Err(e) => return Err(e),
        };

        // A queue that reported nothing to report is already fully described, and the
        // enrichment below would only overwrite the reason it carries.
        if !state.outstanding_known {
            self.rate.reset();
            return Ok(state);
        }

        match &cache {
            CacheProbe::Present(c, mode) => {
                state = state.with_cache_health(c);
                // `minimal` builds a cache and a queue, so nothing above this point can
                // tell it from `writes` — but a write-only open of an uncached file
                // streams past both, leaving its queue a floor rather than a total.
                //
                // Written to fail closed. An unrecognised mode reaches the same branch as
                // `minimal`: rclone renaming `opt.CacheMode` or encoding it as a string
                // would otherwise silently restore "an empty queue means idle" on every
                // mount, which is the claim that gets a file truncated.
                if !mode.is_some_and(CacheMode::all_writes_queued) {
                    state = state.partially_observed(match mode {
                        Some(_) => {
                            "this mount's cache mode is minimal, so a file opened \
                             write-only streams straight to the remote without entering \
                             the queue"
                        }
                        None => {
                            "this rclone did not report a recognised cache mode, so \
                             whether every write reaches the queue is unknown"
                        }
                    });
                }
            }
            // Reached only on a build with no `vfs/queue`; the absent queue key above
            // catches this first otherwise.
            CacheProbe::Absent => {
                return Ok(TransferState::unmonitored(
                    &self.name,
                    "this mount has no write-back cache (--vfs-cache-mode off), so writes \
                     stream straight to the remote and nothing outstanding can be observed",
                ))
            }
            // The queue answered, so its entries are real; only the health flags, the
            // cache path and the cache *mode* are missing. Discarding the entries would
            // throw away a working answer, but without the mode this cannot be told from
            // a `minimal` mount either — so the entries stand as a floor, same as one.
            CacheProbe::NotAsked => {
                state = state.partially_observed(
                    "this rclone registers no vfs/stats, so cache errors, a full cache \
                     and whether every write reaches the queue cannot be reported",
                )
            }
        }

        // Per-file progress, where the build has `core/stats` and the transfers can be
        // attributed to this mount. Without the cache path a transfer cannot be told from
        // another mount's, so the lift is skipped rather than guessed at.
        if let (true, Some(cache)) = (self.caps.has("core/stats"), self.cache_path.clone()) {
            match self.client.call::<CoreStats>("core/stats", empty()).await {
                Ok(stats) => {
                    // Both conditions, per DESIGN.md: the `global_stats` group alone admits
                    // cache *downloads*, which would then be counted as pending uploads.
                    let mine: Vec<_> = stats.writeback_uploads(&cache).cloned().collect();
                    state = state.with_progress(&mine);
                }
                // Losing per-file progress is a silent downgrade otherwise.
                Err(e) => tracing::debug!(mount = %self.name, error = %e,
                    "core/stats unavailable; reporting without per-file progress"),
            }
        }

        let rate = if state.is_idle() || !state.has_byte_total() {
            // Nothing moving, or no byte total to difference. `vfs/stats` reports counts
            // without sizes, so sampling it would yield a confident 0 B/s.
            self.rate.reset();
            None
        } else {
            // Discounting bytes already on the wire, so a large file's progress is
            // visible rather than only its departure from the queue.
            self.rate
                .sample(state.remaining_bytes())
                // Zero asserts "stalled". While something is in flight the truth is
                // "moving, not measurable at this granularity", which is `None`.
                .filter(|r| *r > 0 || state.uploading == Some(0))
        };
        Ok(state.with_rate(rate))
    }

    fn unreachable(&self, e: &RcError) -> TransferState {
        // Not an empty scan: that reports zero outstanding, exactly, which is a claim.
        // The on-disk scanner that could answer here is #22; until then the honest answer
        // is that we do not know.
        TransferState::unmonitored(&self.name, e.to_string())
    }

    async fn fetch_stats(&mut self) -> Result<CacheProbe, RcError> {
        if !self.caps.has("vfs/stats") {
            return Ok(CacheProbe::NotAsked);
        }
        let stats: VfsStats = self.client.call("vfs/stats", empty()).await?;
        let mode = stats.cache_mode();
        let Some(cache) = stats.disk_cache else {
            return Ok(CacheProbe::Absent);
        };
        if !cache.path.is_empty() {
            self.cache_path = Some(cache.path.clone());
        }
        Ok(CacheProbe::Present(cache, mode))
    }

    async fn fetch_queue(&self) -> Result<VfsQueue, RcError> {
        if !self.caps.has("vfs/queue") {
            return Err(RcError::Failed {
                command: "vfs/queue".into(),
                status: 404,
                body: "not registered by this rclone".into(),
            });
        }
        self.client.call("vfs/queue", empty()).await
    }
}

fn empty() -> serde_json::Value {
    serde_json::json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// The rc bodies to answer with, shared so a test can change one between polls.
    type Routes = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, String)>>>;

    fn routes(rs: Vec<(&'static str, String)>) -> Routes {
        std::sync::Arc::new(std::sync::Mutex::new(rs))
    }

    /// Answers a body per rc command path, so a poll can be driven end to end.
    struct FakeRc {
        dir: PathBuf,
        socket: PathBuf,
        routes: Routes,
        handle: tokio::task::JoinHandle<()>,
    }

    impl FakeRc {
        /// Replace one route's body. Anything a poll derives by differencing two polls is
        /// invisible to a server that answers identically every time.
        fn set(&self, path: &str, body: String) {
            let mut rs = self.routes.lock().unwrap();
            for r in rs.iter_mut() {
                if r.0 == path {
                    r.1 = body;
                    return;
                }
            }
            panic!("no such route: {path}");
        }

        async fn new(tag: &str, routes: Vec<(&'static str, String)>) -> Self {
            // The sequence number, not the tag, is what keeps two concurrent tests apart:
            // `new` opens by removing the directory, so a tag used twice has one test
            // deleting the other's live socket.
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("rvt-poll-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.join("rc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

            let routes = self::routes(routes);
            let served = routes.clone();
            let handle = tokio::spawn(async move { serve_routes(listener, served).await });
            Self {
                dir,
                socket,
                routes,
                handle,
            }
        }

        fn client(&self) -> RcClient {
            RcClient::new(&self.socket)
        }
    }

    /// Answer each request from the routing table; 404 for anything unlisted.
    async fn serve_routes(listener: tokio::net::UnixListener, routes: Routes) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = routes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(path, _)| req.starts_with(&format!("POST /{path} ")))
                    .map(|(_, b)| b.clone());
                let resp = match body {
                    Some(b) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                        b.len()
                    ),
                    None => {
                        let b = "command not found";
                        format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{b}", b.len())
                    }
                };
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    }

    impl Drop for FakeRc {
        fn drop(&mut self) {
            self.handle.abort();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../testdata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("captured fixture")
    }

    fn rc_list(commands: &[&str]) -> String {
        let cs: Vec<_> = commands
            .iter()
            .map(|c| serde_json::json!({ "Path": c }))
            .collect();
        serde_json::json!({ "commands": cs }).to_string()
    }

    #[tokio::test]
    async fn a_full_rclone_reports_the_queue_with_sizes_and_in_flight() {
        let fake = FakeRc::new(
            "full",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;

        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.expect("a live rclone is not an error");

        assert_eq!(s.fidelity, Some(Tier::T2));
        assert!(s.pending.known_bytes > 0, "the queue carries a byte total");
        assert_eq!(s.uploading, Some(1));
        assert!(s.degraded_reason.is_none());
    }

    /// A cache in trouble. Every capture in `testdata/` is healthy, so the two states the
    /// user has to act on cannot be served from one.
    fn unhealthy_stats() -> String {
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["diskCache"]["outOfSpace"] = serde_json::json!(true);
        v["diskCache"]["erroredFiles"] = serde_json::json!(3);
        v.to_string()
    }

    /// The captured `vfs/stats` re-stamped with a cache mode. rclone's own encoding:
    /// 0 off, 1 minimal, 2 writes, 3 full. The capture is a `writes` mount, so every
    /// other mode has to be synthesised.
    fn stats_with_cache_mode(mode: u64) -> String {
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["opt"]["CacheMode"] = serde_json::json!(mode);
        v.to_string()
    }

    #[tokio::test]
    async fn a_minimal_cache_mode_mount_is_never_reported_idle() {
        // `minimal` builds a cache and a queue, so `vfs/stats` and `vfs/queue` look exactly
        // like a `writes` mount — but a write-only open of an uncached file streams past
        // both. Measured on a live FUSE mount: 8s into a 20MB write, `vfs/queue` was `[]`,
        // `diskCache` was present with every counter zero, and the transfer appeared only
        // in `core/stats` with no `srcFs`, so the attribution filter drops it too.
        //
        // Reporting that as idle is what lets an unmount truncate the file being written.
        let fake = FakeRc::new(
            "minimal",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", stats_with_cache_mode(1)),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.expect("this is not a fault");

        assert!(
            !s.is_idle(),
            "an empty queue on a minimal mount is not proof the mount is idle"
        );
        assert!(!s.outstanding_known);
        let why = s.degraded_reason.clone().expect("the user has to be told");
        assert!(why.contains("minimal"), "{why}");
    }

    #[tokio::test]
    async fn a_cache_mode_rclone_did_not_name_is_not_assumed_safe() {
        // `opt.CacheMode` is an untyped ordinal in a block rclone reshapes between
        // releases. Giving `vfs.CacheMode` a `MarshalJSON` — it already has a `String()` —
        // would turn it into `"minimal"`, and every mount would parse as mode-unknown. If
        // unknown were read as "all writes are queued", that one upstream change would
        // silently restore the truncation bug on every minimal mount at once.
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["opt"]["CacheMode"] = serde_json::json!("minimal");

        let fake = FakeRc::new(
            "unknownmode",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", v.to_string()),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.unwrap();

        assert!(
            !s.outstanding_known,
            "an unparseable cache mode must fail closed, not open"
        );
        assert!(!s.is_idle());
        assert!(s.degraded_reason.is_some());
    }

    #[tokio::test]
    async fn a_writes_cache_mode_mount_is_still_trusted() {
        // The other side of the check above: `writes` queues every write, so an empty
        // queue there really does mean idle. Without this, "never trust the queue" would
        // pass the minimal test while making every healthy mount permanently unknown.
        let fake = FakeRc::new(
            "writes",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", stats_with_cache_mode(2)),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.unwrap();

        assert!(s.outstanding_known, "a writes mount queues everything");
        assert!(s.is_idle());
        assert!(s.degraded_reason.is_none());
    }

    #[tokio::test]
    async fn the_actionable_flags_come_from_stats_without_losing_the_queue_total() {
        let fake = FakeRc::new(
            "flags",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", unhealthy_stats()),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.unwrap();

        // `erroredFiles` and `outOfSpace` are only in vfs/stats, and the byte total is
        // only in vfs/queue. Both have to survive the merge. The flags are asserted true
        // here because both default to false: a healthy capture would pass this test
        // whether or not the merge happened at all.
        assert!(s.pending.known_bytes > 0, "the queue total survived");
        assert!(
            s.out_of_space,
            "the cache is full and the user must be told"
        );
        assert_eq!(s.errored_files, 3);
    }

    #[tokio::test]
    async fn a_build_without_vfs_stats_keeps_the_queue_answer() {
        let fake = FakeRc::new(
            "nostats",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue"])),
                ("vfs/queue", fixture("vfs-queue-two-items.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.expect("a missing command is not a fault");

        // Not asking `vfs/stats` yields no cache, exactly as a cacheless mount does. Only
        // the second is grounds for discarding the queue, and treating them alike throws
        // away an answer that arrived in full.
        assert_eq!(s.fidelity, Some(Tier::T2));
        assert_eq!(s.pending.files, 2, "the queue's entries survived");
        assert_eq!(s.pending.known_bytes, 393_216);
        assert_eq!(s.files.len(), 2);
        // They are a floor rather than a total, though: without `vfs/stats` there is no
        // cache mode either, so this cannot be told from a `minimal` mount.
        assert!(!s.outstanding_known);
        assert!(
            s.degraded_reason
                .as_deref()
                .is_some_and(|r| r.contains("vfs/stats")),
            "the reason must name the endpoint that is missing, got {:?}",
            s.degraded_reason
        );
    }

    #[tokio::test]
    async fn a_build_without_vfs_queue_falls_to_counts_rather_than_failing() {
        let fake = FakeRc::new(
            "noqueue",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/stats"])),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.expect("a missing command is not a fault");

        assert_eq!(s.fidelity, Some(Tier::T3));
        assert_eq!(
            s.pending.known_bytes, 0,
            "there is no honest byte total without the queue"
        );
        assert!(s.files.is_empty());
    }

    #[tokio::test]
    async fn an_rclone_that_has_gone_away_degrades_and_says_why() {
        // The case the on-disk tier exists for: reporting it as a fault would show the
        // user an error where a disk scan would still have an answer.
        let missing = std::env::temp_dir().join(format!("rvt-gone-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let client = RcClient::new(&missing);

        // `connect` degrades to no capabilities rather than erroring.
        let mut p = MountPoller::connect("backup", client).await;
        assert_eq!(p.tier(), Tier::T4);

        let s = p.poll().await.expect("an absent rclone is not a fault");
        assert!(!s.outstanding_known);
        // The capability tier is T4 — a disk scan could still answer. What produced *this*
        // state is nothing, and `Tier::T4.meets_the_bar()` is true, so naming any tier here
        // would tell a caller these figures are good enough to unmount on.
        assert_eq!(s.fidelity, None);
        assert!(
            !s.fidelity.is_some_and(Tier::meets_the_bar),
            "an unobserved mount must never claim it meets the bar"
        );
        assert!(
            s.degraded_reason.is_some(),
            "the UI has to be able to say why it lost precision"
        );
    }

    #[tokio::test]
    async fn polling_slows_down_when_nothing_is_outstanding() {
        let fake = FakeRc::new(
            "idle",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-queued-not-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-idle.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let busy = p.poll().await.unwrap();
        assert!(!busy.is_idle(), "the capture has a queued file");
        assert_eq!(MountPoller::interval(&busy), ACTIVE);

        // An empty queue rclone actually reported. `VfsQueue::default()` is not that: its
        // key is absent, which is how a cacheless VFS answers and is not idle at all.
        let reported_empty: VfsQueue = serde_json::from_str(r#"{"queue":[]}"#).unwrap();
        let empty = TransferState::from_queue("backup", &reported_empty);
        assert!(empty.is_idle());
        assert_eq!(
            MountPoller::interval(&empty),
            IDLE,
            "an idle mount must not be polled every second"
        );

        let unreported = TransferState::from_queue("backup", &VfsQueue::default());
        assert!(
            !unreported.is_idle(),
            "no queue key means no queue, which is the opposite of an empty one"
        );
    }

    /// Drive the poller against a real rclone, with a real VFS and a real write-back
    /// queue.
    ///
    /// `serve webdav` rather than `mount`: it builds the same VFS and the same write-back
    /// cache without needing FUSE, which is how the #9 spike was measured and which lets
    /// this run in containers where FUSE is unavailable. The mount-lifetime half is #38's
    /// job and genuinely does need FUSE.
    ///
    /// Skipped when rclone is absent, so the suite stays green on a bare runner.
    #[tokio::test]
    async fn a_live_rclone_reports_its_real_write_back_queue() {
        let Ok(rclone) = which_rclone() else {
            eprintln!("skipped: no rclone on PATH");
            return;
        };

        // Both cleanups run on unwind. Every assertion below is between the spawn and the
        // teardown, so a failing one would otherwise leave an rclone holding a listener
        // and an rc socket for as long as the machine stays up.
        let dir = TempDir(std::env::temp_dir().join(format!("rvt-live-{}", std::process::id())));
        let dir = &dir.0;
        let _ = std::fs::remove_dir_all(dir);
        let (src, cache) = (dir.join("src"), dir.join("cache"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");

        // A long write-back delay keeps the file in the queue for the whole test rather
        // than racing us to upload it.
        let mut child = tokio::process::Command::new(&rclone)
            .args([
                "serve",
                "webdav",
                &src.to_string_lossy(),
                "--addr",
                "127.0.0.1:0",
                "--vfs-cache-mode",
                "writes",
                "--vfs-write-back",
                "300s",
                "--cache-dir",
                &cache.to_string_lossy(),
                "--rc",
                "--rc-addr",
                &format!("unix://{}", socket.display()),
                "--rc-no-auth",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("rclone should start");

        // rclone binds the socket 0777 & ~umask; the client refuses anything looser, and
        // in production the service's runtime directory is what protects it.
        let mut ready = false;
        for _ in 0..60 {
            if socket.exists() {
                let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(ready, "rclone never created its rc socket");

        // The port is chosen by the kernel; rclone logs it. Reading it is what lets the
        // write go *through* the VFS — writing into the source directory directly would
        // bypass the write-back cache and leave the queue empty, which is exactly how
        // this test would quietly stop testing anything.
        let stderr = child.stderr.take().expect("piped");
        let port = read_served_port(stderr)
            .await
            .expect("rclone should log its address");

        let body = vec![b'x'; 512 * 1024];
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("webdav listener");
        {
            use tokio::io::AsyncWriteExt;
            let head = format!(
                "PUT /queued.bin HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();
            let mut buf = [0u8; 64];
            use tokio::io::AsyncReadExt;
            let _ = stream.read(&mut buf).await;
        }

        let mut p = MountPoller::connect("live", RcClient::new(&socket)).await;
        assert!(
            matches!(p.tier(), Tier::T1 | Tier::T2),
            "rclone 1.75 registers core/stats and vfs/queue, got {:?}",
            p.tier()
        );

        // The write-back delay is 300s, so the file must still be queued.
        let mut state = p.poll().await.expect("a live rclone is not a fault");
        for _ in 0..40 {
            if !state.is_idle() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            state = p.poll().await.expect("a live rclone is not a fault");
        }

        assert!(
            state.degraded_reason.is_none(),
            "a reachable rclone must not report as degraded: {:?}",
            state.degraded_reason
        );
        assert_eq!(state.mount, "live");
        assert!(
            state.pending.files >= 1,
            "the file written through the VFS should be queued for write-back, got {:?}",
            state.pending
        );
        assert!(
            state.pending.known_bytes >= body.len() as u64,
            "the queue should account for the bytes written: {:?}",
            state.pending
        );
        assert!(
            state.files.iter().any(|f| f.name == "queued.bin"),
            "the queued file should be named: {:?}",
            state.files.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert!(!state.out_of_space && state.errored_files == 0);

        let _ = child.kill().await;
    }

    /// Removes its directory however the test leaves the stack.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Read rclone's log until it announces the port it bound.
    async fn read_served_port(stderr: tokio::process::ChildStderr) -> Option<u16> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stderr).lines();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while let Ok(Some(line)) = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .unwrap_or(Ok(None))
        {
            if let Some(rest) = line.split("127.0.0.1:").nth(1) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = digits.parse() {
                    return Some(p);
                }
            }
        }
        None
    }

    fn which_rclone() -> Result<std::path::PathBuf, ()> {
        let path = std::env::var_os("PATH").ok_or(())?;
        std::env::split_paths(&path)
            .map(|d| d.join("rclone"))
            .find(|c| c.is_file())
            .ok_or(())
    }

    #[tokio::test]
    async fn a_stalled_looking_poll_reports_no_rate_rather_than_zero() {
        // Two polls of the same queue: nothing left it, but a file is in flight. Zero
        // asserts "stalled"; the truth is "moving, not measurable at this granularity".
        let fake = FakeRc::new(
            "rate",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let first = p.poll().await.unwrap();
        assert_eq!(
            first.rate_bytes_per_sec, None,
            "one sample cannot be a rate"
        );
        assert_eq!(first.uploading, Some(1), "the capture has a file in flight");

        let second = p.poll().await.unwrap();
        assert_eq!(
            second.rate_bytes_per_sec, None,
            "nothing left the queue and a file is uploading, so 0 B/s would read as \
             stalled when the transfer is in fact moving"
        );
    }

    #[tokio::test]
    async fn a_mount_with_no_write_back_cache_is_unmonitored_not_idle() {
        // `--vfs-cache-mode off` streams writes straight to the remote: rclone answers
        // `vfs/queue` with `{}` and omits `diskCache` entirely, so a naive read is a
        // confident zero. Verified against real rclone v1.75.0. This is also the one
        // configuration where an interrupted write is genuinely lost, since nothing on
        // disk is holding it.
        // `vfs/stats` is deliberately not registered here. The absent `queue` key is the
        // signal on its own; leaning on the absent `diskCache` would leave the safety
        // property resting on a second endpoint that a build need not provide.
        let fake = FakeRc::new(
            "cacheoff",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue"])),
                ("vfs/queue", "{}".to_string()),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await.expect("this is not a fault");

        assert!(
            !s.outstanding_known,
            "nothing can be observed here, so the figures must not be presented as fact"
        );
        assert!(
            !s.is_idle(),
            "an unobservable mount is not an idle one — an unmount check reads this"
        );
        let why = s
            .degraded_reason
            .clone()
            .expect("the user has to be told why");
        assert!(why.contains("vfs-cache-mode off"), "{why}");
        assert_eq!(
            MountPoller::interval(&s),
            IDLE,
            "re-deriving 'still cannot see it' every second is a busy loop over a fact \
             that cannot change until the mount is restarted"
        );
    }

    #[tokio::test]
    async fn the_rate_tracks_bytes_in_flight_not_departures_from_the_queue() {
        // One 128MB file. It stays in the queue for the whole upload, so the queue's byte
        // total does not move until the moment it finishes — a rate differenced from that
        // total reads 0 B/s for minutes and then spikes to 128MB in one interval. What
        // does move is `core/stats` progress, which is why the rate is differenced from
        // the queue total *minus* bytes already sent.
        let progressed = |bytes: u64| {
            let mut v: serde_json::Value =
                serde_json::from_str(&fixture("core-stats-vfs-upload-midflight.json")).unwrap();
            v["transferring"][0]["bytes"] = serde_json::json!(bytes);
            v.to_string()
        };
        let fake = FakeRc::new(
            "rate",
            vec![
                (
                    "rc/list",
                    rc_list(&["rc/list", "vfs/queue", "vfs/stats", "core/stats"]),
                ),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
                ("core/stats", progressed(20_000_000)),
            ],
        )
        .await;

        let mut p = MountPoller::connect("backup", fake.client()).await;
        let first = p.poll().await.unwrap();
        assert!(first.has_progress, "core/stats reached the state");
        assert_eq!(
            first.rate_bytes_per_sec, None,
            "one sample is not a rate yet"
        );

        // The queue is unchanged; only the bytes sent have grown.
        fake.set("core/stats", progressed(60_000_000));
        let second = p.poll().await.unwrap();
        assert_eq!(second.pending.known_bytes, first.pending.known_bytes);
        assert!(
            second.rate_bytes_per_sec.is_some_and(|r| r > 0),
            "40MB moved between polls, got {:?}",
            second.rate_bytes_per_sec
        );
    }

    #[tokio::test]
    async fn a_mount_that_starts_after_the_service_recovers() {
        // Capabilities are latched at connect. If rclone was not up then, the set is
        // empty — and with an empty set the poller issues no rc calls at all, so without
        // re-probing the mount reports as unobservable for the life of the service even
        // after rclone comes back. The service starting before its mount units, or a
        // mount unit restarting after a crash, both land here.
        let dir = std::env::temp_dir().join(format!("rvt-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");

        // Nothing listening yet.
        let mut p = MountPoller::connect("backup", RcClient::new(&socket)).await;
        let before = p.poll().await.unwrap();
        assert!(!before.outstanding_known, "nothing to see yet");

        // rclone comes up on the same socket.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let rs = routes(vec![
            ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
            ("vfs/queue", fixture("vfs-queue-uploading.json")),
            ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
        ]);
        let server = tokio::spawn(async move { serve_routes(listener, rs).await });

        let after = p.poll().await.expect("the mount is back");
        assert!(
            after.outstanding_known,
            "the poller must re-probe rather than stay blind for the session"
        );
        assert_eq!(after.fidelity, Some(Tier::T2));
        assert!(after.pending.files > 0);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_rclone_that_dies_after_connecting_does_not_report_an_exact_zero() {
        // The path that matters: capabilities were probed successfully, so the poller
        // knows this mount *has* a queue — and then rclone goes away. A mount with 12GB
        // queued must not flip to "nothing outstanding, exactly".
        //
        // Connecting to a socket that never existed takes a different branch (no
        // capabilities, so no cache), which is why that case cannot stand in for this one.
        let fake = FakeRc::new(
            "died",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let before = p.poll().await.unwrap();
        assert!(before.outstanding_known && !before.is_idle());

        // rclone exits: the socket is gone, the capabilities are not.
        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await.expect("a departed rclone is not a fault");
        assert!(
            !after.outstanding_known,
            "the queue did not empty — we simply stopped being able to see it"
        );
        assert!(
            !after.is_idle(),
            "'idle' is a claim we cannot make about a mount we cannot observe"
        );
        // `pending` itself cannot express "unknown" — zero files is vacuously exact, and
        // inventing an unknown-size file to say otherwise would be worse. That is what
        // `outstanding_known` is for, and why `is_idle()` consults it rather than the
        // count alone.
        assert_eq!(after.pending.files, 0);
        assert_eq!(after.rate_bytes_per_sec, None);
        assert!(after.degraded_reason.is_some());
    }
}
