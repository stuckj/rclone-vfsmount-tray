//! The [`MountSupervisor`] implementation.
//!
//! Reality, not bookkeeping: every state answer comes from `/proc/self/mountinfo` rather
//! than from what this process believes it started. That is what lets a mount survive a
//! service restart and still be recognised, and what makes a mount somebody else started
//! visible instead of invisible.

use crate::systemd::{UnitManager, UnitSpec, UnitStatus};
use rvt_core::mountinfo::{self, MountEntry};
use rvt_core::supervisor::{
    BoxFuture, DiscoveredMount, MountState, MountSupervisor, SupervisorError,
};
use rvt_core::{Config, Mount};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long to wait for a mount point to start serving before calling it failed.
///
/// Generous, because this covers an OAuth token refresh and the first listing of a cold
/// remote, not just process startup.
const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// `ENOTCONN`. What every operation on a mount point returns once the FUSE daemon
/// serving it has gone away without unmounting.
const ENOTCONN: i32 = 107;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long to wait on a filesystem call that resolves through a mount point.
///
/// Generous relative to a local `stat`, short relative to a poll tick: the point is only
/// to stop a wedged mount holding the executor, not to diagnose it.
const FS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts rclone mounts as transient systemd units.
pub struct SystemdSupervisor<M: UnitManager> {
    config: Arc<Config>,
    rclone: PathBuf,
    units: M,
    /// Overridden in tests. Everything else reads the real kernel interface.
    mountinfo_path: PathBuf,
    /// Where rc sockets live — `$XDG_RUNTIME_DIR/rclone-vfsmount-tray`.
    runtime_dir: PathBuf,
    ready_timeout: Duration,
    /// How long to wait for a mount point to be released after stopping its unit.
    gone_timeout: Duration,
    /// How long to wait for systemd to free the unit name.
    unit_gone_timeout: Duration,
    /// Paths to treat as stale. `ENOTCONN` needs a real FUSE daemon that has died, which
    /// a unit test cannot arrange, so the probe needs a seam or the stale handling cannot
    /// be tested at all.
    #[cfg(test)]
    stale_paths: std::collections::HashSet<PathBuf>,
}

impl<M: UnitManager> SystemdSupervisor<M> {
    pub fn new(config: Arc<Config>, rclone: PathBuf, units: M, runtime_dir: PathBuf) -> Self {
        Self {
            config,
            rclone,
            units,
            mountinfo_path: PathBuf::from("/proc/self/mountinfo"),
            runtime_dir,
            ready_timeout: READY_TIMEOUT,
            gone_timeout: Duration::from_secs(30),
            unit_gone_timeout: Duration::from_secs(35),
            #[cfg(test)]
            stale_paths: std::collections::HashSet::new(),
        }
    }

    /// Point state reads at a fixture instead of `/proc`, and shorten the readiness wait.
    #[cfg(test)]
    fn with_test_overrides(mut self, mountinfo_path: PathBuf, ready_timeout: Duration) -> Self {
        self.mountinfo_path = mountinfo_path;
        self.ready_timeout = ready_timeout;
        self.gone_timeout = Duration::from_millis(300);
        self.unit_gone_timeout = Duration::from_millis(300);
        self
    }

    #[cfg(test)]
    fn with_stale(mut self, paths: &[&Path]) -> Self {
        self.stale_paths = paths.iter().map(|p| p.to_path_buf()).collect();
        self
    }

    /// The rc socket for a mount.
    pub fn socket_path(&self, name: &str) -> PathBuf {
        self.runtime_dir.join(format!("{name}.sock"))
    }

