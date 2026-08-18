//! Asking one mount what is outstanding, at a rate that matches what it is doing.
//!
//! The tier is resolved per mount rather than once for the process: one mount may answer
//! over rc while another was started by somebody else and can only be scanned on disk.

use rvt_core::capabilities::{Capabilities, Tier};
use rvt_core::config::CacheMode;
use rvt_core::models::{CoreStats, DiskCache, VfsQueue, VfsStats};
use rvt_core::rc::{RcClient, RcError};
use rvt_core::transfer::{RateEstimator, TransferState};
use std::fmt::Write as _;
use std::time::Duration;

use rvt_core::config::Poll;

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
    /// this mount and what the on-disk tier scans for sizes.
    cache_path: Option<String>,
    /// `vfs/stats` `diskCache.pathMeta`. Latched for the same reason as the path above:
    /// the on-disk tier exists to answer once rclone can no longer be asked, so the roots
    /// have to be remembered from when it could.
    meta_path: Option<String>,
    /// The cache mode from the last `vfs/stats` that answered.
    ///
    /// Latched alongside the roots because the disk fallback needs the same qualification
    /// the live path applies: a `minimal` mount's writes can stream past the cache
    /// entirely, so a scan finding nothing has not established that nothing is in flight.
    mode: Option<CacheMode>,
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
            meta_path: None,
            mode: None,
        }
    }

    pub fn tier(&self) -> Tier {
        self.caps.tier()
    }

    /// Whether rclone answered the capability probe at all.
    ///
    /// [`Self::tier`] is `T4` both for an rclone that offers nothing over rc and for one
    /// that could not be reached, since the disk scan is what is left in either case. Only
    /// the first of those is a statement about what this rclone *supports*, so anything
    /// reporting a capability rather than a reading has to tell them apart.
    pub fn rc_answered(&self) -> bool {
        !self.caps.is_empty()
    }

    /// How long to wait before polling again.
    ///
    /// Driven by whether anything is known to be outstanding, not by whether the mount is
    /// idle. A mount that cannot be observed has nothing to re-derive every second, and a
    /// partially observed one with real entries in its queue still has to be watched.
    ///
    /// A disk-derived (T4) state is always the slow cadence however much it found: each
    /// poll behind it is a full walk of the metadata tree.
    ///
    /// Neither cadence can be zero: `Config::validate` refuses that, because a mount with
    /// something outstanding would then be polled in a loop with no wait at all.
    pub fn interval(state: &TransferState, poll: &Poll) -> Duration {
        let idle = Duration::from_secs(poll.idle_secs);
        if state.fidelity == Some(Tier::T4) {
            return idle;
        }
        if state.pending.files > 0 {
            Duration::from_secs(poll.active_secs)
        } else {
            idle
        }
    }

    /// One poll.
    ///
    /// Infallible, for the reason [`Self::connect`] is: a mount that produces no state at
    /// all vanishes from the report, which is worse than one that says "cannot tell" and
    /// why. An unreachable rclone is the case the on-disk tier exists for; a live rclone
    /// answering with a fault is rarer but no more useful to hide.
    pub async fn poll(&mut self) -> TransferState {
        // An empty set issues no rc calls at all, so without re-probing here a mount that
        // was down at connect stays unobservable for the life of the service. Covers both
        // the service starting before its mount units and a unit restarting after a crash.
        if self.caps.is_empty() {
            // Keep whatever the re-probe learned, including a *new* refusal: `probe`
            // returns `Ok` only for unreachable-class errors, so dropping the `Err` would
            // leave the connect-time reason — "no rc socket at …" — on a mount whose
            // socket is now present and answering with a fault.
            self.caps = match Capabilities::probe(&self.client).await {
                Ok(caps) => caps,
                Err(e) => Capabilities::from_refusal(e.to_string()),
            };
        }
        if self.caps.is_empty() {
            // Say what actually happened. `probe` kept the reason; inventing one about
            // the rclone build sends the user to look in the wrong place entirely.
            return TransferState::unmonitored(
                &self.name,
                self.caps
                    .degraded_reason()
                    .unwrap_or("rclone reported no rc commands"),
            );
        }

        // `vfs/stats` first: it carries the cache paths and the two actionable flags, and
        // it is the only source for either.
        let cache = match self.fetch_stats().await {
            Ok(c) => c,
            Err(e) if e.is_unreachable() => {
                self.rate.reset();
                return self.unreachable(&e).await;
            }
            Err(e) => return self.faulted(&e),
        };

        let mut state = match self.fetch_queue().await {
            // A cacheless VFS answers `{}` with HTTP 200, so the *absence* of the key —
            // not an empty list — is what says there is no queue. `from_queue` makes that
            // call and returns an unmonitored state; this is the one signal that does not
            // depend on `vfs/stats` being registered too.
            Ok(queue) => TransferState::from_queue(&self.name, &queue),
            Err(e) if e.is_unreachable() => {
                self.rate.reset();
                return self.unreachable(&e).await;
            }
            // No `vfs/queue` on this build: counts are all there is over rc.
            Err(RcError::Failed { status: 404, .. }) => match &cache {
                CacheProbe::Present(c, _) => TransferState::from_stats(&self.name, c),
                CacheProbe::Absent => {
                    return TransferState::unmonitored(
                        &self.name,
                        "this rclone registers no vfs/queue and this mount has no \
                         write-back cache, so nothing outstanding can be observed",
                    )
                }
                CacheProbe::NotAsked => {
                    return TransferState::unmonitored(
                        &self.name,
                        "this rclone registers neither vfs/queue nor vfs/stats, so \
                         nothing outstanding can be observed over rc",
                    )
                }
            },
            Err(e) => return self.faulted(&e),
        };

        // Reached only when `vfs/queue` omitted the key, which is how a cacheless VFS
        // answers. If `vfs/stats` contradicts that by reporting a cache, believe the
        // endpoint that answered — as the mirror case below does.
        if !state.outstanding_known {
            self.rate.reset();
            return match &cache {
                CacheProbe::Present(c, _) => TransferState::from_stats(&self.name, c)
                    .partially_observed(
                        "this rclone reports a write-back cache but no queue, so only its \
                         counts can be read",
                    ),
                _ => state,
            };
        }

        match &cache {
            CacheProbe::Present(c, mode) => {
                state = state.with_cache_health(c);
                // `minimal` is indistinguishable from `writes` at every endpoint but
                // this one, and a write-only open streams past its queue. Fails closed:
                // an unrecognised mode takes the same branch, so renaming `opt.CacheMode`
                // upstream cannot silently restore "an empty queue means idle".
                if !mode.is_some_and(CacheMode::all_writes_queued) {
                    state = state.partially_observed(match mode {
                        Some(CacheMode::Minimal) => {
                            "this mount's cache mode is minimal, so a file opened \
                             write-only streams straight to the remote without entering \
                             the queue"
                        }
                        Some(_) => {
                            "this mount's cache mode does not put every write through the \
                             queue, so some may not be visible here"
                        }
                        None => {
                            "this rclone did not report a recognised cache mode, so \
                             whether every write reaches the queue is unknown"
                        }
                    });
                }
            }
            // `vfs/queue` answered with a key and `vfs/stats` reports no cache — a
            // contradiction rclone 1.75 does not produce, since a cacheless VFS omits
            // both. If some build does, the queue is the one that answered, so its
            // entries stand as a floor rather than being thrown away.
            CacheProbe::Absent => {
                state = state.partially_observed(
                    "this rclone reports a write-back queue but no cache to hold it, so \
                     what is queued is a floor rather than the whole story",
                )
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

        // Per-file progress, where the build has `core/stats`, the transfers can be
        // attributed to this mount, and there is a queue-derived state to attach them to.
        // Without the cache path a transfer cannot be told from another mount's, so the
        // lift is skipped rather than guessed at; without T2 `with_progress` discards the
        // answer, so asking for it is a wasted round-trip every second.
        let liftable = self.caps.has("core/stats") && state.fidelity == Some(Tier::T2);
        if let (true, Some(cache)) = (liftable, self.cache_path.clone()) {
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

        let rate = if state.pending.files == 0 || !state.has_byte_total() {
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
        state.with_rate(rate)
    }

    /// A live rclone that answered with a fault. Rare, and still not a reason to make the
    /// mount disappear — the text is the only clue the user gets.
    fn faulted(&mut self, e: &RcError) -> TransferState {
        self.rate.reset();
        TransferState::unmonitored(&self.name, e.to_string())
    }

    /// rclone cannot be asked. Fall to the disk, which still has the answer.
    ///
    /// The case this tier exists for: dirty items outlive the process that queued them, so
    /// a crashed or restarting rclone makes its backlog unaskable rather than unknowable.
    /// The roots were latched from a poll that succeeded, so a mount that never answered
    /// has nowhere to look and stays unmonitored — an empty scan there would be a claim.
    ///
    /// A scan that fails is not folded into the rc failure. "rclone is unreachable" alone
    /// would hide that the fallback was tried and could not read the cache either, which
    /// is a different thing to go and look at.
    async fn unreachable(&self, e: &RcError) -> TransferState {
        let (Some(meta), Some(data)) = (self.meta_path.clone(), self.cache_path.clone()) else {
            return TransferState::unmonitored(&self.name, e.to_string());
        };

        let scanned = tokio::task::spawn_blocking(move || {
            rvt_core::scan::scan(std::path::Path::new(&meta), std::path::Path::new(&data))
        })
        .await;

        match scanned {
            // The tree `vfs/stats` told us about is gone. A cache directory disappearing
            // is not evidence the queue drained, so this is not an empty scan.
            Ok(Ok(found)) if !found.root_present => TransferState::unmonitored(
                &self.name,
                format!("{e}; and the cache it reported is no longer on disk"),
            ),
            Ok(Ok(found)) => {
                // One message, built once. `degraded` and `partially_observed` both own
                // `degraded_reason`, so layering them drops whichever ran first — and the
                // one that would be lost is the only mention that a disk scan happened at
                // all.
                let mut why = format!(
                    "{e}; read {} pending file(s) from the cache on disk instead",
                    found.files.len()
                );
                if found.unreadable > 0 {
                    // Otherwise the state reads not-known with nothing in the text to
                    // explain it, and the rc failure takes the blame for the cache being
                    // half-read.
                    let _ = write!(
                        why,
                        "; {} entries there could not be read",
                        found.unreadable
                    );
                }
                if found.truncated {
                    let _ = write!(why, "; and the cache was too large to finish walking");
                }

                // The same rule the live path applies, and for the same reason: under
                // `minimal` a write-only open never enters the cache — which is what this
                // scan reads — so finding nothing has not established that nothing is in
                // flight. An unrecognised or unseen mode fails closed here too.
                let all_queued = self.mode.is_some_and(CacheMode::all_writes_queued);
                if !all_queued {
                    // Three cases, as the live path distinguishes them: an unknown mode is
                    // not the same claim as a known one that streams past the cache, and
                    // saying so anyway states a fact about the mount nothing measured.
                    let _ = match self.mode {
                        Some(_) => write!(
                            why,
                            "; and this mount's cache mode does not put every write through \
                             the cache, so what is on disk is a floor"
                        ),
                        None => write!(
                            why,
                            "; and this rclone never reported a recognised cache mode, so \
                             whether every write reaches the cache is unknown"
                        ),
                    };
                }

                let state = TransferState::from_scan(&self.name, &found);
                if all_queued {
                    state.degraded(why)
                } else {
                    state.partially_observed(why)
                }
            }
            Ok(Err(scan_err)) => TransferState::unmonitored(
                &self.name,
                format!("{e}; and its cache could not be read either: {scan_err}"),
            ),
            Err(join) => TransferState::unmonitored(&self.name, format!("{e}; {join}")),
        }
    }

    /// Drop the cache roots a previous rclone named.
    ///
    /// They describe whichever rclone is behind the socket now, so any *answer* that does
    /// not name a usable pair invalidates them: scanning the old tree would report another
    /// instance's emptiness as this mount's. A silence says nothing, which is why the
    /// unreachable path does not come through here — there the roots are all that is left.
    ///
    /// The mode is separate: it is whatever the last `vfs/stats` that answered reported,
    /// and it is cleared only when none did.
    fn forget_roots(&mut self) {
        self.cache_path = None;
        self.meta_path = None;
    }

    async fn fetch_stats(&mut self) -> Result<CacheProbe, RcError> {
        if !self.caps.has("vfs/stats") {
            return Ok(CacheProbe::NotAsked);
        }
        let stats: VfsStats = match self.client.call("vfs/stats", empty()).await {
            Ok(s) => s,
            // Capabilities were latched at connect. If the rclone behind this socket has
            // been replaced by one that does not register `vfs/stats`, faulting here would
            // discard the T2 answer `vfs/queue` can still give — the same 404 is already
            // handled that way for the queue.
            // An answer, not a silence: this rclone does not have the endpoint, so
            // whatever a previous one said about its cache no longer describes the mount.
            // Keeping the roots here lets the fallback scan a tree that is not this
            // mount's and report its emptiness as an answer.
            Err(RcError::Failed { status: 404, .. }) => {
                // An answer, not a silence: this rclone does not have the endpoint, so
                // nothing it says describes the cache a previous one named — including
                // the mode, which no answering `vfs/stats` has now reported.
                self.forget_roots();
                self.mode = None;
                return Ok(CacheProbe::NotAsked);
            }
            Err(e) => return Err(e),
        };
        // Unconditional. An unreachable poll returns before this, so the latch survives
        // one — but a `vfs/stats` that *did* answer, with a mode this build cannot read,
        // must clear it, or the disk fallback keeps trusting a write path it can no
        // longer see.
        let mode = stats.cache_mode();
        self.mode = mode;
        let Some(cache) = stats.disk_cache else {
            self.forget_roots();
            return Ok(CacheProbe::Absent);
        };
        // Together or not at all. Latched separately, a response carrying one and not the
        // other pairs a fresh root with a stale one — and a stale metadata tree that still
        // exists and is empty reads as "nothing outstanding" for a cache that has a
        // backlog. rclone sets both whenever `diskCache` is present, so this costs nothing.
        if !cache.path.is_empty() && !cache.path_meta.is_empty() {
            self.cache_path = Some(cache.path.clone());
            self.meta_path = Some(cache.path_meta.clone());
        } else {
            // A cache reported but not named is still an answer: this mount's roots are
            // unknown. Leaving the old ones in place is the third way to end up scanning
            // a previous instance's tree, alongside no `diskCache` and no `vfs/stats`.
            self.forget_roots();
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
    use rvt_testutil::Scratch;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn active() -> Duration {
        Duration::from_secs(Poll::default().active_secs)
    }

    fn idle() -> Duration {
        Duration::from_secs(Poll::default().idle_secs)
    }

    /// The rc bodies to answer with, shared so a test can change one between polls.
    type Routes = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, String)>>>;

    fn routes(rs: Vec<(&'static str, String)>) -> Routes {
        std::sync::Arc::new(std::sync::Mutex::new(rs))
    }

    /// Answers a body per rc command path, so a poll can be driven end to end.
    struct FakeRc {
        _dir: Scratch,
        socket: PathBuf,
        routes: Routes,
        handle: tokio::task::JoinHandle<()>,
    }

    impl FakeRc {
        /// Stop serving a route, so it answers 404 — an rclone replaced by a build that
        /// does not register the endpoint.
        fn remove(&self, path: &str) {
            self.routes.lock().unwrap().retain(|(p, _)| *p != path);
        }

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
            let dir = Scratch::new(tag);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.join("rc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

            let routes = self::routes(routes);
            let served = routes.clone();
            let handle = tokio::spawn(async move { serve_routes(listener, served).await });
            Self {
                _dir: dir,
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
        let s = p.poll().await;

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

    /// The captured `vfs/stats` re-stamped with a cache mode and a cache-file count.
    /// rclone's own encoding: 0 off, 1 minimal, 2 writes, 3 full. The capture is a
    /// `writes` mount holding one file, so every other combination is synthesised.
    fn stats_with(mode: u64, cache_files: u64) -> String {
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["opt"]["CacheMode"] = serde_json::json!(mode);
        v["diskCache"]["files"] = serde_json::json!(cache_files);
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
        // Reporting that as idle offers the user an unmount of a mount that is not idle.
        // The kernel refuses it (#73), so the file survives — but the applet has still
        // told them something untrue.
        let fake = FakeRc::new(
            "minimal",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", stats_with(1, 1)),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

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
        // turn every minimal mount confidently idle at once, whatever it was doing.
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
        let s = p.poll().await;

        assert!(
            !s.outstanding_known,
            "an unparseable cache mode must fail closed, not open"
        );
        assert!(!s.is_idle());
        assert!(s.degraded_reason.is_some());
    }

    #[tokio::test]
    async fn a_writes_cache_mode_mount_with_an_empty_queue_is_trusted() {
        // The other side of the checks above. `writes` and `full` put every *closed* write
        // through the queue, so an empty one is an answer rather than an absence, and this
        // is the case that has to stay confident. Without it, "never trust the queue"
        // would pass every other test in this file while leaving the applet permanently
        // unable to call anything safe to unmount — useless rather than cautious.
        //
        // What this does not cover is a file still open: rclone enqueues on close, so
        // nothing over rc sees it and this reads idle throughout. The unmount path asks
        // the kernel before it signals rclone and is refused (#73), so nothing is
        // truncated; the reading here is still wrong, which is why `safe_to_unmount()` is
        // necessary and not sufficient and its rustdoc says so.
        let fake = FakeRc::new(
            "writes",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", stats_with(2, 0)),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

        assert!(s.outstanding_known, "every closed write reaches this queue");
        assert!(s.is_idle());
        assert!(s.safe_to_unmount());
        assert!(s.degraded_reason.is_none());
    }

    #[tokio::test]
    async fn a_cache_without_a_queue_keeps_its_counts() {
        // The mirror of the test below, and the same rule: when the two endpoints
        // disagree, believe the one that answered. rclone 1.75 omits the `queue` key and
        // `diskCache` together, so neither case is reachable against it — but resolving
        // them in opposite directions would be the bug, not the unreachability.
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["diskCache"]["uploadsQueued"] = serde_json::json!(5);

        let fake = FakeRc::new(
            "cachenoqueue",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", "{}".to_string()),
                ("vfs/stats", v.to_string()),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

        assert_eq!(s.fidelity, Some(Tier::T3), "the counts are what is left");
        assert_eq!(s.pending.files, 6, "5 queued plus the 1 in progress");
        assert!(!s.outstanding_known);
        let why = s.degraded_reason.clone().expect("say why");
        assert!(
            !why.contains("cache-mode off") && !why.contains("--vfs-cache-mode off"),
            "vfs/stats just reported a cache; blaming the cache mode is a wrong lead: {why}"
        );
    }

    #[tokio::test]
    async fn a_queue_without_a_cache_keeps_its_entries() {
        // rclone 1.75 never answers this way — a cacheless VFS omits the `queue` key and
        // `diskCache` together — so this pins the handling rather than a live behaviour.
        // The point is that the queue is what answered: discarding two real entries
        // because the *other* endpoint came back empty is the mistake already fixed for a
        // missing `vfs/stats`, and it should not survive in the sibling branch.
        let fake = FakeRc::new(
            "queuenocache",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-two-items.json")),
                (
                    "vfs/stats",
                    r#"{"fs":"/src","inUse":1,"metadataCache":{"dirs":1,"files":0}}"#.to_string(),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

        assert_eq!(s.pending.files, 2, "the queue answered; keep what it said");
        assert_eq!(s.pending.known_bytes, 393_216);
        assert!(
            !s.outstanding_known,
            "but a queue with no cache is not a total"
        );
        assert!(s.degraded_reason.is_some());
    }

    #[tokio::test]
    async fn a_read_mostly_full_mount_is_trusted_too() {
        // `full` queues every closed write as well, and is the mode recommended for
        // read-heavy media mounts. Anything keyed on the *cache* being empty rather than
        // the queue would condemn those permanently: measured on a live mount, one read
        // and no write at all leaves `diskCache.files` at 1, and it stays up for
        // `--vfs-cache-max-age`.
        let fake = FakeRc::new(
            "fullread",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", stats_with(3, 400)),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

        assert!(
            s.safe_to_unmount(),
            "400 read-cached files are not outstanding work: {:?}",
            s.degraded_reason
        );
        assert!(s.degraded_reason.is_none());
    }

    #[tokio::test]
    async fn a_partially_observed_mount_still_reports_a_rate() {
        // A `minimal` mount's queue entries are real — read-write opens go through the
        // cache normally — the queue is only possibly incomplete. Differencing a floor is
        // a sound rate, so refusing one here would leave the user watching "2 files,
        // 384KB pending" with no throughput for the whole transfer. Caution has to stop
        // at the claim it is protecting rather than swallow the working ones with it.
        let fake = FakeRc::new(
            "minrate",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-two-items.json")),
                ("vfs/stats", stats_with(1, 2)),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let first = p.poll().await;
        assert!(!first.outstanding_known, "minimal is never the whole story");
        assert!(first.pending.files > 0, "but its entries are real");

        // One file left the queue between polls.
        fake.set(
            "vfs/queue",
            r#"{"queue":[{"name":"two.bin","id":2,"size":131072,"tries":0,"uploading":false}]}"#
                .to_string(),
        );
        let second = p.poll().await;
        assert!(
            second.rate_bytes_per_sec.is_some_and(|r| r > 0),
            "256KB left the queue, got {:?}",
            second.rate_bytes_per_sec
        );
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
        let s = p.poll().await;

        // `erroredFiles` and `outOfSpace` are only in vfs/stats, and the byte total is
        // only in vfs/queue. Both have to survive the merge. The flags are asserted true
        // here because both default to false: a healthy capture would pass this test
        // whether or not the merge happened at all.
        assert!(s.pending.known_bytes > 0, "the queue total survived");
        assert_eq!(
            s.out_of_space,
            Some(true),
            "the cache is full and the user must be told"
        );
        assert_eq!(s.errored_files, Some(3));
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
        let s = p.poll().await;

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
    async fn a_vfs_stats_that_stops_answering_does_not_fault_the_whole_poll() {
        // Capabilities are latched at connect. If the rclone behind this socket is
        // replaced by one that no longer registers `vfs/stats`, the advertised set is
        // stale and the call 404s — the same 404 `vfs/queue` degrades on. Faulting here
        // instead loses the T2 answer the queue still gives, and `main` logs "could not
        // poll" and reports no state at all for the mount.
        let fake = FakeRc::new(
            "statsgone",
            vec![
                // Advertised, but no route is served for it, so the fake answers 404.
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-two-items.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

        assert_eq!(s.fidelity, Some(Tier::T2));
        assert_eq!(s.pending.files, 2, "the queue still answered");
        assert_eq!(s.pending.known_bytes, 393_216);
    }

    #[tokio::test]
    async fn a_build_without_vfs_queue_falls_to_counts_rather_than_failing() {
        // `bytesUsed` is non-zero here on purpose. The capture reads 0, so an assertion
        // against it would pass whether or not the code wrongly surfaced it as a pending
        // total — which is the exact mistake the assertion below is named for.
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["diskCache"]["bytesUsed"] = serde_json::json!(999_999);

        let fake = FakeRc::new(
            "noqueue",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/stats"])),
                ("vfs/stats", v.to_string()),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let s = p.poll().await;

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
        let dir = Scratch::new("gone");
        let missing = dir.join("rc.sock");
        let client = RcClient::new(&missing);

        // `connect` degrades to no capabilities rather than erroring.
        let mut p = MountPoller::connect("backup", client).await;
        assert_eq!(p.tier(), Tier::T4);

        let s = p.poll().await;
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
        let busy = p.poll().await;
        assert!(!busy.is_idle(), "the capture has a queued file");
        assert_eq!(MountPoller::interval(&busy, &Poll::default()), active());

        // An empty queue rclone actually reported. `VfsQueue::default()` is not that: its
        // key is absent, which is how a cacheless VFS answers and is not idle at all.
        let reported_empty: VfsQueue = serde_json::from_str(r#"{"queue":[]}"#).unwrap();
        let empty = TransferState::from_queue("backup", &reported_empty);
        assert!(empty.is_idle());
        assert_eq!(
            MountPoller::interval(&empty, &Poll::default()),
            idle(),
            "an idle mount must not be polled every second"
        );

        let unreported = TransferState::from_queue("backup", &VfsQueue::default());
        assert!(
            !unreported.is_idle(),
            "no queue key means no queue, which is the opposite of an empty one"
        );
    }

    #[test]
    fn the_cadence_is_the_one_the_config_asked_for() {
        let poll = Poll {
            active_secs: 3,
            idle_secs: 600,
        };
        let reported_empty: VfsQueue = serde_json::from_str(r#"{"queue":[]}"#).unwrap();
        let empty = TransferState::from_queue("backup", &reported_empty);
        assert_eq!(
            MountPoller::interval(&empty, &poll),
            Duration::from_secs(600)
        );

        let mut busy = empty.clone();
        busy.pending.files = 1;
        assert_eq!(MountPoller::interval(&busy, &poll), Duration::from_secs(3));

        // The one cadence the config does not get to raise: each T4 poll walks the whole
        // metadata tree.
        busy.fidelity = Some(Tier::T4);
        assert_eq!(
            MountPoller::interval(&busy, &poll),
            Duration::from_secs(600)
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
        let dir = Scratch::new("live");
        let (src, cache) = (dir.dir("src"), dir.dir("cache"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
        let mut state = p.poll().await;
        for _ in 0..40 {
            if !state.is_idle() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            state = p.poll().await;
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
        assert_eq!(state.out_of_space, Some(false));
        assert_eq!(state.errored_files, Some(0));

        let _ = child.kill().await;
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
        let first = p.poll().await;
        assert_eq!(
            first.rate_bytes_per_sec, None,
            "one sample cannot be a rate"
        );
        assert_eq!(first.uploading, Some(1), "the capture has a file in flight");

        let second = p.poll().await;
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
        let s = p.poll().await;

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
            MountPoller::interval(&s, &Poll::default()),
            idle(),
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
        let first = p.poll().await;
        assert!(first.has_progress, "core/stats reached the state");
        assert_eq!(
            first.rate_bytes_per_sec, None,
            "one sample is not a rate yet"
        );

        // The queue is unchanged; only the bytes sent have grown.
        fake.set("core/stats", progressed(60_000_000));
        let second = p.poll().await;
        assert_eq!(second.pending.known_bytes, first.pending.known_bytes);
        assert!(
            second.rate_bytes_per_sec.is_some_and(|r| r > 0),
            "40MB moved between polls, got {:?}",
            second.rate_bytes_per_sec
        );
    }

    #[tokio::test]
    async fn a_reason_latched_at_connect_does_not_outlive_what_caused_it() {
        // `probe` returns `Ok` with a reason only for unreachable-class errors; anything
        // else is `Err`. Dropping that `Err` leaves the connect-time reason in place, so a
        // mount whose socket is now present and answering keeps reporting "no rc socket at
        // …" — sending the user to look for a missing file that exists.
        let dir = Scratch::new("relatch");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");

        let mut p = MountPoller::connect("backup", RcClient::new(&socket)).await;
        let before = p.poll().await;
        let first = before
            .degraded_reason
            .clone()
            .expect("nothing listening yet");

        // Something is listening now, but `rc/list` is not registered — a 404, which
        // `probe` surfaces as an error rather than as a refusal.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let rs = routes(vec![("core/version", fixture("core-version.json"))]);
        let server = tokio::spawn(async move { serve_routes(listener, rs).await });

        let after = p.poll().await;
        let second = after.degraded_reason.clone().expect("still cannot be read");
        assert_ne!(
            second, first,
            "the socket exists and answers now, so a reason naming its absence is stale"
        );

        server.abort();
    }

    #[tokio::test]
    async fn a_mount_that_starts_after_the_service_recovers() {
        // Capabilities are latched at connect. If rclone was not up then, the set is
        // empty — and with an empty set the poller issues no rc calls at all, so without
        // re-probing the mount reports as unobservable for the life of the service even
        // after rclone comes back. The service starting before its mount units, or a
        // mount unit restarting after a crash, both land here.
        let dir = Scratch::new("late");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");

        // Nothing listening yet.
        let mut p = MountPoller::connect("backup", RcClient::new(&socket)).await;
        let before = p.poll().await;
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

        let after = p.poll().await;
        assert!(
            after.outstanding_known,
            "the poller must re-probe rather than stay blind for the session"
        );
        assert_eq!(after.fidelity, Some(Tier::T2));
        assert!(after.pending.files > 0);

        server.abort();
    }

    /// A `vfs/stats` whose cache paths point at a tree this test controls.
    ///
    /// The captured fixture names the machine the capture was taken on, so a test relying
    /// on it is really asserting against whatever happens to be at that path — which on
    /// this machine is sometimes a real directory.
    fn stats_pointing_at(meta: &std::path::Path, data: &std::path::Path) -> String {
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture("vfs-stats-upload-in-progress.json")).unwrap();
        v["diskCache"]["pathMeta"] = serde_json::json!(meta.to_string_lossy());
        v["diskCache"]["path"] = serde_json::json!(data.to_string_lossy());
        v.to_string()
    }

    /// Write one dirty descriptor and its data file, as rclone lays them out.
    fn dirty_entry(root: &std::path::Path, name: &str, bytes: usize) {
        let meta = root.join("vfsMeta");
        let data = root.join("vfs");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(
            meta.join(name),
            format!(
                r#"{{"ModTime":"2026-08-08T00:00:00Z","ATime":"2026-08-08T00:00:00Z",
                    "Size":{bytes},"Rs":[{{"Pos":0,"Size":{bytes}}}],"Fingerprint":"","Dirty":true}}"#
            ),
        )
        .unwrap();
        std::fs::write(data.join(name), vec![b'x'; bytes]).unwrap();
    }

    #[tokio::test]
    async fn an_rclone_that_dies_after_connecting_falls_to_the_disk() {
        // Capabilities were probed successfully, so the poller knows this mount has a
        // queue — and then rclone goes away. The backlog does not: dirty items outlive the
        // process that queued them, which is the whole reason this tier exists. Before
        // #22 the honest answer here was "cannot tell"; now it is the answer itself.
        let cache = Scratch::new("died");
        dirty_entry(cache.path(), "left-behind.bin", 4096);

        let fake = FakeRc::new(
            "died",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                (
                    "vfs/stats",
                    stats_pointing_at(&cache.join("vfsMeta"), &cache.join("vfs")),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let before = p.poll().await;
        assert!(before.outstanding_known && !before.is_idle());

        // rclone exits: the socket is gone, the capabilities are not.
        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert_eq!(after.fidelity, Some(Tier::T4), "read off disk, not over rc");
        assert_eq!(after.pending.files, 1, "the backlog is still there");
        assert_eq!(after.pending.known_bytes, 4096);
        assert!(!after.is_idle());
        assert!(
            after.degraded_reason.is_some(),
            "this is a fallback, and the user should be told which one"
        );
        assert_eq!(
            MountPoller::interval(&after, &Poll::default()),
            idle(),
            "a disk-derived state has files outstanding but must not drive the 1s cadence \
             — each poll behind it is a full walk of the metadata tree"
        );
    }

    #[tokio::test]
    async fn a_minimal_mount_is_not_trusted_just_because_the_disk_was_readable() {
        // The live path refuses to call a `minimal` mount idle on an empty queue, because
        // a write-only open streams past the cache entirely. The disk fallback has to
        // inherit that: the cache is exactly what such a write bypasses, so a scan finding
        // nothing there proves even less than an empty queue did. Without the mode latched
        // beside the roots, falling back produced a *more* confident answer than the live
        // path it replaced.
        let cache = Scratch::new("minfall");
        cache.dir("vfsMeta");
        cache.dir("vfs");

        let mut v: serde_json::Value = serde_json::from_str(&stats_pointing_at(
            &cache.join("vfsMeta"),
            &cache.join("vfs"),
        ))
        .unwrap();
        v["opt"]["CacheMode"] = serde_json::json!(1);

        let fake = FakeRc::new(
            "minfall",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", v.to_string()),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert!(
            !after.safe_to_unmount(),
            "an empty cache on a minimal mount says nothing about a streaming write"
        );
        assert!(!after.outstanding_known);
        let why = after.degraded_reason.clone().expect("say why");
        assert!(
            why.contains("does not put every write through the cache"),
            "the mode was reported and is known to stream, which is a different thing to \
             say than 'unknown': {why}"
        );
    }

    #[tokio::test]
    async fn a_writes_mount_falling_to_disk_is_still_trusted() {
        // The other side, or the rule above would just be "never trust the fallback",
        // which makes the tier pointless.
        let cache = Scratch::new("writefall");
        cache.dir("vfsMeta");
        cache.dir("vfs");

        let fake = FakeRc::new(
            "writefall",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                (
                    "vfs/stats",
                    stats_pointing_at(&cache.join("vfsMeta"), &cache.join("vfs")),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert!(
            after.safe_to_unmount(),
            "the cache is present, readable and empty on a mode that queues every write: \
             {:?}",
            after.degraded_reason
        );
        assert_eq!(after.fidelity, Some(Tier::T4));
    }

    #[tokio::test]
    async fn a_cache_mode_that_stops_being_recognised_clears_the_latched_one() {
        // The latch exists so the disk fallback still knows the mode once rclone is gone.
        // But `vfs/stats` answering with a mode this build cannot read is not the same as
        // it not answering: keeping the previous value there has the fallback trust a
        // write path it can no longer see, undoing the fail-closed default on the live
        // path. The replacement-on-the-same-socket case is one the poller already handles
        // elsewhere, so it is not hypothetical.
        let cache = Scratch::new("modelatch");
        cache.dir("vfsMeta");
        cache.dir("vfs");

        let good = stats_pointing_at(&cache.join("vfsMeta"), &cache.join("vfs"));
        let mut odd: serde_json::Value = serde_json::from_str(&good).unwrap();
        odd["opt"]["CacheMode"] = serde_json::json!(9);

        let fake = FakeRc::new(
            "modelatch",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", good),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        assert!(p.poll().await.safe_to_unmount(), "a writes mount, latched");

        // Same socket, a build whose cache mode this one cannot read.
        fake.set("vfs/stats", odd.to_string());
        let live = p.poll().await;
        assert!(!live.outstanding_known, "the live path fails closed on it");

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert!(
            !after.safe_to_unmount(),
            "and so must the fallback — the stale Writes would have said yes"
        );
        let why = after.degraded_reason.clone().expect("say why");
        assert!(
            why.contains("recognised cache mode"),
            "an unknown mode is not the same claim as a known non-queueing one: {why}"
        );
    }

    #[tokio::test]
    async fn the_two_cache_roots_are_latched_together_or_not_at_all() {
        // They name halves of one tree. Taking a fresh `path` while keeping a stale
        // `pathMeta` scans one mount's descriptors against another's data files, so every
        // size reads unknown — and had the staleness gone the other way, an empty old
        // metadata tree would have reported a backlogged cache as idle. rclone sets both
        // whenever `diskCache` is present, so this is a guard rather than a fix.
        let old = Scratch::new("rootpair-a");
        let new = Scratch::new("rootpair-b");
        dirty_entry(old.path(), "waiting.bin", 2048);
        new.dir("vfs");

        let good = stats_pointing_at(&old.join("vfsMeta"), &old.join("vfs"));
        // A response naming a data root but no metadata root.
        let mut half: serde_json::Value = serde_json::from_str(&good).unwrap();
        half["diskCache"]["path"] = serde_json::json!(new.join("vfs").to_string_lossy());
        half["diskCache"]["pathMeta"] = serde_json::json!("");

        let fake = FakeRc::new(
            "rootpair",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", good),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let _ = p.poll().await;

        // Still live. The mode was reported, so this poll must not complain that it was
        // not — clearing the latch alongside the roots produced exactly that wrong
        // complaint, about a mount whose only problem was unusable paths.
        fake.set("vfs/stats", half.to_string());
        let live = p.poll().await;
        if let Some(why) = live.degraded_reason.clone() {
            assert!(
                !why.contains("recognised cache mode"),
                "rclone did report a mode; only its roots were unusable: {why}"
            );
        }

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert_eq!(
            after.fidelity, None,
            "a cache reported but not named leaves this mount's roots unknown, and the \
             previous instance's tree is not an answer about this one"
        );
        assert!(!after.safe_to_unmount());
    }

    #[tokio::test]
    async fn an_rclone_without_vfs_stats_clears_the_roots_a_previous_one_named() {
        // A 404 is an answer: this rclone does not have the endpoint, so nothing it says
        // describes the cache a previous one named. Keeping the roots lets the fallback
        // scan a tree belonging to an instance that is gone and report its emptiness as
        // this mount's — a confident idle for a mount whose real cache was never named.
        let stale = Scratch::new("404root");
        stale.dir("vfsMeta");
        stale.dir("vfs");

        let fake = FakeRc::new(
            "root404",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                (
                    "vfs/stats",
                    stats_pointing_at(&stale.join("vfsMeta"), &stale.join("vfs")),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        assert!(
            p.poll().await.safe_to_unmount(),
            "roots latched from a cache"
        );

        // Same socket, a build with no `vfs/stats`. Capabilities were latched at connect,
        // so it is still asked, and answers 404.
        fake.remove("vfs/stats");
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert_eq!(
            after.fidelity, None,
            "no cache of this mount's was ever named, so no tier produced this"
        );
        assert!(!after.safe_to_unmount());
    }

    #[tokio::test]
    async fn a_cache_that_stops_being_reported_clears_the_latched_roots() {
        // The roots describe the rclone answering now. If it says it has no cache, a tree
        // latched from a previous instance is not this mount's — and scanning it finds an
        // empty directory, which reads as "nothing outstanding" for a mount that has no
        // cache to be outstanding in. rclone 1.75 pairs a cacheless VFS with CacheMode 0,
        // which fails closed on its own, so this is the class fix rather than a live bug.
        let stale = Scratch::new("staleroot");
        stale.dir("vfsMeta");
        stale.dir("vfs");

        let good = stats_pointing_at(&stale.join("vfsMeta"), &stale.join("vfs"));
        // Same socket, an rclone reporting a queueing mode and no cache.
        let mut cacheless: serde_json::Value = serde_json::from_str(&good).unwrap();
        cacheless["diskCache"] = serde_json::Value::Null;

        let fake = FakeRc::new(
            "staleroot",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", r#"{"queue":[]}"#.to_string()),
                ("vfs/stats", good),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        assert!(
            p.poll().await.safe_to_unmount(),
            "roots latched from a cache"
        );

        fake.set("vfs/stats", cacheless.to_string());
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert_eq!(
            after.fidelity, None,
            "there is no cache of this mount's to scan, so no tier produced this"
        );
        assert!(!after.safe_to_unmount());
    }

    #[tokio::test]
    async fn a_cache_that_cannot_be_read_is_named_separately_from_the_rc_failure() {
        // `unreachable`'s rustdoc promises this: reporting only "rclone is unreachable"
        // when the fallback was tried and could not read the cache either sends the user
        // to look at rclone, when the thing to look at is the cache directory.
        use std::os::unix::fs::PermissionsExt;
        let cache = Scratch::new("noread");
        dirty_entry(cache.path(), "waiting.bin", 16);

        let fake = FakeRc::new(
            "noread",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                (
                    "vfs/stats",
                    stats_pointing_at(&cache.join("vfsMeta"), &cache.join("vfs")),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);
        let meta = cache.join("vfsMeta");
        std::fs::set_permissions(&meta, std::fs::Permissions::from_mode(0o000)).unwrap();

        let after = p.poll().await;
        std::fs::set_permissions(&meta, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(!after.outstanding_known);
        let why = after.degraded_reason.clone().expect("say why");
        assert!(
            why.contains("could not be read"),
            "the cache failing to read is a different thing to go and look at than rclone \
             being unreachable: {why}"
        );
    }

    #[tokio::test]
    async fn a_fallback_that_could_not_read_everything_says_which_part_it_missed() {
        // Otherwise the state reads not-known with nothing in the text to explain it, and
        // the rc failure takes the blame for a cache that was only half read — sending the
        // user to look at rclone when the thing to look at is the cache directory.
        let cache = Scratch::new("partial");
        dirty_entry(cache.path(), "readable.bin", 32);
        // A descriptor that exists and does not parse. rclone rewrites these in place, so
        // a torn read looks exactly like this.
        cache.write("vfsMeta/torn.bin", "");

        let fake = FakeRc::new(
            "partialscan",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                (
                    "vfs/stats",
                    stats_pointing_at(&cache.join("vfsMeta"), &cache.join("vfs")),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert!(!after.outstanding_known, "one entry went unread");
        let why = after.degraded_reason.clone().expect("say why");
        assert!(
            why.contains("could not be read"),
            "the reason has to name the unread entries, not just the rc failure: {why}"
        );
    }

    #[tokio::test]
    async fn an_rclone_that_dies_with_its_cache_gone_reports_unknown_not_zero() {
        // The other half. An empty scan and an absent tree both find no dirty files, but
        // a cache directory disappearing is not evidence the queue drained — and this is
        // the mount that must never flip to "nothing outstanding, exactly".
        let cache = Scratch::new("nocache");

        let fake = FakeRc::new(
            "diednocache",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                (
                    "vfs/stats",
                    stats_pointing_at(&cache.join("vfsMeta"), &cache.join("vfs")),
                ),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await;
        let _ = p.poll().await;

        fake.handle.abort();
        let _ = std::fs::remove_file(&fake.socket);

        let after = p.poll().await;
        assert!(
            !after.outstanding_known,
            "the queue did not empty — the cache we were told about is simply not there"
        );
        assert!(!after.is_idle());
        assert_eq!(after.pending.files, 0);
        assert_eq!(after.rate_bytes_per_sec, None);
        // `from_scan` alone would give all of the above, so assert what this branch adds:
        // no tier, because no scan of a cache happened, and a reason that says which of
        // the two absences it was. Otherwise the user is told a scan returned zero.
        assert_eq!(
            after.fidelity, None,
            "there was no cache to scan, so no tier produced this"
        );
        let why = after.degraded_reason.clone().expect("say why");
        assert!(
            why.contains("no longer on disk"),
            "'read 0 pending files from the cache' would be a claim about a cache that \
             is not there: {why}"
        );
    }
}
