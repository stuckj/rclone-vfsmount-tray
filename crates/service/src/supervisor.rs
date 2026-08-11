//! The [`MountSupervisor`] implementation.
//!
//! Every state answer comes from `/proc/self/mountinfo` rather than from what this
//! process believes it started, which makes a mount that outlived the service and a mount
//! somebody else started the same problem. See DESIGN.md.

use crate::systemd::{UnitManager, UnitSpec, UnitStatus};
use rvt_core::config::UNIT_PREFIX;
use rvt_core::mountinfo::{self, MountEntry};
use rvt_core::supervisor::{
    BoxFuture, DiscoveredMount, MountState, MountSupervisor, SupervisorError,
};
use rvt_core::{Config, Mount};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long to wait for a mount point to start serving. Covers an OAuth refresh and the
/// first listing of a cold remote, not just process startup.
const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// `ENOTCONN`. What every operation on a mount point returns once the FUSE daemon
/// serving it has gone away without unmounting.
const ENOTCONN: i32 = 107;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long to wait on a filesystem call that resolves through a mount point. Only long
/// enough to stop a wedged mount holding the executor, not to diagnose it.
const FS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How hard to try when asking the kernel for a mount point back.
///
/// Measured on rclone v1.75.0 and Linux 6.8, 15 MB written to a file still open: `-u` is
/// refused while rclone is alive and **still refused after it is killed** — the writer's
/// descriptor pins the mount, not the daemon. Only `-z` takes it. Separate operations
/// rather than a flag because they are not interchangeable. See DESIGN.md.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Release {
    /// `fusermount3 -u`. Refused while any process is using the mount, which is the only
    /// signal an in-progress write gives.
    Refuse,
    /// `fusermount3 -u -z`. Detaches whatever holds it, so a writer mid-file loses the
    /// rest. Only after *our own* unit has stopped, and only under `force`.
    Detach,
}

/// The unit an operation acts on, and what is needed to act on it.
///
/// A configured mount reduces to one of these, and so does an orphan — a unit of ours
/// the config no longer names, rebuilt from the argv systemd still holds for it. Past
/// this point the two are handled identically, which is the whole of #71: an orphan is
/// stopped by stopping its unit, not by fusermounting the path it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// The mount name, which is also the lock this operation holds.
    name: String,
    unit: String,
    /// Canonicalised, as the kernel reports it.
    point: PathBuf,
    /// `remote:path`, to check against what is actually mounted there.
    fs_spec: String,
}

/// Starts rclone mounts as transient systemd units.
pub struct SystemdSupervisor<M: UnitManager> {
    config: Arc<Config>,
    rclone: PathBuf,
    units: M,
    /// Overridden in tests. Everything else reads the real kernel interface.
    mountinfo_path: PathBuf,
    /// Where rc sockets live — `$XDG_RUNTIME_DIR/rclone-vfsmount-tray`.
    runtime_dir: PathBuf,
    /// The config this service was loaded from, passed to the pre-start hook.
    ///
    /// A transient unit runs from the systemd user manager's environment, so it inherits
    /// neither `XDG_CONFIG_HOME` nor `--config`.
    config_path: PathBuf,
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
    /// Stands in for the kernel probe, since holding a mount for real needs FUSE and
    /// tier-1 CI has none. `None` runs the real `fusermount3`, which is what a test
    /// building its own supervisor gets. Not emptied on stop — see [`Release`].
    #[cfg(test)]
    busy_paths: Option<Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>>,
    /// Ordered record of the probe and the unit stop, shared with the fake unit manager.
    /// Which of the two ran first is the whole of #73, and nothing else observes it.
    #[cfg(test)]
    events: Arc<std::sync::Mutex<Vec<String>>>,
    /// One lock per mount name.
    ///
    /// Two clients exist by design, and `mount` yields between reading the unit status
    /// and starting the unit, so without this both can see `Inactive` and both start.
    locks: tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl<M: UnitManager> SystemdSupervisor<M> {
    pub fn new(
        config: Arc<Config>,
        rclone: PathBuf,
        units: M,
        runtime_dir: PathBuf,
        config_path: PathBuf,
    ) -> Self {
        Self {
            config,
            rclone,
            units,
            mountinfo_path: PathBuf::from("/proc/self/mountinfo"),
            runtime_dir,
            config_path,
            ready_timeout: READY_TIMEOUT,
            gone_timeout: Duration::from_secs(30),
            unit_gone_timeout: Duration::from_secs(35),
            #[cfg(test)]
            stale_paths: std::collections::HashSet::new(),
            #[cfg(test)]
            busy_paths: None,
            #[cfg(test)]
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
            locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
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

    /// Answer the kernel probe from `busy` instead of running `fusermount3`. A path in the
    /// set is refused, as one with a file open is.
    #[cfg(test)]
    fn with_busy(
        mut self,
        busy: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
    ) -> Self {
        self.busy_paths = Some(busy);
        self
    }

    #[cfg(test)]
    fn with_events(mut self, events: Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        self.events = events;
        self
    }

    /// Ask the kernel for the mount point back. See [`Release`] and DESIGN.md.
    async fn release_point(&self, point: &Path, how: Release) -> Result<(), SupervisorError> {
        #[cfg(test)]
        if let Some(busy) = self.busy_paths.as_ref() {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{how:?} {}", point.display()));
            let held = busy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(point);
            // Held points refuse `-u` alive or dead, and yield to `-z`. See [`Release`].
            if held && how == Release::Refuse {
                return Err(SupervisorError::Busy {
                    detail: format!(
                        "{} could not be unmounted: fusermount3: failed to unmount {}: \
                         Device or resource busy",
                        point.display(),
                        point.display()
                    ),
                });
            }
            return self.drop_from_fixture(point);
        }
        Self::fusermount(point, how).await
    }

    /// Take one mount point out of the mountinfo fixture, as a release the kernel accepted
    /// takes it out of `/proc`. Other mounts in the fixture stay: a test may have put them
    /// there precisely to check they are left alone.
    #[cfg(test)]
    fn drop_from_fixture(&self, point: &Path) -> Result<(), SupervisorError> {
        let body =
            std::fs::read_to_string(&self.mountinfo_path).map_err(|e| Self::fixture_err(&e))?;
        let kept: String = body
            .lines()
            .filter(|l| {
                mountinfo::parse(l)
                    .first()
                    .is_none_or(|e| e.mount_point != point)
            })
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(&self.mountinfo_path, kept).map_err(|e| Self::fixture_err(&e))
    }

    #[cfg(test)]
    fn fixture_err(e: &std::io::Error) -> SupervisorError {
        SupervisorError::Supervision {
            context: format!("rewriting the mountinfo fixture: {e}"),
            source: None,
        }
    }

    /// The rc socket for a mount.
    pub fn socket_path(&self, name: &str) -> PathBuf {
        rc_socket_path(&self.runtime_dir, name)
    }

    /// The `ExecStartPre` that clears leftovers before rclone is exec'd.
    ///
    /// `None` when this binary cannot be located: the mount still starts, it just will
    /// not auto-recover from a hard kill. `/proc/self/exe` reads `<path> (deleted)` once
    /// the file has been replaced, which is what a package upgrade does.
    fn pre_start_hook(&self, name: &str) -> Option<(PathBuf, Vec<String>)> {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, mount = name,
                    "cannot locate this binary; the mount will start but systemd will not \
                     be able to restart it after a hard kill");
                return None;
            }
        };
        // A replaced binary leaves a path that can never be exec'd again.
        if !exe.is_file() || exe.to_string_lossy().ends_with(" (deleted)") {
            tracing::warn!(path = %exe.display(), mount = name,
                "this binary is no longer at the path it was started from — probably an \
                 upgrade; the mount will start but will not auto-recover from a hard kill");
            return None;
        }
        Some((
            exe,
            vec![
                // Before the subcommand: `--config` is not a global argument.
                "--config".to_string(),
                self.config_path.to_string_lossy().into_owned(),
                "prepare-mount".to_string(),
                "--name".to_string(),
                name.to_string(),
            ],
        ))
    }

