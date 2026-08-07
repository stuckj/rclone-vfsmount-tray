//! Asking one mount what is outstanding, at a rate that matches what it is doing.
//!
//! The tier is resolved per mount rather than once for the process: one mount may answer
//! over rc while another was started by somebody else and can only be scanned on disk.

use rvt_core::capabilities::{Capabilities, Tier};
use rvt_core::models::{CoreStats, DiskCache, VfsQueue, VfsStats};
use rvt_core::rc::{RcClient, RcError};
use rvt_core::transfer::{RateEstimator, TransferState};
use std::time::Duration;

/// Poll interval while something is outstanding.
const ACTIVE: Duration = Duration::from_secs(1);
/// Poll interval while the queue is empty. An idle mount should cost nothing.
const IDLE: Duration = Duration::from_secs(15);

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
    pub async fn connect(name: &str, client: RcClient) -> Result<Self, RcError> {
        let caps = Capabilities::probe(&client).await?;
        Ok(Self {
            name: name.to_string(),
            client,
            caps,
            rate: RateEstimator::new(),
            cache_path: None,
        })
    }

    pub fn tier(&self) -> Tier {
        self.caps.tier()
    }

    /// How long to wait before polling again.
    pub fn interval(state: &TransferState) -> Duration {
        if state.is_idle() {
            IDLE
        } else {
            ACTIVE
        }
    }

    /// One poll.
    ///
    /// Never fails for a mount that has gone away: an unreachable rclone is the case the
    /// on-disk tier exists for, so it degrades rather than erroring and says why. A real
    /// fault from a live rclone is returned.
    pub async fn poll(&mut self) -> Result<TransferState, RcError> {
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
            Ok(queue) => TransferState::from_queue(&self.name, &queue),
            Err(e) if e.is_unreachable() => {
                self.rate.reset();
                return Ok(self.unreachable(&e));
            }
            // No `vfs/queue` on this build: counts are all there is over rc.
            Err(RcError::Failed { status: 404, .. }) => match &cache {
                Some(c) => TransferState::from_stats(&self.name, c),
                None => TransferState::from_scan(&self.name, Vec::new())
                    .degraded("this rclone has no vfs/queue and no disk cache"),
            },
            Err(e) => return Err(e),
        };

        if let Some(c) = &cache {
            state = state.with_cache_health(c);
        }

        // Per-file progress, where the build has `core/stats` and the transfers can be
        // attributed to this mount. Without the cache path a transfer cannot be told from
        // another mount's, so the lift is skipped rather than guessed at.
        if self.caps.has("core/stats") && self.cache_path.is_some() {
            if let Ok(stats) = self.client.call::<CoreStats>("core/stats", empty()).await {
                // Both conditions, per DESIGN.md: the `global_stats` group alone admits
                // cache *downloads*, which would then be counted as pending uploads.
                let cache = self.cache_path.clone().unwrap_or_default();
                let mine: Vec<_> = stats.writeback_uploads(&cache).cloned().collect();
                state = state.with_progress(&mine);
            }
        }

        let rate = if state.is_idle() {
            self.rate.reset();
            None
        } else {
            self.rate.sample(state.pending.known_bytes)
        };
        Ok(state.with_rate(rate))
    }

    fn unreachable(&self, e: &RcError) -> TransferState {
        // An empty scan rather than a fabricated one: the on-disk scanner is #22, and
        // reporting zero outstanding when the answer is unknown would be worse than
        // saying so. The reason is what tells the UI to show a degraded state.
        TransferState::from_scan(&self.name, Vec::new()).degraded(e.to_string())
    }

    async fn fetch_stats(&mut self) -> Result<Option<DiskCache>, RcError> {
        if !self.caps.has("vfs/stats") {
            return Ok(None);
        }
        let stats: VfsStats = self.client.call("vfs/stats", empty()).await?;
        let cache = stats.disk_cache;
        if let Some(c) = &cache {
            if !c.path.is_empty() {
                self.cache_path = Some(c.path.clone());
            }
        }
        Ok(cache)
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

    /// Answers a fixed body per rc command path, so a poll can be driven end to end.
    struct FakeRc {
        dir: PathBuf,
        socket: PathBuf,
        handle: tokio::task::JoinHandle<()>,
    }

    impl FakeRc {
        async fn new(tag: &str, routes: Vec<(&'static str, String)>) -> Self {
            let dir = std::env::temp_dir().join(format!("rvt-poll-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.join("rc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

            let handle = tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                while let Ok((mut stream, _)) = listener.accept().await {
                    let routes = routes.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let body = routes
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
                                format!(
                                    "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{b}",
                                    b.len()
                                )
                            }
                        };
                        let _ = stream.write_all(resp.as_bytes()).await;
                        let _ = stream.flush().await;
                    });
                }
            });
            Self {
                dir,
                socket,
                handle,
            }
        }

        fn client(&self) -> RcClient {
            RcClient::new(&self.socket)
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

        let mut p = MountPoller::connect("backup", fake.client()).await.unwrap();
        let s = p.poll().await.expect("a live rclone is not an error");

        assert_eq!(s.fidelity, Tier::T2);
        assert!(s.pending.known_bytes > 0, "the queue carries a byte total");
        assert_eq!(s.uploading, Some(1));
        assert!(s.degraded_reason.is_none());
    }

    #[tokio::test]
    async fn the_actionable_flags_come_from_stats_without_losing_the_queue_total() {
        let fake = FakeRc::new(
            "flags",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await.unwrap();
        let s = p.poll().await.unwrap();

        // `erroredFiles` and `outOfSpace` are only in vfs/stats, and the byte total is
        // only in vfs/queue. Both have to survive the merge.
        assert!(s.pending.known_bytes > 0);
        assert!(!s.out_of_space, "the capture is not out of space");
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
        let mut p = MountPoller::connect("backup", fake.client()).await.unwrap();
        let s = p.poll().await.expect("a missing command is not a fault");

        assert_eq!(s.fidelity, Tier::T3);
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
        let mut p = MountPoller::connect("backup", client).await.unwrap();
        assert_eq!(p.tier(), Tier::T4);

        let s = p.poll().await.expect("an absent rclone is not a fault");
        assert_eq!(s.fidelity, Tier::T4);
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
        let mut p = MountPoller::connect("backup", fake.client()).await.unwrap();
        let busy = p.poll().await.unwrap();
        assert!(!busy.is_idle(), "the capture has a queued file");
        assert_eq!(MountPoller::interval(&busy), ACTIVE);

        let empty = TransferState::from_queue("backup", &VfsQueue::default());
        assert!(empty.is_idle());
        assert_eq!(
            MountPoller::interval(&empty),
            IDLE,
            "an idle mount must not be polled every second"
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

        let dir = std::env::temp_dir().join(format!("rvt-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (src, cache) = (dir.join("src"), dir.join("cache"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
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

        let mut p = MountPoller::connect("live", RcClient::new(&socket))
            .await
            .expect("a live rclone answers rc/list");
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
        let _ = std::fs::remove_dir_all(&dir);
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
    async fn a_rate_appears_only_once_bytes_have_actually_moved() {
        let fake = FakeRc::new(
            "rate",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", fixture("vfs-queue-uploading.json")),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;
        let mut p = MountPoller::connect("backup", fake.client()).await.unwrap();
        let first = p.poll().await.unwrap();
        assert_eq!(
            first.rate_bytes_per_sec, None,
            "one sample cannot be a rate; reporting 0 would read as stalled"
        );
        let second = p.poll().await.unwrap();
        assert!(second.rate_bytes_per_sec.is_some());
    }
}