    fn mount_config(&self, name: &str) -> Result<&Mount, SupervisorError> {
        self.config
            .mounts
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| SupervisorError::UnknownMount(name.to_string()))
    }

    /// The mount point as the kernel will report it.
    ///
    /// mountinfo records fully resolved paths. A configured `/home/u/mnt/x` where
    /// `/home/u/mnt` is a symlink appears there as its target, so an uncanonicalised
    /// comparison never matches and a working mount reads as down.
    ///
    /// Falls back to the configured path when it cannot be resolved, which is the normal
    /// case before the directory exists.
    async fn resolved_point(m: &Mount) -> PathBuf {
        let raw = m.mount_point.clone();
        let fallback = raw.clone();
        Self::off_thread(move || std::fs::canonicalize(&raw).ok())
            .await
            .flatten()
            .unwrap_or(fallback)
    }

    /// Run a filesystem call off the async executor, and give up on it if it hangs.
    ///
    /// Every `stat` here resolves *through* a FUSE mount point. When rclone is gone the
    /// kernel answers `ENOTCONN`, but when it is alive and not answering — a dropped
    /// network, a VFS deadlock — the call blocks uninterruptibly, which is the familiar
    /// "`df` hangs" symptom. On the executor that consumes a worker per wedged mount and
    /// eventually stops the service answering D-Bus at all, so one bad remote would take
    /// down every other mount: exactly the failure the per-mount process model exists to
    /// avoid.
    ///
    /// `None` means it did not answer in time. The blocking-pool thread is still stuck —
    /// nothing can un-stick a blocking `stat` — but the executor is not.
    async fn off_thread<T, F>(f: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        match tokio::time::timeout(FS_PROBE_TIMEOUT, tokio::task::spawn_blocking(f)).await {
            Ok(Ok(v)) => Some(v),
            _ => None,
        }
    }

    /// Whether a mount point is present but no longer served.
    ///
    /// When a FUSE daemon dies without unmounting, the kernel keeps the mountinfo entry
    /// and every operation on it fails with `ENOTCONN`. It is indistinguishable from a
    /// healthy mount by path alone, so it has to be probed.
    async fn is_stale(&self, path: &Path) -> bool {
        #[cfg(test)]
        if !self.stale_paths.is_empty() {
            return self.stale_paths.contains(path);
        }
        let p = path.to_path_buf();
        // A probe that never answers is a mount that is not serving, which is what the
        // caller wants to know — but it is not the *stale* case, which is specifically a
        // dead daemon. Reporting false leaves it as `Mounted`, and the next poll retries.
        Self::off_thread(move || match std::fs::metadata(&p) {
            Err(e) => e.raw_os_error() == Some(ENOTCONN),
            Ok(_) => false,
        })
        .await
        .unwrap_or(false)
    }

    fn live_mounts(&self) -> Vec<MountEntry> {
        // A mountinfo that cannot be read means no evidence of any mount, not an error:
        // reporting everything as down is honest, and the next poll recovers.
        mountinfo::read_from(&self.mountinfo_path).unwrap_or_default()
    }

    /// Resolve one mount's state from the kernel plus systemd.
    ///
    /// The kernel says whether anything is mounted there; the unit says whether it is
    /// ours. Both are needed — mounted with no unit of ours is precisely a foreign mount.
    async fn resolve(&self, m: &Mount) -> Result<MountState, SupervisorError> {
        let point = Self::resolved_point(m).await;
        let live = mountinfo::is_mounted_at(&self.live_mounts(), &point);

        // A mount point left behind by a dead rclone is still "mounted" as far as the
        // kernel is concerned. Calling it Foreign would be doubly wrong: it is ours, and
        // it would be neither startable (something is already there) nor stoppable
        // (foreign mounts refuse to unmount without force).
        if live && self.is_stale(&point).await {
            return Ok(MountState::Failed {
                reason: format!(
                    "{} is mounted but not responding — the rclone serving it exited \
                     without unmounting. Mounting again will clear it.",
                    point.display()
                ),
            });
        }

        let unit = self.units.status(&m.unit_name()).await?;
        Ok(match (live, unit) {
            (true, UnitStatus::Active | UnitStatus::Activating) => MountState::Mounted,
            // rclone flushes the write-back cache during its stop timeout, so this state
            // lasts up to 30s of the user's own unmount. Reporting it as anything else
            // would tell them their mount had become somebody else's mid-operation.
            (true, UnitStatus::Deactivating) => MountState::Unmounting,
            // Ours, and it died with the kernel entry still there. Still ours.
            (true, UnitStatus::Failed) => MountState::Failed {
                reason: self.failure_reason(m).await,
            },
            // Mounted with no unit of ours at all: somebody else started it.
            (true, UnitStatus::Inactive) => MountState::Foreign,

            // `Type=exec` reports active as soon as rclone is exec'd, seconds before the
            // mount point answers, so an active unit with nothing mounted yet is coming
            // up rather than down.
            (false, UnitStatus::Active | UnitStatus::Activating) => MountState::Mounting,
            (false, UnitStatus::Deactivating) => MountState::Unmounting,
            (false, UnitStatus::Failed) => MountState::Failed {
                reason: self.failure_reason(m).await,
            },
            (false, UnitStatus::Inactive) => MountState::Unmounted,
        })
    }

    /// Wait for the mount point to start serving.
    ///
    /// Polls the kernel rather than trusting the unit: systemd calls the unit active as
    /// soon as rclone has been exec'd, which is seconds before the mount point answers.
    async fn await_ready(&self, m: &Mount) -> Result<(), SupervisorError> {
        let deadline = std::time::Instant::now() + self.ready_timeout;
        loop {
            if mountinfo::is_mounted_at(&self.live_mounts(), &Self::resolved_point(m).await) {
                return Ok(());
            }
            // A unit that has already failed will never become ready; waiting out the
            // full timeout would only delay showing the user why.
            if self.units.status(&m.unit_name()).await? == UnitStatus::Failed {
                return Err(SupervisorError::RcloneFailed {
                    reason: self.failure_reason(m).await,
                    source: None,
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(SupervisorError::RcloneFailed {
                    reason: format!(
                        "{} did not start serving within {}s. {}",
                        m.mount_point.display(),
                        self.ready_timeout.as_secs(),
                        self.failure_reason(m).await
                    )
                    .trim_end()
                    .to_string(),
                    source: None,
                });
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn failure_reason(&self, m: &Mount) -> String {
        let out = self.units.recent_output(&m.unit_name()).await;
        if out.is_empty() {
            "rclone logged nothing; check `journalctl --user -u ".to_string() + &m.unit_name() + "`"
        } else {
            out
        }
    }

    /// Create the runtime directory private to this user.
    ///
    /// This is what actually protects the rc sockets. rclone creates a socket 0775
    /// whatever it is asked for, and connecting to a UNIX socket needs only write
    /// permission — but a 0700 directory cannot be traversed by anyone else, so the mode
    /// of the socket inside it stops being reachable. Doing it this way keeps the
    /// protection off the *mount unit's* umask, which rclone also applies to every file
    /// it creates inside the mount.
    fn prepare_runtime_dir(&self) -> Result<(), SupervisorError> {
        let ctx = |e: std::io::Error| SupervisorError::Supervision {
            context: format!(
                "preparing the runtime directory {}",
                self.runtime_dir.display()
            ),
            source: Some(Box::new(e)),
        };
        std::fs::create_dir_all(&self.runtime_dir).map_err(ctx)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.runtime_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(ctx)?;
        }
        Ok(())
    }

    /// Make sure the mount point is a directory we can mount onto.
    ///
    /// Created when missing: requiring the user to `mkdir` first is friction with no
    /// safety value, since a wrong path fails at mount time either way.
    fn prepare_mount_point(path: &Path) -> Result<(), SupervisorError> {
        let bad = |reason: String, source: Option<rvt_core::supervisor::Cause>| {
            SupervisorError::BadMountPoint {
                path: path.display().to_string(),
                reason,
                source,
            }
        };
        match std::fs::metadata(path) {
            Ok(md) if md.is_dir() => Ok(()),
            Ok(_) => Err(bad("exists but is not a directory".into(), None)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)
                .map_err(|e| bad("could not be created".into(), Some(Box::new(e)))),
            Err(e) => Err(bad("could not be inspected".into(), Some(Box::new(e)))),
        }
    }

    /// Unmount a path without going through systemd.
    ///
    /// Needed for foreign mounts, and as the fallback when stopping the unit leaves the
    /// mount point behind. Each escalation is reported rather than applied silently: a
    /// lazy unmount detaches a filesystem that may still have writers.
    async fn fusermount(path: &Path, lazy: bool) -> Result<(), SupervisorError> {
        let mut args = vec!["-u"];
        if lazy {
            args.push("-z");
        }
        let path_str = path.to_string_lossy().into_owned();
        args.push(&path_str);
        let out = tokio::process::Command::new("fusermount3")
            .args(&args)
            .output()
            .await;
        // fusermount3 is the current name; fall back for older systems.
        let out = match out {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tokio::process::Command::new("fusermount")
                    .args(&args)
                    .output()
                    .await
            }
            other => other,
        };
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(SupervisorError::Busy {
                path: format!(
                    "{}: {}",
                    path.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            }),
            Err(e) => Err(SupervisorError::Supervision {
                context: format!("running fusermount for {}", path.display()),
                source: Some(Box::new(e)),
            }),
        }
    }

    /// Wait for the unit to stop occupying its name.
    ///
    /// `StopUnit` only enqueues a job, and rclone can hold `TimeoutStopUSec` flushing its
    /// write-back cache, so the name stays taken well after the mount point is released.
    async fn await_unit_gone(&self, m: &Mount, timeout: Duration) -> Result<(), SupervisorError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.units.status(&m.unit_name()).await? {
                UnitStatus::Inactive | UnitStatus::Failed => return Ok(()),
                _ if std::time::Instant::now() >= deadline => {
                    return Err(SupervisorError::Supervision {
                        context: format!(
                            "{} is still shutting down after {}s",
                            m.unit_name(),
                            timeout.as_secs()
                        ),
                        source: None,
                    })
                }
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    async fn await_gone(&self, m: &Mount, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !mountinfo::is_mounted_at(&self.live_mounts(), &Self::resolved_point(m).await) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl<M: UnitManager> MountSupervisor for SystemdSupervisor<M> {
    fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
        Box::pin(async move {
            let m = self.mount_config(name)?;

            let point = Self::resolved_point(m).await;
            if mountinfo::is_mounted_at(&self.live_mounts(), &point) {
                // A mount point the kernel still lists but nothing is serving. Left
                // alone it blocks every future attempt, since rclone cannot mount over
                // it — so clear it rather than reporting success onto a dead mount.
                if self.is_stale(&point).await {
                    Self::fusermount(&point, false).await?;
                } else if self.units.status(&m.unit_name()).await? != UnitStatus::Inactive {
                    // Already serving, and it is ours. This is the path taken after a
                    // service restart, when every mount is already up.
                    return Ok(());
                } else {
                    // Something else is mounted there. Reporting success would tell the
                    // user their configured cache mode, read-only flag and rc socket were
                    // applied, when what is serving is a process we did not start.
                    return Err(SupervisorError::NotManaged(format!(
                        "{name}: {} is already mounted by something we did not start",
                        point.display()
                    )));
                }
            }

            Self::prepare_mount_point(&m.mount_point)?;
            self.prepare_runtime_dir()?;

            let unit = self.units.status(&m.unit_name()).await?;

            // A unit already running under this name means a mount is on its way up —
            // a second click during the readiness window, or two clients. Starting again
            // would fail with systemd's "unit already exists", which is both useless to
            // the user and wrong: the right answer is to wait for the one in flight.
            if matches!(unit, UnitStatus::Active | UnitStatus::Activating) {
                return self.await_ready(m).await;
            }
            // Still shutting down. Starting now loses the race against systemd freeing
            // the name, so wait for it rather than reporting a failure the user cannot
            // act on. This is the ordinary remount gesture.
            if unit == UnitStatus::Deactivating {
                self.await_unit_gone(m, self.unit_gone_timeout).await?;
            }

            // rclone binds its rc socket with a bare listen and will not replace a stale
            // one, so a socket left by a killed rclone makes every subsequent start fail
            // with "address already in use". A killed rclone is exactly the case, since Go
            // unlinks the socket only on a clean close.
            //
            // Safe unconditionally here: the branches above return or wait, so by this
            // point no process of ours holds that path. Unlinking a socket a *live* rclone
            // is serving would strand that mount at T4 for the life of the process.
            let socket = self.socket_path(&m.name);
            if socket.exists() {
                let _ = std::fs::remove_file(&socket);
            }

            // A previous failure leaves the unit loaded, and StartTransientUnit refuses a
            // name that already exists — without this, a mount could be retried once and
            // then never again.
            self.units.reset_failed(&m.unit_name()).await?;

            let spec = UnitSpec {
                name: m.unit_name(),
                description: format!(
                    "rclone mount {} at {}",
                    m.fs_spec(),
                    m.mount_point.display()
                ),
                executable: self.rclone.clone(),
                args: m.mount_args(&self.socket_path(&m.name)),
                // Ordinary, because rclone applies this to every file it creates inside
                // the mount. A mount with `allow_other` set so another service account
                // can read it would get 0600 files and fail with EACCES on all of them.
                umask: 0o022,
            };
            self.units.start_transient(&spec).await?;
            self.await_ready(m).await
        })
    }

    fn unmount<'a>(
        &'a self,
        name: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), SupervisorError>> {
        Box::pin(async move {
            let m = self.mount_config(name)?;
            let point = Self::resolved_point(m).await;
            let live = mountinfo::is_mounted_at(&self.live_mounts(), &point);
            let unit = self.units.status(&m.unit_name()).await?;
            // Anything but Inactive means a unit of ours exists — running, restarting, or
            // failed. Whether the mount point is currently serving is a separate question:
            // a unit can be looping without ever having mounted anything.
            let ours = unit != UnitStatus::Inactive;

            if live && !ours && !force {
                return Err(SupervisorError::NotManaged(name.to_string()));
            }
            if !live && !ours {
                return Ok(());
            }

            // The pending-upload check lands with the rc client (#12, #21, #23). It is a
            // warning rather than a refusal (#19): nothing is lost by unmounting, since
            // the write-back cache is on disk and resumes, so proceeding while the answer
            // is unknown is the correct default rather than a gap.

            if ours {
                // `StopUnit` only enqueues a job, so the unit has not stopped — let alone
                // failed — when this returns. Clearing the failed state has to wait until
                // it has actually settled, or it clears nothing: rclone exiting non-zero
                // on SIGTERM, or outliving the 30s stop timeout while flushing a large
                // write-back queue, both land in `failed` *after* an eager reset, leaving
                // a mount the user just unmounted reporting as failed.
                self.units.stop(&m.unit_name()).await?;
                if !live {
                    // Nothing was mounted; stopping the unit is the whole job. This is the
                    // path for a unit that was restart-looping without ever serving.
                    self.await_unit_gone(m, self.unit_gone_timeout).await?;
                    return self.units.reset_failed(&m.unit_name()).await;
                }
                if self.await_gone(m, self.gone_timeout).await {
                    // The mount point is released, but the name is not free until systemd
                    // finishes the job — and a remount immediately after would collide.
                    self.await_unit_gone(m, self.unit_gone_timeout).await?;
                    return self.units.reset_failed(&m.unit_name()).await;
                }
            }

            // Either it was foreign, or the unit stopped without releasing the mount
            // point, or it is a stale point left by an rclone that died.
            if !mountinfo::is_mounted_at(&self.live_mounts(), &point) {
                return Ok(());
            }
            // Ownership was decided from the unit name; the release acts on a path. Those
            // can disagree — a hand-edited `mount_point` (the documented workflow until
            // #42) leaves a unit serving one path while the config names another, and
            // releasing blind would tear down a filesystem the user never asked about,
            // possibly one another unit of ours is serving.
            let live_now = self.live_mounts();
            let entry = live_now
                .iter()
                .find(|e| e.is_rclone() && e.mount_point == point);
            if let Some(e) = entry {
                if e.source != m.fs_spec() {
                    return Err(SupervisorError::NotManaged(format!(
                        "{name}: {} is serving {}, not {} — refusing to unmount something \
                     this mount does not own",
                        point.display(),
                        e.source,
                        m.fs_spec()
                    )));
                }
            }
            Self::fusermount(&point, false).await?;
            if self.await_gone(m, Duration::from_secs(10)).await {
                return Ok(());
            }
            Err(SupervisorError::Busy {
                path: format!(
                    "{} is still mounted after fusermount -u; something is holding it open. \
                     A lazy unmount would detach it from processes still writing to it, so \
                     it is not done automatically.",
                    point.display()
                ),
            })
        })
    }

    fn state<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
        Box::pin(async move {
            let m = self.mount_config(name)?;
            self.resolve(m).await
        })
    }

    fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
        Box::pin(async move {
            let live = self.live_mounts();
            let mut out = Vec::new();

            for m in &self.config.mounts {
                out.push(DiscoveredMount::new(&m.name, self.resolve(m).await?));
            }

            // Live rclone mounts at paths we do not have configured. These are the whole
            // point of #18: they exist, they can be monitored, and pretending otherwise
            // is what makes a tray applet feel like it is lying.
            // Canonicalised, exactly as `resolve` compares them. Matching the raw path
            // here would list a mount under a symlinked directory twice — once as itself
            // and once, from the kernel's resolved path, as somebody else's.
            let mut configured: Vec<PathBuf> = Vec::with_capacity(self.config.mounts.len());
            for m in &self.config.mounts {
                configured.push(Self::resolved_point(m).await);
            }
            for e in live.iter().filter(|e| e.is_rclone()) {
                if configured.contains(&e.mount_point) {
                    continue;
                }
                out.push(DiscoveredMount::new(foreign_name(e), MountState::Foreign));
            }
            Ok(out)
        })
    }
}

/// A name for a mount we did not configure.
///
/// The full mount point, not its last component. Two foreign mounts can share a basename
/// (`/mnt/a/data` and `/mnt/b/data`), and a basename can equal a configured mount's name —
/// and since names are how clients address mounts, a collision means an action aimed at
/// one mount lands on another. A configured name cannot contain `/`, so a path can never
/// collide with one.
fn foreign_name(e: &MountEntry) -> String {
    e.mount_point.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_core::config::CacheMode;
    use std::sync::Mutex;

    /// Records what it was asked to do and reports whatever status the test sets.
    #[derive(Default)]
    struct FakeUnits {
        status: Mutex<UnitStatus>,
        started: Mutex<Vec<UnitSpec>>,
        stopped: Mutex<Vec<String>>,
        reset: Mutex<Vec<String>>,
        /// Rewritten with no mounts when a unit is stopped, so `await_gone` sees what it
        /// would see from a real rclone releasing its mount point.
        clears_on_stop: Mutex<Option<PathBuf>>,
    }

    impl UnitManager for FakeUnits {
        fn start_transient<'a>(
            &'a self,
            spec: &'a UnitSpec,
        ) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.started.lock().unwrap().push(spec.clone());
                Ok(())
            })
        }
        fn stop<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.stopped.lock().unwrap().push(unit.to_string());
                if let Some(p) = self.clears_on_stop.lock().unwrap().as_ref() {
                    std::fs::write(p, mountinfo_with(&[])).unwrap();
                }
                *self.status.lock().unwrap() = UnitStatus::Inactive;
                Ok(())
            })
        }
        fn status<'a>(
            &'a self,
            _unit: &'a str,
        ) -> BoxFuture<'a, Result<UnitStatus, SupervisorError>> {
            Box::pin(async move { Ok(*self.status.lock().unwrap()) })
        }
        fn reset_failed<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.reset.lock().unwrap().push(unit.to_string());
                Ok(())
            })
        }
        fn recent_output<'a>(&'a self, _unit: &'a str) -> BoxFuture<'a, String> {
            Box::pin(async { "rclone: couldn't connect: permission denied".to_string() })
        }
    }

    /// A real directory, because `mount` refuses a mount point it cannot create and the
    /// tests that exercise starting a unit have to get past that check.
    fn mount_point(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rvt-mp-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mountinfo_with(paths: &[&str]) -> String {
        let mut s = String::from("28 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n");
        for (i, p) in paths.iter().enumerate() {
            s.push_str(&format!(
                "{} 28 0:5{} / {} rw,relatime shared:7{} - fuse.rclone backup:pictures rw\n",
                150 + i,
                i,
                p,
                i
            ));
        }
        s
    }

    fn fixture(name: &str, contents: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("rvt-sup-{}-{name}.mountinfo", std::process::id()));
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn config_with_backup(mount_point: PathBuf) -> Arc<Config> {
        let mut c = Config::default();
        c.mounts.push(Mount {
            name: "backup".into(),
            remote: "backup".into(),
            path: "pictures".into(),
            mount_point,
            cache_mode: CacheMode::Writes,
            cache_max_size: None,
            cache_max_age: None,
            auto_mount: true,
            read_only: false,
            allow_other: false,
            uid: None,
            gid: None,
            umask: None,
            extra_args: Vec::new(),
        });
        Arc::new(c)
    }

    /// `mounted` controls whether the configured mount appears in mountinfo; `extra` adds
    /// live rclone mounts at paths the config does not know about.
    fn supervisor(
        tag: &str,
        mounted: bool,
        extra: &[&str],
        status: UnitStatus,
    ) -> SystemdSupervisor<FakeUnits> {
        let mp = mount_point(tag);
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("rvt-run-{}-{tag}", std::process::id())),
        );
        let mp_str = mp.to_string_lossy().into_owned();
        let mut live: Vec<&str> = Vec::new();
        if mounted {
            live.push(&mp_str);
        }
        live.extend_from_slice(extra);

        let units = FakeUnits::default();
        *units.status.lock().unwrap() = status;
        SystemdSupervisor::new(
            config_with_backup(mp.clone()),
            PathBuf::from("/usr/bin/rclone"),
            units,
            std::env::temp_dir().join(format!("rvt-run-{}-{tag}", std::process::id())),
        )
        .with_test_overrides(
            fixture(tag, &mountinfo_with(&live)),
            Duration::from_millis(300),
        )
    }

    #[tokio::test]
    async fn a_mount_we_started_is_ours() {
        let s = supervisor("ours", true, &[], UnitStatus::Active);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Mounted);
    }

    #[tokio::test]
    async fn a_mount_with_no_unit_of_ours_is_foreign() {
        // Same kernel evidence, no unit. This is the case that must not be reported as
        // ours, because acting on it would be acting on somebody else's mount.
        let s = supervisor("foreign", true, &[], UnitStatus::Inactive);
        let st = s.state("backup").await.unwrap();
        assert_eq!(st, MountState::Foreign);
        assert!(st.is_live() && !st.is_managed());
    }

    #[tokio::test]
    async fn nothing_mounted_is_unmounted_not_failed() {
        let s = supervisor("down", false, &[], UnitStatus::Inactive);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Unmounted);
    }

    #[tokio::test]
    async fn a_failed_unit_carries_rclones_own_words() {
        let s = supervisor("failed", false, &[], UnitStatus::Failed);
        match s.state("backup").await.unwrap() {
            MountState::Failed { reason } => {
                assert!(reason.contains("permission denied"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mounting_something_already_up_is_a_no_op() {
        // The service restarting must not try to remount everything it finds.
        let s = supervisor("already", true, &[], UnitStatus::Active);
        s.mount("backup").await.unwrap();
        assert!(
            s.units.started.lock().unwrap().is_empty(),
            "an already-serving mount must not be started again"
        );
    }

    #[tokio::test]
    async fn a_mount_that_never_appears_reports_why() {
        let s = supervisor("never", false, &[], UnitStatus::Inactive);
        match s.mount("backup").await {
            Err(SupervisorError::RcloneFailed { reason, .. }) => {
                assert!(reason.contains("permission denied"), "{reason}");
            }
            other => panic!("expected RcloneFailed, got {other:?}"),
        }
        // The unit must be reset first, or a second attempt could never start.
        assert_eq!(
            s.units.reset.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()]
        );
    }

    #[tokio::test]
    async fn the_rc_socket_is_protected_by_its_directory_not_by_the_units_umask() {
        // rclone applies the unit's umask to every file it creates inside the mount, so a
        // restrictive one here gives a mount with `allow_other` 0600 files that the
        // account it was shared with cannot read. The socket is kept private by the mode
        // of the directory holding it instead.
        let s = supervisor("umask", false, &[], UnitStatus::Inactive);
        let _ = s.mount("backup").await;

        let started = s.units.started.lock().unwrap();
        let spec = started.first().expect("a unit should have been started");
        assert_eq!(
            spec.umask, 0o022,
            "an ordinary umask, or files inside the mount become unreadable"
        );
        assert!(spec.args.iter().any(|a| a.starts_with("unix://")));
        drop(started);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&s.runtime_dir)
                .expect("the runtime directory should have been created")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "the rc socket directory must not be traversable by anyone else"
            );
        }
    }

    #[tokio::test]
    async fn unmounting_something_already_down_is_a_no_op() {
        let s = supervisor("gone", false, &[], UnitStatus::Inactive);
        s.unmount("backup", false).await.unwrap();
        assert!(s.units.stopped.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_foreign_mount_is_not_unmounted_without_force() {
        let s = supervisor("noforce", true, &[], UnitStatus::Inactive);
        match s.unmount("backup", false).await {
            Err(SupervisorError::NotManaged(n)) => assert_eq!(n, "backup"),
            other => panic!("expected NotManaged, got {other:?}"),
        }
        assert!(
            s.units.stopped.lock().unwrap().is_empty(),
            "nothing should have been stopped"
        );
    }

    /// Wire the fake so stopping the unit also clears the mount, as rclone does.
    fn releasing(s: &SystemdSupervisor<FakeUnits>) {
        *s.units.clears_on_stop.lock().unwrap() = Some(s.mountinfo_path.clone());
    }

    #[tokio::test]
    async fn unmounting_a_mount_we_started_stops_its_unit() {
        let s = supervisor("stops", true, &[], UnitStatus::Active);
        releasing(&s);
        s.unmount("backup", false).await.expect("our own mount");
        assert_eq!(
            s.units.stopped.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()],
            "the unit must actually be stopped"
        );
    }

    #[tokio::test]
    async fn a_unit_running_without_ever_serving_can_still_be_stopped() {
        // The wedge: rclone restart-loops without mounting anything. Nothing is live, so
        // an is_live() check alone returns Ok and leaves it respawning forever, with no
        // way to stop it from the applet.
        let s = supervisor("wedged", false, &[], UnitStatus::Activating);
        s.unmount("backup", false)
            .await
            .expect("a running unit of ours must be stoppable even with nothing mounted");
        assert_eq!(
            s.units.stopped.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()]
        );
        // and cleared, or the name cannot be reused
        assert_eq!(
            s.units.reset.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()]
        );
    }

    #[tokio::test]
    async fn a_failed_unit_is_stopped_and_cleared_so_the_mount_can_be_retried() {
        let s = supervisor("retryable", false, &[], UnitStatus::Failed);
        s.unmount("backup", false)
            .await
            .expect("a failed unit is ours");
        assert!(!s.units.stopped.lock().unwrap().is_empty());
        assert!(!s.units.reset.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_mount_being_torn_down_is_not_reported_as_somebody_elses() {
        // rclone flushes the write-back cache during its stop timeout, so this state can
        // last 30s of the user's own unmount.
        let s = supervisor("tearing", true, &[], UnitStatus::Deactivating);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Unmounting);
    }

    #[tokio::test]
    async fn our_own_crashed_mount_stays_ours() {
        let s = supervisor("crashed", true, &[], UnitStatus::Failed);
        match s.state("backup").await.unwrap() {
            MountState::Failed { .. } => {}
            other => panic!("a failed unit of ours must not read as Foreign, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_started_unit_with_nothing_mounted_yet_is_coming_up() {
        // `Type=exec` reports active the moment rclone is exec'd, seconds before the
        // mount point answers. Reporting that as Unmounted inverts the two states.
        let s = supervisor("comingup", false, &[], UnitStatus::Active);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Mounting);
    }

    #[tokio::test]
    async fn force_actually_attempts_to_release_a_foreign_mount() {
        // Without force this is refused. With it, the release must genuinely be attempted
        // — it fails here because the fixture is not a real filesystem, but "we did not
        // try" and "we tried and could not" are different answers.
        let s = supervisor("force", true, &[], UnitStatus::Inactive);
        let e = s
            .unmount("backup", true)
            .await
            .expect_err("there is no real filesystem to release");
        assert!(
            !matches!(e, SupervisorError::NotManaged(_)),
            "force must bypass the refusal, got {e:?}"
        );
        let msg = e.to_string();
        assert!(
            !msg.contains("still mounted after"),
            "that message means the release was never attempted: {msg}"
        );
    }

    #[tokio::test]
    async fn a_stale_socket_is_cleared_when_the_unit_died() {
        // The only state in which a stale socket can exist. rclone binds with a bare
        // listen and dies on EADDRINUSE, so leaving it makes the mount unstartable.
        let s = supervisor("staleock", false, &[], UnitStatus::Failed);
        std::fs::create_dir_all(&s.runtime_dir).unwrap();
        let sock = s.socket_path("backup");
        std::fs::write(&sock, b"").unwrap();

        let _ = s.mount("backup").await;
        assert!(
            !sock.exists(),
            "a socket left by a dead rclone must be removed before restarting it"
        );
    }

    #[tokio::test]
    async fn a_second_mount_while_one_is_coming_up_does_not_error() {
        // `StartTransientUnit` refuses a name that is already taken, and its error is a
        // raw D-Bus string. A second click during the 45s readiness window must wait for
        // the mount in flight instead.
        let s = supervisor("double", false, &[], UnitStatus::Activating);
        std::fs::create_dir_all(&s.runtime_dir).unwrap();
        let sock = s.socket_path("backup");
        std::fs::write(&sock, b"").unwrap();

        let e = s
            .mount("backup")
            .await
            .expect_err("nothing ever mounts here");
        assert!(
            !matches!(e, SupervisorError::Supervision { .. }),
            "a unit already coming up must not surface as an init-system error: {e:?}"
        );
        assert!(
            s.units.started.lock().unwrap().is_empty(),
            "it must not try to start a second unit under the same name"
        );
        // The socket of the rclone coming up must survive: unlinking it would strand that
        // mount at T4 for the life of the process, with nothing able to reach its rc API.
        assert!(
            sock.exists(),
            "a live rclone's socket must not be unlinked by a second mount attempt"
        );
    }

    #[tokio::test]
    async fn a_symlinked_mount_point_is_matched_against_the_resolved_path() {
        // The kernel records resolved paths. Comparing the configured one would report a
        // working mount as down, then fail to unmount it either.
        let real = std::env::temp_dir().join(format!("rvt-real-{}", std::process::id()));
        let link = std::env::temp_dir().join(format!("rvt-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut c = Config::default();
        c.mounts.push(Mount {
            mount_point: link.clone(),
            ..config_with_backup(real.clone()).mounts[0].clone()
        });
        let units = FakeUnits::default();
        *units.status.lock().unwrap() = UnitStatus::Active;
        let s = SystemdSupervisor::new(
            Arc::new(c),
            PathBuf::from("/usr/bin/rclone"),
            units,
            std::env::temp_dir().join(format!("rvt-run-link-{}", std::process::id())),
        )
        .with_test_overrides(
            fixture("symlink", &mountinfo_with(&[&real.to_string_lossy()])),
            Duration::from_millis(300),
        );

        assert_eq!(
            s.state("backup").await.unwrap(),
            MountState::Mounted,
            "the mount is up at the resolved path and must be recognised"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[tokio::test]
    async fn a_mount_point_that_no_longer_responds_is_reported_as_failed() {
        // The kernel still lists it, so a path check alone says "mounted". Every
        // operation on it returns ENOTCONN.
        let s = supervisor("stale", true, &[], UnitStatus::Inactive);
        let point = mount_point("stale");
        let s = s.with_stale(&[&point]);
        match s.state("backup").await.unwrap() {
            MountState::Failed { reason } => {
                assert!(reason.contains("not responding"), "{reason}");
            }
            other => panic!("a dead mount point must not read as {other:?}"),
        }
    }

    #[tokio::test]
    async fn mounting_over_a_dead_mount_point_clears_it_first() {
        // rclone cannot mount over an occupied path, so without this the mount is
        // unrecoverable from the applet: every attempt returns success onto a dead mount.
        let s = supervisor("staleclear", true, &[], UnitStatus::Inactive);
        let point = mount_point("staleclear");
        let s = s.with_stale(&[&point]);
        let e = s
            .mount("backup")
            .await
            .expect_err("there is no real filesystem here to release");
        let msg = e.to_string();
        assert!(
            !msg.contains("already mounted by something we did not start"),
            "a stale point is ours to clear, not somebody else's mount: {msg}"
        );
    }

    #[tokio::test]
    async fn mounting_waits_out_a_unit_that_is_still_stopping() {
        // The remount gesture. rclone can hold the unit name for the whole 30s stop
        // timeout while it flushes, and starting into that returns systemd's raw
        // "unit already exists".
        let s = supervisor("remount", false, &[], UnitStatus::Deactivating);
        let e = s.mount("backup").await.expect_err("the fake never settles");
        match e {
            SupervisorError::Supervision { context, .. } => assert!(
                context.contains("still shutting down"),
                "it must wait for the name, not collide with it: {context}"
            ),
            other => panic!("expected a wait-then-give-up, got {other:?}"),
        }
        assert!(
            s.units.started.lock().unwrap().is_empty(),
            "nothing may be started while the old unit still holds the name"
        );
    }

    #[tokio::test]
    async fn a_failed_unit_ends_the_wait_instead_of_burning_the_timeout() {
        // Without the early exit the user waits the full readiness window to be told
        // something systemd already knew.
        let s = supervisor("earlyexit", false, &[], UnitStatus::Failed).with_test_overrides(
            fixture("earlyexit2", &mountinfo_with(&[])),
            Duration::from_secs(20),
        );
        let started = std::time::Instant::now();
        let _ = s.mount("backup").await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}: a unit that has already failed will never become ready",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn unmount_refuses_a_path_serving_a_different_remote() {
        // Ownership is decided from the unit name but the release acts on a path. A
        // hand-edited mount_point can put those out of step, and releasing blind would
        // tear down a filesystem the user never named.
        let mp = mount_point("mismatch");
        let mut c = Config::default();
        c.mounts.push(Mount {
            remote: "somethingelse".into(),
            ..config_with_backup(mp.clone()).mounts[0].clone()
        });
        let units = FakeUnits::default();
        *units.status.lock().unwrap() = UnitStatus::Active;
        let s = SystemdSupervisor::new(
            Arc::new(c),
            PathBuf::from("/usr/bin/rclone"),
            units,
            std::env::temp_dir().join(format!("rvt-run-mm-{}", std::process::id())),
        )
        .with_test_overrides(
            // mountinfo says backup:pictures is what is actually there.
            fixture("mismatch", &mountinfo_with(&[&mp.to_string_lossy()])),
            Duration::from_millis(300),
        );

        match s.unmount("backup", false).await {
            Err(SupervisorError::NotManaged(why)) => {
                assert!(why.contains("is serving"), "{why}");
            }
            other => panic!("expected a refusal naming the mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reconcile_reports_configured_and_foreign_mounts() {
        let other = "/tmp/somebody-elses";
        let s = supervisor("reconcile", true, &[other], UnitStatus::Active);
        let found = s.reconcile().await.unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].name, "backup");
        assert_eq!(found[0].state, MountState::Mounted);
        // The unconfigured one is reported rather than ignored, and named by its full
        // path so it cannot collide with a configured name or another foreign mount.
        assert_eq!(found[1].name, other);
        assert_eq!(found[1].state, MountState::Foreign);
    }

    #[tokio::test]
    async fn a_configured_mount_that_is_down_is_still_listed() {
        // Omitting it would make a down mount indistinguishable from a deleted one.
        let s = supervisor("listdown", false, &[], UnitStatus::Inactive);
        let found = s.reconcile().await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn an_unknown_mount_is_named_in_the_error() {
        let s = supervisor("unknown", false, &[], UnitStatus::Inactive);
        match s.state("nope").await {
            Err(SupervisorError::UnknownMount(n)) => assert_eq!(n, "nope"),
            other => panic!("expected UnknownMount, got {other:?}"),
        }
    }
}