    /// The lock guarding operations on one mount.
    async fn lock_for(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.locks.lock().await;
        map.entry(name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn mount_config(&self, name: &str) -> Result<&Mount, SupervisorError> {
        self.config
            .mounts
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| SupervisorError::UnknownMount(name.to_string()))
    }

    /// A mount point as the kernel will report it.
    ///
    /// mountinfo records fully resolved paths, so a mount under a symlinked directory
    /// never matches its configured path. Falls back to that path when it cannot be
    /// resolved, the normal case before the directory exists.
    async fn resolved(raw: PathBuf) -> PathBuf {
        let fallback = raw.clone();
        Self::off_thread(move || std::fs::canonicalize(&raw).ok())
            .await
            .flatten()
            .unwrap_or(fallback)
    }

    async fn resolved_point(m: &Mount) -> PathBuf {
        Self::resolved(m.mount_point.clone()).await
    }

    async fn target_of(m: &Mount) -> Target {
        Target {
            name: m.name.clone(),
            unit: m.unit_name(),
            point: Self::resolved_point(m).await,
            fs_spec: m.fs_spec(),
        }
    }

    /// Every unit of ours that is serving something the config no longer names.
    ///
    /// The sweep `UNIT_PREFIX` promises: without it a renamed mount leaves its old unit
    /// running and unaccounted for, and the mount point it holds reads as somebody
    /// else's.
    ///
    /// Being named by no config entry is not enough to make a unit the owner of a mount
    /// point: its argv records only where it *meant* to mount, so a leftover would claim
    /// whatever turns up at that path afterwards, and stopping it would take down a mount
    /// that was never its own. It must also be [serving](UnitStatus::is_serving), and not
    /// be shadowing a configured mount whose own unit is up.
    ///
    /// What is mounted there is deliberately **not** required to match the `remote:path`
    /// the unit was given. mountinfo carries the Fs rclone resolved, not the argument it
    /// was handed: measured on v1.75.0, an `alias` remote mounted as `ali:` reports its
    /// backing path instead, and a trailing slash in the argument is dropped. Demanding
    /// equality excused the entire sweep for those configs — the failure this exists to
    /// fix, left in place with nothing to show for it.
    async fn orphans(&self) -> Result<Vec<Target>, SupervisorError> {
        let loaded = self.units.list_units(UNIT_PREFIX).await?;
        let live = self.live_mounts();
        let mut configured = Vec::with_capacity(self.config.mounts.len());
        for m in &self.config.mounts {
            configured.push((Self::resolved_point(m).await, m.unit_name()));
        }

        let mut out = Vec::new();
        for u in loaded {
            if !u.status.is_serving() || configured.iter().any(|(_, c)| *c == u.name) {
                continue;
            }
            // Nothing to act on without knowing what it serves: stopping a unit whose
            // mount point is unknown could take down anything.
            let (Some(serving), Some(name)) = (u.serving, orphan_name(&u.name)) else {
                continue;
            };
            let point = Self::resolved(serving.mount_point).await;
            if !mountinfo::is_mounted_at(&live, &point) {
                continue;
            }
            // After a rename the old unit and the new one name the same path with the
            // same remote, so nothing about the mount itself separates them: the one to
            // believe is the one whose config entry still exists and whose unit is up.
            if self.configured_unit_serving_at(&configured, &point).await? {
                continue;
            }
            out.push(Target {
                name,
                unit: u.name,
                point,
                fs_spec: serving.fs_spec,
            });
        }
        Ok(out)
    }

    /// Whether a configured mount at one of these points has a unit of its own serving
    /// it. Points come in already resolved: canonicalising them per candidate would put
    /// one full probe timeout, and one parked blocking thread, behind every wedged mount
    /// in the config.
    async fn configured_unit_serving_at(
        &self,
        configured: &[(PathBuf, String)],
        point: &Path,
    ) -> Result<bool, SupervisorError> {
        for (p, unit) in configured {
            if p == point && self.units.status(unit).await?.is_serving() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Where a unit of ours actually mounted, from the argv systemd still holds for it.
    ///
    /// The one fact about a running mount that neither a config edit nor rclone rewrites.
    /// `None` when systemd no longer has the unit, or its argv cannot be read.
    async fn argv_point(&self, unit: &str) -> Result<Option<PathBuf>, SupervisorError> {
        let Some(serving) = self
            .units
            .list_units(UNIT_PREFIX)
            .await?
            .into_iter()
            .find(|u| u.name == unit)
            .and_then(|u| u.serving)
        else {
            return Ok(None);
        };
        Ok(Some(Self::resolved(serving.mount_point).await))
    }

    /// The orphan holding a mount point, if one is.
    async fn orphan_at(&self, point: &Path) -> Result<Option<Target>, SupervisorError> {
        Ok(self.orphans().await?.into_iter().find(|o| o.point == point))
    }

    /// Refuse to mount over, or release, a point one of our own units still holds.
    ///
    /// Taking it either way would leave that unit running against a path it no longer
    /// owns, and the user cannot act on a refusal that does not say what is in the way.
    async fn refuse_to_take_over(&self, name: &str, point: &Path) -> Result<(), SupervisorError> {
        match self.orphan_at(point).await? {
            None => Ok(()),
            Some(o) => Err(SupervisorError::NotManaged(format!(
                "{name}: {} is still held by {}, left over from a mount this config no \
                 longer names. Unmount {:?} first.",
                point.display(),
                o.unit,
                o.name
            ))),
        }
    }

    /// Which unit an unmount request has to act on.
    ///
    /// Usually the one the configured mount names. It can also be an orphan: addressed
    /// directly, under the name its unit was started with, or reached through a
    /// configured mount whose point that orphan is the one still holding — what a rename
    /// leaves behind. Both have to stop the unit: taken for foreign instead, the mount
    /// cannot be unmounted without `force`, and `force` cannot escalate past a holder,
    /// because [`Release::Detach`] is gated on the mount being ours.
    async fn unmount_target(&self, name: &str) -> Result<Target, SupervisorError> {
        let Ok(m) = self.mount_config(name) else {
            return self
                .orphans()
                .await?
                .into_iter()
                .find(|o| o.name == name)
                .ok_or_else(|| SupervisorError::UnknownMount(name.to_string()));
        };

        let target = Self::target_of(m).await;
        // Serving under its own name, so it is the unit to act on. A unit that is failed,
        // or waiting to restart, holds nothing: whatever is at the point belongs to
        // somebody else, and acting on this one would release their mount and then stop
        // this one instead.
        if self.units.status(&target.unit).await?.is_serving() {
            return Ok(target);
        }
        match self.orphan_at(&target.point).await? {
            Some(o) => {
                tracing::info!(
                    mount = %target.name,
                    unit = %o.unit,
                    "stopping the unit still holding this mount point; the config no \
                     longer names it"
                );
                Ok(o)
            }
            None => Ok(target),
        }
    }

    /// Run a filesystem call off the async executor, and give up on it if it hangs.
    ///
    /// Every `stat` here resolves *through* a mount point, and an rclone that is alive
    /// but not answering blocks it uninterruptibly. `None` means it did not answer in
    /// time; the blocking-pool thread stays stuck, but the executor does not.
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

    /// Whether a mount point is present but no longer served. A FUSE daemon that dies
    /// without unmounting leaves its entry behind, answering `ENOTCONN` — indistinguishable
    /// from a healthy mount by path alone.
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
        // No evidence of any mount, not an error — but it also disables the guard
        // against mounting over somebody else's filesystem, so it must not be quiet.
        match mountinfo::read_from(&self.mountinfo_path) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    path = %self.mountinfo_path.display(),
                    error = %e,
                    "could not read mountinfo; every mount will report as absent this poll"
                );
                Vec::new()
            }
        }
    }

    /// Resolve one mount's state. The kernel says whether anything is mounted there; the
    /// unit says whether it is ours.
    async fn resolve(&self, m: &Mount) -> Result<MountState, SupervisorError> {
        let point = Self::resolved_point(m).await;
        let live = mountinfo::is_mounted_at(&self.live_mounts(), &point);

        // Still "mounted" to the kernel. Reporting it as Foreign would leave it neither
        // startable nor stoppable.
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
            // A teardown the user asked for. Reporting it as anything else would tell them
            // their mount had become somebody else's mid-operation.
            (true, UnitStatus::Deactivating) => MountState::Unmounting,
            // Ours, and it died with the kernel entry still there. Still ours.
            (true, UnitStatus::Failed) => MountState::Failed {
                reason: self.failure_reason(&m.unit_name()).await,
            },
            // Mounted with no unit of ours *under this name*. Either somebody else
            // started it, or one of our own units is still holding the point under the
            // name this mount used to have.
            (true, UnitStatus::Inactive) => match self.orphan_at(&point).await? {
                Some(o) => MountState::Failed {
                    reason: format!(
                        "{} is still served by {}, left over from a mount this config no \
                         longer names — usually a rename. Unmounting {:?} frees it.",
                        point.display(),
                        o.unit,
                        o.name
                    ),
                },
                None => MountState::Foreign,
            },

            // `Type=exec` reports active as soon as rclone is exec'd, seconds before the
            // mount point answers, so an active unit with nothing mounted yet is coming
            // up rather than down.
            (false, UnitStatus::Active | UnitStatus::Activating) => MountState::Mounting,
            (false, UnitStatus::Deactivating) => MountState::Unmounting,
            (false, UnitStatus::Failed) => MountState::Failed {
                reason: self.failure_reason(&m.unit_name()).await,
            },
            (false, UnitStatus::Inactive) => MountState::Unmounted,
        })
    }

    /// Wait for the mount point to start serving. Polls the kernel, not the unit:
    /// `Type=exec` reports active seconds before the mount point answers.
    async fn await_ready(&self, m: &Mount) -> Result<(), SupervisorError> {
        let deadline = std::time::Instant::now() + self.ready_timeout;
        // Resolved once: the path cannot change under us during the wait, and doing it
        // per poll puts a blocking `canonicalize` on the pool every 250ms.
        let point = Self::resolved_point(m).await;
        loop {
            if mountinfo::is_mounted_at(&self.live_mounts(), &point) {
                return Ok(());
            }
            // A unit that has already failed will never become ready; waiting out the
            // full timeout would only delay showing the user why.
            if self.units.status(&m.unit_name()).await? == UnitStatus::Failed {
                return Err(SupervisorError::RcloneFailed {
                    reason: self.failure_reason(&m.unit_name()).await,
                    source: None,
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(SupervisorError::RcloneFailed {
                    reason: format!(
                        "{} did not start serving within {}s. {}",
                        m.mount_point.display(),
                        self.ready_timeout.as_secs(),
                        self.failure_reason(&m.unit_name()).await
                    )
                    .trim_end()
                    .to_string(),
                    source: None,
                });
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn failure_reason(&self, unit: &str) -> String {
        let out = self.units.recent_output(unit).await;
        if out.is_empty() {
            format!("rclone logged nothing; check `journalctl --user -u {unit}`")
        } else {
            out
        }
    }

    /// Create the runtime directory private to this user.
    ///
    /// This is what protects the rc sockets: rclone does no chmod when binding, and a
    /// directory nobody else can traverse makes the socket's own mode moot. The unit's
    /// umask cannot be used instead — rclone applies it to files inside the mount too.
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

    /// Make sure the mount point is a directory we can mount onto. Created when missing;
    /// a wrong path fails at mount time either way.
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

    /// Unmount a path without going through systemd — for foreign mounts, and when
    /// stopping the unit leaves the point behind.
    async fn fusermount(path: &Path, how: Release) -> Result<(), SupervisorError> {
        let mut args = vec!["-u"];
        if how == Release::Detach {
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
            // A complete sentence, because this escapes unwrapped from `mount` and the
            // pre-start hook as well as through `refused`.
            Ok(o) => Err(SupervisorError::Busy {
                detail: format!(
                    "{} could not be unmounted: {}",
                    path.display(),
                    String::from_utf8_lossy(&o.stderr)
                        .trim()
                        .trim_end_matches('.')
                ),
            }),
            Err(e) => Err(SupervisorError::Supervision {
                context: format!("running fusermount for {}", path.display()),
                source: Some(Box::new(e)),
            }),
        }
    }

    /// Add what to do about it to a refusal the kernel gave.
    ///
    /// Hedged with "usually" because `fusermount` reports every non-zero exit the same
    /// way, so a refusal cannot be told from "not a mount point". A failure to *run* it
    /// passes through untouched. Not "close the file": measured, a working directory
    /// inside the mount or a read-only descriptor is enough to be refused.
    fn refused(point: &Path, e: SupervisorError) -> SupervisorError {
        let SupervisorError::Busy { detail: reason } = e else {
            return e;
        };
        SupervisorError::Busy {
            detail: format!(
                "{reason}. Usually a process is still using the mount — a file open under \
                 it, or a shell whose working directory is inside it. `fuser -m {}` names \
                 them. Unmounting anyway cuts anything mid-write off, and rclone then \
                 uploads the partial file as if it were complete.",
                point.display()
            ),
        }
    }

    /// Wait for the unit to stop occupying its name. `StopUnit` only enqueues a job, so
    /// the name is still taken when it returns, and a remount straight after would collide
    /// with it.
    async fn await_unit_gone(&self, unit: &str, timeout: Duration) -> Result<(), SupervisorError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.units.status(unit).await? {
                UnitStatus::Inactive | UnitStatus::Failed => return Ok(()),
                _ if std::time::Instant::now() >= deadline => {
                    return Err(SupervisorError::Supervision {
                        context: format!(
                            "{unit} is still shutting down after {}s",
                            timeout.as_secs()
                        ),
                        source: None,
                    })
                }
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    async fn await_gone(&self, point: &Path, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !mountinfo::is_mounted_at(&self.live_mounts(), point) {
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
            let lock = self.lock_for(name).await;
            let _guard = lock.lock().await;

            let point = Self::resolved_point(m).await;
            if mountinfo::is_mounted_at(&self.live_mounts(), &point) {
                // A mount point the kernel still lists but nothing is serving. Left
                // alone it blocks every future attempt, since rclone cannot mount over
                // it — so clear it rather than reporting success onto a dead mount.
                if self.is_stale(&point).await {
                    // Unless a unit of ours under a dropped name is still behind it: an
                    // rclone whose FUSE connection has been aborted answers `ENOTCONN`
                    // while systemd still reports its unit `active`. The release does not
                    // ask who owns the point, so it is asked here. A unit that has
                    // *exited* is not caught — it is no longer serving, by definition —
                    // and clearing the point it left is the whole purpose of this branch.
                    self.refuse_to_take_over(name, &point).await?;
                    self.release_point(&point, Release::Refuse).await?;
                } else {
                    let status = self.units.status(&m.unit_name()).await?;
                    // Nothing of ours is serving under this name, so something else is at
                    // the point. Ask what, before starting anything across it.
                    if !status.is_serving() {
                        self.refuse_to_take_over(name, &point).await?;
                    }
                    match status {
                        // Already serving, and it is ours. This is the path taken after a
                        // service restart, when every mount is already up.
                        UnitStatus::Active | UnitStatus::Activating => return Ok(()),
                        // Still mounted, but on its way out. Waiting is what stops the
                        // remount colliding with the name systemd has not freed yet.
                        UnitStatus::Deactivating => {
                            self.await_unit_gone(&m.unit_name(), self.unit_gone_timeout)
                                .await?;
                        }
                        UnitStatus::Failed => {}
                        UnitStatus::Inactive => {
                            // Something else is mounted there. Reporting success would tell the
                            // user their configured cache mode, read-only flag and rc socket were
                            // applied, when what is serving is a process we did not start.
                            return Err(SupervisorError::NotManaged(format!(
                                "{name}: {} is already mounted by something we did not start",
                                point.display()
                            )));
                        }
                    }
                }
            }

            Self::prepare_mount_point(&m.mount_point)?;
            self.prepare_runtime_dir()?;

            let unit = self.units.status(&m.unit_name()).await?;

            // A mount already on its way up — a second click, or two clients. Starting
            // again returns systemd's raw "unit already exists"; wait for it instead.
            if matches!(unit, UnitStatus::Active | UnitStatus::Activating) {
                return self.await_ready(m).await;
            }
            // Still shutting down. Starting now loses the race against systemd freeing
            // the name, so wait for it rather than reporting a failure the user cannot
            // act on. This is the ordinary remount gesture.
            if unit == UnitStatus::Deactivating {
                self.await_unit_gone(&m.unit_name(), self.unit_gone_timeout)
                    .await?;
            }

            // rclone binds with a bare listen and dies on EADDRINUSE rather than
            // replacing a stale socket, and Go unlinks one only on a clean close.
            // Unconditional is safe here: the branches above return or wait.
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
                // This binary, re-invoked to clear a stale socket or mount point.
                pre_start: self.pre_start_hook(&m.name),
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
            let lock = self.lock_for(name).await;
            let _guard = lock.lock().await;
            // Not always the unit this name's own config entry would build: see
            // `unmount_target`. Everything below acts on what came back.
            let target = match self.unmount_target(name).await {
                Ok(t) => t,
                Err(e) => {
                    // `lock_for` inserts on demand, and the name is only known to be good
                    // once it has resolved. Nothing can ever act under a name that
                    // resolves to nothing, so its entry is dropped rather than left to
                    // accumulate one per bad name a client sends.
                    if matches!(e, SupervisorError::UnknownMount(_)) {
                        self.locks.lock().await.remove(name);
                    }
                    return Err(e);
                }
            };

            // A redirect acts on a unit the caller's own lock does not guard, and after a
            // rename two names reach the same one — the configured entry and the orphan.
            // Without this, both run the release-stop-clear sequence at once. Redirects
            // only ever run configured name to orphan name, never the reverse, so holding
            // the two in this order cannot cycle.
            let redirect_lock = if target.name != name {
                Some(self.lock_for(&target.name).await)
            } else {
                None
            };
            let _redirect_guard = match redirect_lock.as_ref() {
                Some(l) => Some(l.lock().await),
                None => None,
            };

            let Target {
                name,
                unit,
                point,
                fs_spec,
            } = target;

            let live = mountinfo::is_mounted_at(&self.live_mounts(), &point);
            let status = self.units.status(&unit).await?;
            // Anything but Inactive means a unit of ours exists — running, restarting, or
            // failed. Whether the mount point is currently serving is a separate question:
            // a unit can be looping without ever having mounted anything.
            let ours = status != UnitStatus::Inactive;

            if live && !ours && !force {
                return Err(SupervisorError::NotManaged(name));
            }
            if !live && !ours {
                return Ok(());
            }

            // Detaching is only right once the writer is gone, so: only after we stopped
            // the unit ourselves. `-z` on a foreign mount would strand a live rclone
            // serving a mount nothing can see. See DESIGN.md.
            let may_detach = force && ours;

            // Ownership was decided from the unit name; the release acts on a path. A
            // hand-edited `mount_point` puts those out of step, and releasing blind would
            // tear down a filesystem the user never named. This runs before anything
            // touches the unit: refusing after the SIGTERM has gone out refuses nothing.
            if live {
                let live_now = self.live_mounts();
                let entry = live_now
                    .iter()
                    .find(|e| e.is_rclone() && e.mount_point == point);
                // Unless the unit's own argv names this very point, which settles it:
                // that is where this unit actually mounted. Falling back to the source
                // below is a guess, because rclone records the Fs it resolved rather
                // than the argument it was given — an `alias` remote reports its backing
                // path — so on those configs it refuses a mount that is plainly ours.
                let established = self.argv_point(&unit).await? == Some(point.clone());
                if let Some(e) = entry.filter(|_| !established) {
                    // `force` is the caller having confirmed with the user, so it overrides
                    // the mismatch as well as the ownership refusal — otherwise #18's
                    // "unmount anyway" cannot work on a foreign mount at all.
                    if !force && e.source != fs_spec {
                        return Err(SupervisorError::NotManaged(format!(
                            "{name}: {} is serving {}, not {} — refusing to unmount something \
                         this mount does not own",
                            point.display(),
                            e.source,
                            fs_spec
                        )));
                    }
                }

                // The kernel decides, before rclone is signalled. Why this has to come
                // first, and why any failure refuses, is in DESIGN.md under "the unmount
                // order". #73.
                match self.release_point(&point, Release::Refuse).await {
                    Ok(()) => {}
                    Err(e) => {
                        // The point can go away between the liveness read above and here —
                        // an rclone crash, or a concurrent `systemctl stop`. A path that is
                        // no longer mounted is not a failure to unmount it.
                        if !mountinfo::is_mounted_at(&self.live_mounts(), &point) {
                            // Gone anyway.
                        } else if may_detach {
                            // Overriding the one signal an in-progress write ever gives, so
                            // it does not pass quietly. Logged here rather than at the
                            // detach itself because this is the decision; by then it has
                            // already happened.
                            tracing::warn!(
                                mount = %name,
                                path = %point.display(),
                                error = %e,
                                "forced unmount over a refusal to release: anything writing \
                                 through this mount will be cut off mid-file, and rclone will \
                                 then upload the partial file as if it were complete"
                            );
                        } else {
                            // Includes `force` on a mount that is not ours: see `may_detach`.
                            return Err(Self::refused(&point, e));
                        }
                    }
                }
            }

            if ours {
                // `StopUnit` only enqueues a job, so the unit reaches `failed` — on a
                // non-zero exit or a stop timeout — after this returns. Clearing before it
                // settles clears nothing.
                self.units.stop(&unit).await?;
                // The wait is not only for rclone to exit: a holder that lets go inside it
                // is the difference between the escalation below succeeding and refusing.
                if !live || self.await_gone(&point, self.gone_timeout).await {
                    // Either nothing was mounted — the path for a unit restart-looping
                    // without ever serving — or the point is released. The name is not
                    // free until systemd finishes the job, and a remount immediately
                    // after would collide.
                    self.await_unit_gone(&unit, self.unit_gone_timeout).await?;
                    return self.units.reset_failed(&unit).await;
                }
            }

            // Either it was foreign, or the unit stopped without releasing the mount
            // point, or it is a stale point left by an rclone that died.
            if !mountinfo::is_mounted_at(&self.live_mounts(), &point) {
                return Ok(());
            }
            // Same call as the probe above. Usually a different answer by now — rclone has
            // exited and released it — but not if something still holds a file: measured,
            // a mount stays busy against `-u` after its daemon is dead, because what pins
            // it is the holder's descriptor.
            match self.release_point(&point, Release::Refuse).await {
                Ok(()) => {}
                Err(e) if !may_detach => return Err(Self::refused(&point, e)),
                Err(_) => {
                    self.release_point(&point, Release::Detach).await?;
                }
            }
            if self.await_gone(&point, Duration::from_secs(10)).await {
                // Reaching here means the stop timed out, which is what leaves the unit
                // `failed`. Without this the mount the user just successfully unmounted
                // reports as failed on the next poll.
                return self.units.reset_failed(&unit).await;
            }
            Err(SupervisorError::Busy {
                detail: format!(
                    "{} is still mounted after everything this can do to release it. \
                     Something is holding it that a lazy unmount did not clear either, so \
                     there is likely a second filesystem stacked under the same path.",
                    point.display()
                ),
            })
        })
    }

    fn state<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
        Box::pin(async move {
            match self.mount_config(name) {
                Ok(m) => self.resolve(m).await,
                // `reconcile` reports orphans, so whatever is listing them has to be
                // able to poll them too.
                Err(unknown) => {
                    if self.orphans().await?.iter().any(|o| o.name == name) {
                        Ok(MountState::Orphaned)
                    } else {
                        Err(unknown)
                    }
                }
            }
        })
    }

    fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
        Box::pin(async move {
            let live = self.live_mounts();
            let mut out = Vec::new();

            for m in &self.config.mounts {
                out.push(DiscoveredMount::new(&m.name, self.resolve(m).await?));
            }

            // Canonicalised, exactly as `resolve` compares them: matching the raw path
            // would list a mount under a symlinked directory twice.
            let mut claimed: Vec<PathBuf> = Vec::with_capacity(self.config.mounts.len());
            for m in &self.config.mounts {
                claimed.push(Self::resolved_point(m).await);
            }
            // Ours, under a name the config dropped. Reported before the sweep below and
            // counted as claimed, or the same mount point would appear twice: once as
            // somebody else's, once as ours.
            for o in self.orphans().await? {
                claimed.push(o.point);
                out.push(DiscoveredMount::new(o.name, MountState::Orphaned));
            }
            for e in live.iter().filter(|e| e.is_rclone()) {
                if claimed.contains(&e.mount_point) {
                    continue;
                }
                out.push(DiscoveredMount::new(foreign_name(e), MountState::Foreign));
            }
            Ok(out)
        })
    }
}

/// Where a mount's rc socket lives.
///
/// This service's runtime directory joined with the shared socket file name. The client
/// resolves the same name against `XDG_RUNTIME_DIR`; `RcClient::socket_file_name` is the
/// single definition both use, and explains why the name is escaped rather than folded.
fn rc_socket_path(runtime_dir: &Path, name: &str) -> PathBuf {
    runtime_dir.join(rvt_core::RcClient::socket_file_name(name))
}

/// Clear what a hard-killed rclone leaves behind, so a start can succeed.
///
/// Runs from `ExecStartPre`, so it covers systemd's automatic restarts. Talks to nothing
/// — systemd is waiting on it — and releases a mount point only when that point is stale,
/// since a live one there is somebody else's. See DESIGN.md, "Delegated restart needs a
/// pre-start hook to work at all".
pub async fn prepare_for_start(
    config: &Config,
    runtime_dir: &Path,
    mountinfo_path: &Path,
    name: &str,
) -> Result<(), SupervisorError> {
    let m = config
        .mounts
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| SupervisorError::UnknownMount(name.to_string()))?;

    let socket = rc_socket_path(runtime_dir, name);
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }

    // Bounded, because systemd's start job is blocked on this: a `stat` through a wedged
    // FUSE mount never returns, and a process in uninterruptible sleep cannot be killed at
    // `TimeoutStartSec` either. A probe that does not answer is treated as *not* stale, so
    // the fallback is to leave the mount alone rather than to release something live.
    let raw = m.mount_point.clone();
    let fallback = raw.clone();
    let point = SystemdSupervisor::<crate::systemd::dbus::SystemdUnits>::off_thread(move || {
        std::fs::canonicalize(&raw).ok()
    })
    .await
    .flatten()
    .unwrap_or(fallback);

    let live = match mountinfo::read_from(mountinfo_path) {
        Ok(entries) => entries,
        Err(e) => {
            // Declining to clear a stale point means the start that follows will fail on
            // an occupied path, so the reason has to be recoverable from the journal.
            tracing::warn!(path = %mountinfo_path.display(), error = %e,
                "could not read mountinfo; not clearing any stale mount point");
            Vec::new()
        }
    };
    let probe = point.clone();
    let stale = SystemdSupervisor::<crate::systemd::dbus::SystemdUnits>::off_thread(
        move || matches!(std::fs::metadata(&probe), Err(e) if e.raw_os_error() == Some(ENOTCONN)),
    )
    .await
    .unwrap_or(false);
    if mountinfo::is_mounted_at(&live, &point) && stale {
        SystemdSupervisor::<crate::systemd::dbus::SystemdUnits>::fusermount(
            &point,
            Release::Refuse,
        )
        .await?;
    }
    Ok(())
}

/// A name for a mount we did not configure.
///
/// The full mount point, not its last component: basenames collide, names are how clients
/// address mounts, and a configured name cannot contain `/`.
fn foreign_name(e: &MountEntry) -> String {
    e.mount_point.to_string_lossy().into_owned()
}

/// The mount name a unit of ours was started for.
///
/// `Config::validate` restricts names to the characters a unit name accepts, so for
/// anything that reached systemd this inverts [`Mount::unit_name`] exactly. `None` for a
/// name that does not fit the pattern, which is then not one of ours to act on.
fn orphan_name(unit: &str) -> Option<String> {
    let name = unit.strip_prefix(UNIT_PREFIX)?.strip_suffix(".service")?;
    // `Config::validate` rejects the same three, since a name is used to build paths.
    // Nothing this service starts can carry one, so a unit that does was hand-made.
    (!matches!(name, "" | "." | "..")).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemd::{LoadedUnit, Serving};
    use rvt_core::config::CacheMode;
    use rvt_testutil::Scratch;
    use std::sync::Mutex;

    /// Records what it was asked to do and reports whatever status the test sets.
    #[derive(Default)]
    struct FakeUnits {
        status: Mutex<UnitStatus>,
        /// Units systemd would report as loaded, each answering for its own name. What
        /// `status` reports covers every other name, which keeps the tests that only
        /// ever have one unit as they were.
        loaded: Mutex<Vec<LoadedUnit>>,
        started: Mutex<Vec<UnitSpec>>,
        stopped: Mutex<Vec<String>>,
        reset: Mutex<Vec<String>>,
        /// Rewritten with no mounts when a unit is stopped, so `await_gone` sees what it
        /// would see from a real rclone releasing its mount point.
        clears_on_stop: Mutex<Option<PathBuf>>,
        /// Shared with the supervisor, so a test can see whether the kernel probe or the
        /// unit stop came first.
        events: Arc<Mutex<Vec<String>>>,
        /// Points something holds a file open under. Deliberately *not* cleared on stop:
        /// what pins a mount is the holder's own descriptor, so killing rclone leaves it
        /// exactly as busy. Measured — see [`Release`].
        busy: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
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
                self.events.lock().unwrap().push(format!("stop {unit}"));
                if let Some(p) = self.clears_on_stop.lock().unwrap().as_ref() {
                    std::fs::write(p, mountinfo_with(&[])).unwrap();
                }
                let mut loaded = self.loaded.lock().unwrap();
                match loaded.iter_mut().find(|u| u.name == unit) {
                    Some(u) => u.status = UnitStatus::Inactive,
                    None => *self.status.lock().unwrap() = UnitStatus::Inactive,
                }
                Ok(())
            })
        }
        fn status<'a>(
            &'a self,
            unit: &'a str,
        ) -> BoxFuture<'a, Result<UnitStatus, SupervisorError>> {
            Box::pin(async move {
                let loaded = self.loaded.lock().unwrap();
                Ok(match loaded.iter().find(|u| u.name == unit) {
                    Some(u) => u.status,
                    None => *self.status.lock().unwrap(),
                })
            })
        }
        fn list_units<'a>(
            &'a self,
            prefix: &'a str,
        ) -> BoxFuture<'a, Result<Vec<LoadedUnit>, SupervisorError>> {
            Box::pin(async move {
                let loaded = self.loaded.lock().unwrap();
                Ok(loaded
                    .iter()
                    .filter(|u| u.name.starts_with(prefix))
                    .cloned()
                    .collect())
            })
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
    fn mount_point(scratch: &Scratch) -> PathBuf {
        scratch.dir("mnt")
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

    fn fixture(scratch: &Scratch, name: &str, contents: &str) -> PathBuf {
        scratch.write(format!("{name}.mountinfo"), contents)
    }

    /// Serves `backup:pictures`, which is what [`mountinfo_with`] writes as the source,
    /// so the check that a mount point is serving what its config says survives.
    fn a_mount(name: &str, mount_point: PathBuf) -> Mount {
        Mount {
            name: name.into(),
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
        }
    }

    fn config_with_backup(mount_point: PathBuf) -> Arc<Config> {
        let mut c = Config::default();
        c.mounts.push(a_mount("backup", mount_point));
        Arc::new(c)
    }

    /// `mounted` controls whether the configured mount appears in mountinfo; `extra` adds
    /// live rclone mounts at paths the config does not know about.
    fn supervisor(
        tag: &str,
        mounted: bool,
        extra: &[&str],
        status: UnitStatus,
    ) -> (Scratch, SystemdSupervisor<FakeUnits>) {
        let scratch = Scratch::new(tag);
        let mp = mount_point(&scratch);
        let mp_str = mp.to_string_lossy().into_owned();
        let mut live: Vec<&str> = Vec::new();
        if mounted {
            live.push(&mp_str);
        }
        live.extend_from_slice(extra);

        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let busy: Arc<Mutex<std::collections::HashSet<PathBuf>>> = Arc::default();
        let units = FakeUnits {
            events: events.clone(),
            busy: busy.clone(),
            ..Default::default()
        };
        *units.status.lock().unwrap() = status;
        let sup = SystemdSupervisor::new(
            config_with_backup(mp.clone()),
            PathBuf::from("/usr/bin/rclone"),
            units,
            // Deliberately not created: the runtime directory is the supervisor's to make.
            scratch.join("run"),
            PathBuf::from("/nonexistent/config.toml"),
        )
        .with_test_overrides(
            fixture(&scratch, tag, &mountinfo_with(&live)),
            Duration::from_millis(300),
        )
        .with_events(events)
        .with_busy(busy);
        (scratch, sup)
    }

    #[tokio::test]
    async fn a_mount_we_started_is_ours() {
        let (_sc, s) = supervisor("ours", true, &[], UnitStatus::Active);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Mounted);
    }

    #[tokio::test]
    async fn a_mount_with_no_unit_of_ours_is_foreign() {
        // Same kernel evidence, no unit. This is the case that must not be reported as
        // ours, because acting on it would be acting on somebody else's mount.
        let (_sc, s) = supervisor("foreign", true, &[], UnitStatus::Inactive);
        let st = s.state("backup").await.unwrap();
        assert_eq!(st, MountState::Foreign);
        assert!(st.is_live() && !st.is_managed());
    }

    #[tokio::test]
    async fn nothing_mounted_is_unmounted_not_failed() {
        let (_sc, s) = supervisor("down", false, &[], UnitStatus::Inactive);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Unmounted);
    }

    #[tokio::test]
    async fn a_failed_unit_carries_rclones_own_words() {
        let (_sc, s) = supervisor("failed", false, &[], UnitStatus::Failed);
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
        let (_sc, s) = supervisor("already", true, &[], UnitStatus::Active);
        s.mount("backup").await.unwrap();
        assert!(
            s.units.started.lock().unwrap().is_empty(),
            "an already-serving mount must not be started again"
        );
    }

    #[tokio::test]
    async fn a_mount_that_never_appears_reports_why() {
        let (_sc, s) = supervisor("never", false, &[], UnitStatus::Inactive);
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
        let (_sc, s) = supervisor("umask", false, &[], UnitStatus::Inactive);
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
        let (_sc, s) = supervisor("gone", false, &[], UnitStatus::Inactive);
        s.unmount("backup", false).await.unwrap();
        assert!(s.units.stopped.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_foreign_mount_is_not_unmounted_without_force() {
        let (_sc, s) = supervisor("noforce", true, &[], UnitStatus::Inactive);
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

    /// Something holds a file open under `point`, so the kernel refuses to release it.
    ///
    /// Deliberately *not* wired with [`releasing`], and the set is not cleared on stop —
    /// see [`Release`] for why. So the second release is refused too, and only `Detach`
    /// takes it.
    fn hold_open(s: &SystemdSupervisor<FakeUnits>, point: &Path) {
        s.units.busy.lock().unwrap().insert(point.to_path_buf());
    }

    #[tokio::test]
    async fn unmounting_a_mount_we_started_stops_its_unit() {
        let (_sc, s) = supervisor("stops", true, &[], UnitStatus::Active);
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
        let (_sc, s) = supervisor("wedged", false, &[], UnitStatus::Activating);
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
        let (_sc, s) = supervisor("retryable", false, &[], UnitStatus::Failed);
        s.unmount("backup", false)
            .await
            .expect("a failed unit is ours");
        assert!(!s.units.stopped.lock().unwrap().is_empty());
        assert!(!s.units.reset.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_mount_being_torn_down_is_not_reported_as_somebody_elses() {
        // The window between `StopUnit` returning and systemd finishing the job.
        let (_sc, s) = supervisor("tearing", true, &[], UnitStatus::Deactivating);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Unmounting);
    }

    #[tokio::test]
    async fn our_own_crashed_mount_stays_ours() {
        let (_sc, s) = supervisor("crashed", true, &[], UnitStatus::Failed);
        match s.state("backup").await.unwrap() {
            MountState::Failed { .. } => {}
            other => panic!("a failed unit of ours must not read as Foreign, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_started_unit_with_nothing_mounted_yet_is_coming_up() {
        // `Type=exec` reports active the moment rclone is exec'd, seconds before the
        // mount point answers. Reporting that as Unmounted inverts the two states.
        let (_sc, s) = supervisor("comingup", false, &[], UnitStatus::Active);
        assert_eq!(s.state("backup").await.unwrap(), MountState::Mounting);
    }

    /// #73. A file still being written is invisible to every rc endpoint — rclone queues
    /// a file when it is *closed* — so the kernel's refusal is the only signal there is,
    /// and it only arrives if nothing has signalled rclone first. `StopUnit` is a SIGTERM
    /// under `KillMode=mixed`, and rclone exits on it even when its own unmount returned
    /// EBUSY, severing the writer and later publishing the truncated cache item.
    #[tokio::test]
    async fn a_mount_with_a_file_open_is_refused_before_anything_signals_rclone() {
        let (sc, s) = supervisor("openwrite", true, &[], UnitStatus::Active);
        let point = mount_point(&sc);
        hold_open(&s, &point);

        match s.unmount("backup", false).await {
            Err(SupervisorError::Busy { detail }) => {
                assert!(detail.contains(&point.display().to_string()), "{detail}");
                assert!(
                    detail.contains("still using the mount"),
                    "unhelpful: {detail}"
                );
                assert!(
                    detail.starts_with(&point.display().to_string()),
                    "rendered verbatim, so it has to open with the path: {detail}"
                );
                assert!(
                    !detail.contains("close that"),
                    "a cwd inside the mount is enough to be refused, so do not say \
                     'close the file': {detail}"
                );
            }
            other => panic!("a busy mount must be refused, got {other:?}"),
        }

        assert!(
            s.units.stopped.lock().unwrap().is_empty(),
            "the unit must not be stopped: the SIGTERM is what truncates the file"
        );
        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            &[format!("Refuse {}", point.display())],
            "the kernel must be asked, and nothing else may happen after it says no"
        );
        assert!(
            mountinfo::is_mounted_at(&s.live_mounts(), &point),
            "a refusal must leave the mount exactly as it was"
        );
    }

    /// `fusermount` reports every non-zero exit as `Busy`, so a refusal cannot be told
    /// from "not a mount point" — the advice has to hedge. A failure to *run* it is a
    /// different thing and must not collect advice about closing files.
    #[test]
    fn a_refusal_hedges_its_cause_and_a_spawn_failure_is_passed_through() {
        let point = Path::new("/mnt/backup");

        let hedged = SystemdSupervisor::<FakeUnits>::refused(
            point,
            SupervisorError::Busy {
                detail: "/mnt/backup could not be unmounted: fusermount3: failed to \
                         unmount /mnt/backup: Device or resource busy"
                    .into(),
            },
        );
        let msg = hedged.to_string();
        assert!(
            msg.starts_with("/mnt/backup could not be unmounted"),
            "{msg}"
        );
        assert!(msg.contains("Usually"), "must hedge, not assert: {msg}");
        // Rendered verbatim, so it reads as one sentence rather than a sentence quoted
        // inside another. Both of these are what a wrapper around it would add.
        assert!(!msg.contains('"'), "no nested quoting: {msg}");
        assert!(msg.ends_with('.'), "must end as a sentence does: {msg}");

        let spawn = SystemdSupervisor::<FakeUnits>::refused(
            point,
            SupervisorError::Supervision {
                context: "running fusermount for /mnt/backup".into(),
                source: None,
            },
        );
        assert!(
            matches!(spawn, SupervisorError::Supervision { .. }),
            "a missing helper is not a busy mount, got {spawn:?}"
        );
        assert!(
            !spawn.to_string().contains("file open"),
            "no advice about closing files when nothing was asked: {spawn}"
        );
    }

    /// The other half of the ordering: on a mount nothing is holding, the kernel is still
    /// asked first, and only then is the unit stopped.
    #[tokio::test]
    async fn an_idle_mount_is_released_before_its_unit_is_stopped() {
        let (sc, s) = supervisor("idleorder", true, &[], UnitStatus::Active);
        releasing(&s);
        s.unmount("backup", false).await.expect("nothing holds it");
        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            &[
                format!("Refuse {}", mount_point(&sc).display()),
                "stop rvt-mount-backup.service".to_string(),
            ],
            "the release must come first, or the refusal can never happen"
        );
    }

    /// `force` is #18's "unmount anyway", already confirmed with the user, so a busy mount
    /// still comes down — and only `-z` brings it. See [`Release`].
    #[tokio::test]
    async fn force_detaches_a_busy_mount_because_plain_unmount_stays_refused() {
        let (sc, s) = supervisor("forcebusy", true, &[], UnitStatus::Active);
        let point = mount_point(&sc);
        hold_open(&s, &point);

        s.unmount("backup", true)
            .await
            .expect("force must actually bring the mount down");

        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            &[
                // Refused, with nothing signalled yet.
                format!("Refuse {}", point.display()),
                // Overridden: rclone is signalled and the writer is severed.
                "stop rvt-mount-backup.service".to_string(),
                // Still refused — the holder, not rclone, is what pins it.
                format!("Refuse {}", point.display()),
                // Only this takes it, and only because there is no longer a live writer
                // for it to detach from.
                format!("Detach {}", point.display()),
            ],
            "force must escalate all the way, and only at the end"
        );
        assert!(
            !mountinfo::is_mounted_at(&s.live_mounts(), &point),
            "the point must actually be gone once force has run"
        );
        assert_eq!(
            s.units.reset.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()],
            "the unit name must be reusable afterwards"
        );
    }

    /// The counterpart: a `Detach` reaching a mount the user did not force is the failure
    /// this whole change exists to prevent. It covers the probe, which is where a
    /// non-forced unmount of a held mount ends. The escalation's own `force` check is
    /// reachable only when a release succeeds and the path is *still* mounted — stacked
    /// mounts — so nothing here exercises that one.
    #[tokio::test]
    async fn nothing_is_ever_detached_without_force() {
        let (sc, s) = supervisor("nodetach", true, &[], UnitStatus::Active);
        hold_open(&s, &mount_point(&sc));
        let _ = s.unmount("backup", false).await;
        assert!(
            !s.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("Detach")),
            "a non-forced unmount must never detach: {:?}",
            s.events.lock().unwrap()
        );
    }

    /// `force` on a *foreign* mount that is held must refuse rather than detach. Nothing
    /// was signalled — that rclone is not ours to stop — so it is alive and serving, and
    /// `-z` would strand it holding a mount nothing can see, with whatever it is buffering
    /// in a cache directory the user can no longer reach. A cwd inside the mount is enough
    /// to reach this: measured, that alone makes `fusermount3 -u` return EBUSY.
    #[tokio::test]
    async fn force_never_detaches_a_mount_whose_unit_was_not_stopped() {
        let (sc, s) = supervisor("foreignbusy", true, &[], UnitStatus::Inactive);
        let point = mount_point(&sc);
        hold_open(&s, &point);

        let e = s
            .unmount("backup", true)
            .await
            .expect_err("a held foreign mount cannot be taken without stranding its rclone");
        assert!(matches!(e, SupervisorError::Busy { .. }), "{e:?}");

        assert!(
            s.units.stopped.lock().unwrap().is_empty(),
            "a foreign unit is not ours to stop"
        );
        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            // Once. Knowing up front that detaching is not on the table, there is nothing
            // a second identical call could learn.
            &[format!("Refuse {}", point.display())],
            "it must try, and stop at trying — no Detach without a stop of our own"
        );
    }

    #[tokio::test]
    async fn force_actually_attempts_to_release_a_foreign_mount() {
        // Without force this is refused. With it the release must genuinely be attempted —
        // "we did not try" and "we tried" are different answers, and the event log says
        // which happened rather than leaving it to be inferred from an error message.
        let (sc, s) = supervisor("force", true, &[], UnitStatus::Inactive);
        s.unmount("backup", true)
            .await
            .expect("force must release a foreign mount");
        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            &[format!("Refuse {}", mount_point(&sc).display())],
            "force must reach the kernel, and must not stop a unit that is not ours"
        );
    }

    #[test]
    fn a_socket_path_never_escapes_the_runtime_directory() {
        // A foreign mount is named by its absolute mount point, and `Path::join` given an
        // absolute path throws the base away. Without this, reconcile handing a foreign
        // name here would have the service stat — and, finding any user-owned socket in a
        // private directory, connect to and POST rc/list at — an arbitrary filesystem
        // location that has nothing to do with the mount.
        let (_sc, s) = supervisor("escape", false, &[], UnitStatus::Active);
        for name in ["/srv/media", "/", "../../etc/passwd", "a/b"] {
            let p = s.socket_path(name);
            assert!(
                p.starts_with(&s.runtime_dir),
                "{name:?} produced {p:?}, outside {:?}",
                s.runtime_dir
            );
        }
        // The ordinary case is untouched.
        assert_eq!(
            s.socket_path("backup"),
            s.runtime_dir.join("backup.sock"),
            "a configured name must keep the path it has always had"
        );
    }

    #[tokio::test]
    async fn a_stale_socket_is_cleared_when_the_unit_died() {
        // The only state in which a stale socket can exist. rclone binds with a bare
        // listen and dies on EADDRINUSE, so leaving it makes the mount unstartable.
        let (_sc, s) = supervisor("staleock", false, &[], UnitStatus::Failed);
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
        let (_sc, s) = supervisor("double", false, &[], UnitStatus::Activating);
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
        let scratch = Scratch::new("symlink");
        let real = scratch.dir("real");
        let link = scratch.join("link");
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
            scratch.join("run"),
            PathBuf::from("/nonexistent/config.toml"),
        )
        .with_test_overrides(
            fixture(
                &scratch,
                "symlink",
                &mountinfo_with(&[&real.to_string_lossy()]),
            ),
            Duration::from_millis(300),
        );

        assert_eq!(
            s.state("backup").await.unwrap(),
            MountState::Mounted,
            "the mount is up at the resolved path and must be recognised"
        );
    }

    #[tokio::test]
    async fn a_mount_point_that_no_longer_responds_is_reported_as_failed() {
        // The kernel still lists it, so a path check alone says "mounted". Every
        // operation on it returns ENOTCONN.
        let (sc, s) = supervisor("stale", true, &[], UnitStatus::Inactive);
        let point = mount_point(&sc);
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
        let (sc, s) = supervisor("staleclear", true, &[], UnitStatus::Inactive);
        let point = mount_point(&sc);
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
        // The remount gesture. `StopUnit` only enqueues a job, so the name can still be
        // taken when the next mount starts, and starting into that returns systemd's raw
        // "unit already exists".
        let (_sc, s) = supervisor("remount", false, &[], UnitStatus::Deactivating);
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
        let (sc, s) = supervisor("earlyexit", false, &[], UnitStatus::Failed);
        let s = s.with_test_overrides(
            fixture(&sc, "earlyexit2", &mountinfo_with(&[])),
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
        let scratch = Scratch::new("mismatch");
        let mp = mount_point(&scratch);
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
            scratch.join("run"),
            PathBuf::from("/nonexistent/config.toml"),
        )
        .with_test_overrides(
            // mountinfo says backup:pictures is what is actually there.
            fixture(
                &scratch,
                "mismatch",
                &mountinfo_with(&[&mp.to_string_lossy()]),
            ),
            Duration::from_millis(300),
        );

        match s.unmount("backup", false).await {
            Err(SupervisorError::NotManaged(why)) => {
                assert!(why.contains("is serving"), "{why}");
            }
            other => panic!("expected a refusal naming the mismatch, got {other:?}"),
        }
        // The unit is ours and stopping it would not touch the foreign filesystem, so this
        // could go either way. It refuses without stopping, deliberately: a refusal that
        // has already half-acted is the shape the #73 ordering exists to get rid of, and
        // the user still has `force`. Asserted so the answer cannot change by accident.
        assert!(
            s.units.stopped.lock().unwrap().is_empty(),
            "a refusal must leave the system as it found it"
        );
    }

    #[tokio::test]
    async fn mounting_over_somebody_elses_mount_is_refused() {
        // The property this PR leads with. Silently taking over another process's mount
        // is what makes a tray applet get uninstalled.
        let (_sc, s) = supervisor("takeover", true, &[], UnitStatus::Inactive);
        match s.mount("backup").await {
            Err(SupervisorError::NotManaged(why)) => {
                assert!(why.contains("did not start"), "{why}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            s.units.started.lock().unwrap().is_empty(),
            "no unit may be started onto an occupied mount point"
        );
    }

    #[tokio::test]
    async fn mounting_does_not_report_success_onto_a_mount_being_torn_down() {
        // rclone's own unmount can fail with EBUSY and hold the point for the whole stop
        // timeout. Reporting success there leaves the user with no mount and no unit once
        // systemd finishes, and no indication anything went wrong.
        let (_sc, s) = supervisor("teardown", true, &[], UnitStatus::Deactivating);
        let e = s
            .mount("backup")
            .await
            .expect_err("the fake never leaves Deactivating");
        match e {
            SupervisorError::Supervision { context, .. } => assert!(
                context.contains("still shutting down"),
                "it must wait for the teardown, not claim success: {context}"
            ),
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn force_overrides_a_source_mismatch_as_well_as_the_refusal() {
        // "Unmount anyway" on a foreign mount of a *different* remote is exactly the case
        // #18 is about. Refusing it after the user confirmed reads as a contradiction.
        let scratch = Scratch::new("forcemismatch");
        let mp = mount_point(&scratch);
        let mut c = Config::default();
        c.mounts.push(Mount {
            remote: "somethingelse".into(),
            ..config_with_backup(mp.clone()).mounts[0].clone()
        });
        // One event log and one busy set, shared with the fake, or the assertion below
        // watches a channel nothing writes to.
        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let busy: Arc<Mutex<std::collections::HashSet<PathBuf>>> = Arc::default();
        let units = FakeUnits {
            events: events.clone(),
            busy: busy.clone(),
            ..Default::default()
        };
        *units.status.lock().unwrap() = UnitStatus::Inactive;
        let s = SystemdSupervisor::new(
            Arc::new(c),
            PathBuf::from("/usr/bin/rclone"),
            units,
            scratch.join("run"),
            PathBuf::from("/nonexistent/config.toml"),
        )
        .with_test_overrides(
            fixture(
                &scratch,
                "forcemismatch",
                &mountinfo_with(&[&mp.to_string_lossy()]),
            ),
            Duration::from_millis(300),
        )
        .with_events(events)
        .with_busy(busy);

        s.unmount("backup", true)
            .await
            .expect("force must override the source mismatch, not just the ownership one");
        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            &[format!("Refuse {}", mp.display())],
            "the release must have been attempted despite the mismatch"
        );
    }

    #[tokio::test]
    async fn the_started_unit_carries_a_recovery_hook_the_binary_understands() {
        // Without it a killed rclone can never be restarted by systemd: it will not
        // rebind over its own leftover socket. The argv is checked against the binary's
        // own parser, because a hook systemd runs and the binary rejects is a no-op that
        // nothing else would notice.
        let (_sc, s) = supervisor("hook", false, &[], UnitStatus::Inactive);
        let _ = s.mount("backup").await;

        let started = s.units.started.lock().unwrap();
        let spec = started.first().expect("a unit should have been started");
        let (bin, args) = spec
            .pre_start
            .as_ref()
            .expect("every mount unit needs the recovery hook");
        assert!(bin.is_file(), "the hook must point at a binary that exists");
        assert_eq!(
            args,
            &[
                "--config",
                "/nonexistent/config.toml",
                "prepare-mount",
                "--name",
                "backup"
            ],
            "the hook must be told which config the service actually loaded, and \
             `--config` is not a global argument so it has to precede the subcommand"
        );

        // The binary's parser must accept exactly this.
        let mut argv = vec![bin.to_string_lossy().into_owned()];
        argv.extend(args.iter().cloned());
        <crate::Args as clap::Parser>::try_parse_from(&argv)
            .expect("the binary must accept the argv its own units record");
    }

    #[tokio::test]
    async fn the_recovery_hook_clears_a_stale_socket() {
        let scratch = Scratch::new("hookstale");
        let dir = scratch.dir("run");
        let mp = mount_point(&scratch);
        let sock = scratch.write("run/backup.sock", b"");

        let cfg = config_with_backup(mp.clone());
        prepare_for_start(
            &cfg,
            &dir,
            &fixture(&scratch, "hooksock", &mountinfo_with(&[])),
            "backup",
        )
        .await
        .unwrap();
        assert!(
            !sock.exists(),
            "rclone dies on EADDRINUSE rather than replacing its own leftover socket"
        );
    }

    #[tokio::test]
    async fn the_recovery_hook_leaves_a_live_mount_alone() {
        // The guard that keeps this from being a take-over. The hook runs without the
        // ownership checks `mount()` applies, so it must only ever clear a mount point
        // that is dead — a live one at the configured path is somebody else's.
        let scratch = Scratch::new("hooklive");
        let dir = scratch.dir("run");
        let mp = mount_point(&scratch);

        let cfg = config_with_backup(mp.clone());
        let mi = fixture(
            &scratch,
            "hooklive",
            &mountinfo_with(&[&mp.to_string_lossy()]),
        );
        prepare_for_start(&cfg, &dir, &mi, "backup").await.unwrap();

        // `mp` is a real directory, so it is not stale; the hook must not have touched
        // it. If it had run fusermount the call would have errored rather than returned.
        assert!(
            mp.is_dir(),
            "a live mount point must be left exactly as it was"
        );
    }

    #[tokio::test]
    async fn the_recovery_hook_names_a_mount_it_does_not_know() {
        let scratch = Scratch::new("hookunk");
        let dir = scratch.dir("run");
        let cfg = config_with_backup(mount_point(&scratch));
        match prepare_for_start(
            &cfg,
            &dir,
            &fixture(&scratch, "hookunk", &mountinfo_with(&[])),
            "nope",
        )
        .await
        {
            Err(SupervisorError::UnknownMount(n)) => assert_eq!(n, "nope"),
            other => panic!("expected UnknownMount, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reconcile_reports_configured_and_foreign_mounts() {
        let other = "/tmp/somebody-elses";
        let (_sc, s) = supervisor("reconcile", true, &[other], UnitStatus::Active);
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
        let (_sc, s) = supervisor("listdown", false, &[], UnitStatus::Inactive);
        let found = s.reconcile().await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, MountState::Unmounted);
    }

    #[tokio::test]
    async fn an_unknown_mount_is_named_in_the_error() {
        let (_sc, s) = supervisor("unknown", false, &[], UnitStatus::Inactive);
        match s.state("nope").await {
            Err(SupervisorError::UnknownMount(n)) => assert_eq!(n, "nope"),
            other => panic!("expected UnknownMount, got {other:?}"),
        }
    }

    /// `backup` has been renamed to `backups` in the config and the service restarted.
    /// The old unit is still up and still serving the same point; nothing answers to the
    /// new name. #71.
    fn renamed(tag: &str) -> (Scratch, SystemdSupervisor<FakeUnits>) {
        let scratch = Scratch::new(tag);
        let mp = mount_point(&scratch);
        let mp_str = mp.to_string_lossy().into_owned();

        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let busy: Arc<Mutex<std::collections::HashSet<PathBuf>>> = Arc::default();
        let units = FakeUnits {
            events: events.clone(),
            busy: busy.clone(),
            ..Default::default()
        };
        // Nothing under the new name — every unit but the one below reads Inactive.
        *units.status.lock().unwrap() = UnitStatus::Inactive;
        units.loaded.lock().unwrap().push(LoadedUnit {
            name: "rvt-mount-backup.service".into(),
            status: UnitStatus::Active,
            serving: Some(Serving {
                fs_spec: "backup:pictures".into(),
                mount_point: mp.clone(),
            }),
        });

        let mut config = Config::default();
        config.mounts.push(a_mount("backups", mp.clone()));

        let sup = SystemdSupervisor::new(
            Arc::new(config),
            PathBuf::from("/usr/bin/rclone"),
            units,
            scratch.join("run"),
            PathBuf::from("/nonexistent/config.toml"),
        )
        .with_test_overrides(
            fixture(&scratch, tag, &mountinfo_with(&[&mp_str])),
            Duration::from_millis(300),
        )
        .with_events(events)
        .with_busy(busy);
        (scratch, sup)
    }

    #[tokio::test]
    async fn a_renamed_mount_does_not_read_as_somebody_elses() {
        let (_sc, s) = renamed("rename-state");
        match s.state("backups").await.unwrap() {
            MountState::Failed { reason } => assert!(
                reason.contains("rvt-mount-backup.service"),
                "the reason has to name the unit in the way: {reason}"
            ),
            other => panic!("a mount of ours must not read as Foreign, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_unit_a_rename_left_behind_is_reported_as_ours() {
        let (_sc, s) = renamed("rename-reconcile");
        let found = s.reconcile().await.unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        let orphan = found
            .iter()
            .find(|m| m.name == "backup")
            .unwrap_or_else(|| panic!("the old unit must be listed: {found:?}"));
        assert_eq!(orphan.state, MountState::Orphaned);
        assert!(
            !found.iter().any(|m| m.state == MountState::Foreign),
            "the point is held by a unit of ours: listing it as foreign too would put \
             the same mount in the list twice, under two owners: {found:?}"
        );
    }

    #[tokio::test]
    async fn an_orphan_can_be_polled_under_the_name_its_unit_runs_as() {
        let (_sc, s) = renamed("rename-poll");
        assert_eq!(s.state("backup").await.unwrap(), MountState::Orphaned);
    }

    #[tokio::test]
    async fn unmounting_an_orphan_stops_its_unit() {
        let (_sc, s) = renamed("rename-unmount-orphan");
        releasing(&s);
        s.unmount("backup", false)
            .await
            .expect("an orphan is ours to stop");
        assert_eq!(
            s.units.stopped.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()]
        );
    }

    /// The damage in #71: `backups` read as foreign, so unmounting the user's own mount
    /// refused unless they forced it, and the force released the mount point without ever
    /// stopping the unit that owned it.
    #[tokio::test]
    async fn unmounting_under_the_new_name_stops_the_unit_holding_the_point() {
        let (sc, s) = renamed("rename-unmount-new");
        releasing(&s);
        s.unmount("backups", false)
            .await
            .expect("the point is held by a unit of ours, so no force is needed");
        assert_eq!(
            s.units.stopped.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()],
            "the unit that owns the point is what has to be stopped"
        );
        assert_eq!(
            s.events.lock().unwrap().as_slice(),
            &[
                format!("Refuse {}", mount_point(&sc).display()),
                "stop rvt-mount-backup.service".to_string(),
            ],
            "an orphan comes down the same way any mount of ours does: kernel first"
        );
    }

    /// The costly half of reading a mount of ours as foreign. `Release::Detach` is gated
    /// on ownership, so a *busy* orphan could not be brought down at all: the kernel
    /// refuses `-u` while a holder has it, and `force` had nothing left to escalate to.
    #[tokio::test]
    async fn forcing_a_busy_orphan_reaches_the_same_escalation_as_any_mount_of_ours() {
        let (sc, s) = renamed("rename-force-busy");
        let point = mount_point(&sc);
        hold_open(&s, &point);

        s.unmount("backups", true)
            .await
            .expect("force must bring down a busy mount of ours, orphan or not");

        let events = s.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                format!("Refuse {}", point.display()),
                "stop rvt-mount-backup.service".to_string(),
                format!("Refuse {}", point.display()),
                format!("Detach {}", point.display()),
            ],
            "the unit must be stopped before the detach, exactly as for a configured mount"
        );
    }

    /// The stale branch of `mount` releases a mount point without asking who owns it, and
    /// an orphan whose rclone was killed is exactly a stale point with a unit behind it.
    #[tokio::test]
    async fn mounting_over_a_stale_orphan_does_not_release_its_point() {
        let (sc, s) = renamed("rename-stale");
        let point = mount_point(&sc);
        let s = s.with_stale(&[&point]);

        match s.mount("backups").await {
            Err(SupervisorError::NotManaged(msg)) => assert!(
                msg.contains("rvt-mount-backup.service"),
                "the refusal has to name the unit holding it: {msg}"
            ),
            other => panic!("expected a refusal naming the orphan, got {other:?}"),
        }
        let events = s.events.lock().unwrap().clone();
        assert!(
            events.is_empty(),
            "a point one of our units owns must not be released out from under it: {events:?}"
        );
        assert!(s.units.started.lock().unwrap().is_empty());
    }

    /// After a rename two names reach one unit, so the caller's own lock no longer guards
    /// what is being stopped.
    #[tokio::test]
    async fn a_redirected_unmount_waits_for_the_lock_on_the_unit_it_stops() {
        let (_sc, s) = renamed("rename-lock");
        releasing(&s);
        // Held by whoever is already unmounting the orphan under its own name.
        let held = s.lock_for("backup").await;
        let _guard = held.lock().await;

        let blocked =
            tokio::time::timeout(Duration::from_millis(300), s.unmount("backups", false)).await;
        assert!(
            blocked.is_err(),
            "the redirect ran anyway, so both callers can drive the stop at once"
        );
        let stopped = s.units.stopped.lock().unwrap().clone();
        assert!(
            stopped.is_empty(),
            "nothing may be stopped while blocked: {stopped:?}"
        );
    }

    #[tokio::test]
    async fn mounting_over_an_orphan_names_the_unit_in_the_way() {
        let (_sc, s) = renamed("rename-mount");
        match s.mount("backups").await {
            Err(SupervisorError::NotManaged(msg)) => assert!(
                msg.contains("rvt-mount-backup.service"),
                "the user cannot act on a refusal that does not say what is there: {msg}"
            ),
            other => panic!("expected a refusal naming the orphan, got {other:?}"),
        }
    }

    /// A unit that failed leaves its argv loaded for the rest of the login session, and
    /// after a rename that argv names the path the *new* unit now serves. Claiming it
    /// would put one mount point in the list twice and — because both carry the same
    /// `remote:path` after a rename, so the source check cannot separate them — make
    /// `unmount("backup")` tear down the live, healthy mount with no `force` asked for.
    #[tokio::test]
    async fn a_dead_unit_does_not_inherit_the_mount_that_replaced_it() {
        let (sc, s) = renamed("rename-phantom");
        let point = mount_point(&sc);
        // The rename has been through a full cycle: the new unit is up and serving, and
        // the old one is a failed leftover still naming the same path.
        {
            let mut loaded = s.units.loaded.lock().unwrap();
            loaded.clear();
            loaded.extend([
                LoadedUnit {
                    name: "rvt-mount-backup.service".into(),
                    status: UnitStatus::Failed,
                    serving: Some(Serving {
                        fs_spec: "backup:pictures".into(),
                        mount_point: point.clone(),
                    }),
                },
                LoadedUnit {
                    name: "rvt-mount-backups.service".into(),
                    status: UnitStatus::Active,
                    serving: Some(Serving {
                        fs_spec: "backup:pictures".into(),
                        mount_point: point.clone(),
                    }),
                },
            ]);
        }

        let found = s.reconcile().await.unwrap();
        assert_eq!(
            found.len(),
            1,
            "one mount point must not be listed twice, under two owners: {found:?}"
        );
        assert_eq!(found[0].name, "backups");
        assert_eq!(found[0].state, MountState::Mounted);

        // The idempotent path a service restart takes: already up, and ours.
        s.mount("backups")
            .await
            .expect("a mount already serving under its own unit is already mounted");

        match s.unmount("backup", false).await {
            Err(SupervisorError::UnknownMount(n)) => assert_eq!(n, "backup"),
            other => panic!("a dead unit must not be able to unmount a live mount: {other:?}"),
        }
        let events = s.events.lock().unwrap().clone();
        assert!(
            events.is_empty(),
            "the live mount must not have been touched: {events:?}"
        );
    }

    /// The gap systemd leaves between an rclone exiting and its restart is reported as
    /// `Activating`, so being "running" does not mean serving anything. If the new unit
    /// mounted the path during that gap, the old one is on its way out and holds nothing
    /// — and the two are identical by path and remote, so only the configured unit's own
    /// state separates them.
    #[tokio::test]
    async fn a_unit_awaiting_restart_does_not_claim_the_mount_that_replaced_it() {
        let (sc, s) = renamed("rename-restart-gap");
        let point = mount_point(&sc);
        {
            let mut loaded = s.units.loaded.lock().unwrap();
            loaded[0].status = UnitStatus::Activating;
            loaded.push(LoadedUnit {
                name: "rvt-mount-backups.service".into(),
                status: UnitStatus::Active,
                serving: Some(Serving {
                    fs_spec: "backup:pictures".into(),
                    mount_point: point.clone(),
                }),
            });
        }

        let found = s.reconcile().await.unwrap();
        assert_eq!(
            found.len(),
            1,
            "the mount belongs to the configured unit serving it: {found:?}"
        );
        assert_eq!(found[0].state, MountState::Mounted);

        match s.unmount("backup", false).await {
            Err(SupervisorError::UnknownMount(n)) => assert_eq!(n, "backup"),
            other => panic!("a unit awaiting restart must not unmount a live mount: {other:?}"),
        }
        let events = s.events.lock().unwrap().clone();
        assert!(events.is_empty(), "the live mount was touched: {events:?}");
    }

    /// `lock_for` inserts on demand and nothing evicts, so an unmount that resolves to
    /// nothing must not leave an entry behind — #40 puts this on D-Bus, where the name
    /// comes from a client.
    #[tokio::test]
    async fn an_unmount_of_a_name_that_resolves_to_nothing_leaves_no_lock() {
        let (_sc, s) = supervisor("lockleak", false, &[], UnitStatus::Inactive);
        for n in ["typo", "another", "third"] {
            match s.unmount(n, false).await {
                Err(SupervisorError::UnknownMount(_)) => {}
                other => panic!("expected UnknownMount for {n:?}, got {other:?}"),
            }
        }
        assert!(
            s.locks.lock().await.is_empty(),
            "one map entry per bad name a client sends is unbounded growth"
        );
    }

    /// rclone records the Fs it resolved, not the argument it was given: measured on
    /// v1.75.0, an `alias` remote mounted as `ali:` reports its backing path in
    /// mountinfo, and a trailing slash in `path` is dropped. Requiring the two to match
    /// left every such config with the bug this sweep exists to fix, and nothing to say
    /// so.
    #[tokio::test]
    async fn an_orphan_is_found_when_rclone_rewrote_the_source() {
        let (sc, s) = renamed("rename-alias");
        // What the unit was told to mount. The fixture's mountinfo says the point serves
        // `backup:pictures`, as rclone would report it after resolving an alias.
        s.units.loaded.lock().unwrap()[0].serving = Some(Serving {
            fs_spec: "ali:".into(),
            mount_point: mount_point(&sc),
        });

        let found = s.reconcile().await.unwrap();
        assert!(
            found
                .iter()
                .any(|m| m.name == "backup" && m.state == MountState::Orphaned),
            "an alias remote's mount is still ours: {found:?}"
        );
    }

    /// Reporting it as ours is only half the promise. The pre-flight check that the point
    /// serves what the mount owns compares the same rewritten source, so on these configs
    /// it refused the unmount the `Orphaned` row exists to offer — and refused the
    /// configured mount's own unmount too, which never needed a rename to reach.
    #[tokio::test]
    async fn a_rewritten_source_does_not_refuse_the_unmount_of_a_unit_that_owns_the_point() {
        for (case, unit, addressed) in [
            ("alias-orphan", "rvt-mount-backup.service", "backup"),
            ("alias-configured", "rvt-mount-backups.service", "backups"),
        ] {
            let (sc, s) = renamed(case);
            releasing(&s);
            // What the unit was started with. The fixture's mountinfo reports
            // `backup:pictures`, as rclone does once it has resolved an alias.
            s.units.loaded.lock().unwrap()[0] = LoadedUnit {
                name: unit.into(),
                status: UnitStatus::Active,
                serving: Some(Serving {
                    fs_spec: "ali:".into(),
                    mount_point: mount_point(&sc),
                }),
            };

            s.unmount(addressed, false)
                .await
                .unwrap_or_else(|e| panic!("{case}: the unit's argv names this point: {e}"));
            assert_eq!(
                s.units.stopped.lock().unwrap().as_slice(),
                &[unit.to_string()],
                "{case}"
            );
        }
    }

    /// The `Activating` half of the state guard, where nothing else can catch it: a unit
    /// awaiting restart at a path no config entry names. Its previous rclone has already
    /// exited, so whatever the kernel still lists there is a corpse or a stranger, and
    /// neither is a mount to report as live and ours.
    #[tokio::test]
    async fn a_unit_awaiting_restart_claims_nothing_at_all() {
        let other = "/tmp/somebody-elses";
        let (_sc, s) = supervisor("gap-alone", true, &[other], UnitStatus::Active);
        s.units.loaded.lock().unwrap().push(LoadedUnit {
            name: "rvt-mount-elsewhere.service".into(),
            status: UnitStatus::Activating,
            serving: Some(Serving {
                fs_spec: "backup:pictures".into(),
                mount_point: PathBuf::from(other),
            }),
        });

        let found = s.reconcile().await.unwrap();
        assert!(
            !found.iter().any(|m| m.state == MountState::Orphaned),
            "a unit between rclones is serving nothing: {found:?}"
        );
        assert!(
            found
                .iter()
                .any(|m| m.name == other && m.state == MountState::Foreign),
            "and whatever is actually there is still reported: {found:?}"
        );
    }

    /// The state guard on its own. A unit that failed is not serving whatever now sits at
    /// the path its argv names, and claiming it would put a stranger's mount within reach
    /// of `force` — where `-z` strands an rclone that was never signalled, which
    /// DESIGN.md forbids.
    #[tokio::test]
    async fn a_dead_unit_does_not_claim_a_mount_nothing_of_ours_serves() {
        let (_sc, s) = renamed("rename-dead-alone");
        s.units.loaded.lock().unwrap()[0].status = UnitStatus::Failed;

        let found = s.reconcile().await.unwrap();
        assert!(
            !found.iter().any(|m| m.state == MountState::Orphaned),
            "a failed unit serves nothing: {found:?}"
        );
        assert_eq!(s.state("backups").await.unwrap(), MountState::Foreign);
        match s.unmount("backup", true).await {
            Err(SupervisorError::UnknownMount(n)) => assert_eq!(n, "backup"),
            other => panic!("a dead unit must not be addressable: {other:?}"),
        }
    }

    /// The redirect has to fire on any unit that is not *running* under the new name, not
    /// only on one systemd has never heard of. A failed unit serves nothing, so the point
    /// belongs to the orphan and releasing it is the original #71 damage over again.
    #[tokio::test]
    async fn a_failed_unit_under_the_new_name_does_not_capture_the_unmount() {
        let (_sc, s) = renamed("rename-failed-new");
        releasing(&s);
        s.units.loaded.lock().unwrap().push(LoadedUnit {
            name: "rvt-mount-backups.service".into(),
            status: UnitStatus::Failed,
            serving: None,
        });

        s.unmount("backups", false)
            .await
            .expect("the orphan still holds the point");
        assert_eq!(
            s.units.stopped.lock().unwrap().as_slice(),
            &["rvt-mount-backup.service".to_string()],
            "stopping the dead unit under the new name releases the orphan's live mount \
             and leaves the orphan running"
        );
    }

    /// Both cases where a unit of ours is loaded but there is nothing to act on: it is
    /// serving nothing, or its argv does not say what it serves. Reporting either as a
    /// mount would put a row in the list that no action fits.
    #[tokio::test]
    async fn a_loaded_unit_serving_nothing_is_not_an_orphan() {
        let (_sc, s) = renamed("rename-idle");
        s.units.loaded.lock().unwrap().extend([
            LoadedUnit {
                name: "rvt-mount-gone.service".into(),
                status: UnitStatus::Failed,
                serving: Some(Serving {
                    fs_spec: "backup:gone".into(),
                    mount_point: PathBuf::from("/tmp/never-mounted"),
                }),
            },
            LoadedUnit {
                name: "rvt-mount-opaque.service".into(),
                status: UnitStatus::Active,
                serving: None,
            },
        ]);
        let found = s.reconcile().await.unwrap();
        assert!(
            !found.iter().any(|m| m.name == "gone" || m.name == "opaque"),
            "{found:?}"
        );
    }

    #[test]
    fn a_unit_name_maps_back_to_the_mount_it_was_started_for() {
        assert_eq!(
            orphan_name("rvt-mount-backup.service").as_deref(),
            Some("backup")
        );
        // Round trip, since this is what addresses the unit afterwards.
        assert_eq!(
            a_mount("my.mount_1-a", PathBuf::from("/mnt")).unit_name(),
            "rvt-mount-my.mount_1-a.service"
        );
        assert_eq!(
            orphan_name("rvt-mount-my.mount_1-a.service").as_deref(),
            Some("my.mount_1-a")
        );
        for not_ours in [
            "rclone-backup.service",
            "rvt-mount-.service",
            "rvt-mount-backup.mount",
            "rvt-mount-backup",
            // Rejected by `Config::validate` too, so nothing of ours is named this.
            "rvt-mount-..service",
            "rvt-mount-...service",
        ] {
            assert_eq!(orphan_name(not_ours), None, "{not_ours}");
        }
    }
}
