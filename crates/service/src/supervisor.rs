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
const POLL_INTERVAL: Duration = Duration::from_millis(250);

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
        }
    }

    /// Point state reads at a fixture instead of `/proc`, and shorten the readiness wait.
    #[cfg(test)]
    fn with_test_overrides(mut self, mountinfo_path: PathBuf, ready_timeout: Duration) -> Self {
        self.mountinfo_path = mountinfo_path;
        self.ready_timeout = ready_timeout;
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
        let live = mountinfo::is_mounted_at(&self.live_mounts(), &m.mount_point);
        let unit = self.units.status(&m.unit_name()).await?;
        Ok(match (live, unit) {
            (true, UnitStatus::Active | UnitStatus::Activating) => MountState::Mounted,
            // Mounted, but not by a unit of ours. Adopted for display only.
            (true, _) => MountState::Foreign,
            (false, UnitStatus::Activating) => MountState::Mounting,
            (false, UnitStatus::Failed) => MountState::Failed {
                reason: self.units.recent_output(&m.unit_name()).await,
            },
            (false, _) => MountState::Unmounted,
        })
    }

    /// Wait for the mount point to start serving.
    ///
    /// Polls the kernel rather than trusting the unit: systemd calls the unit active as
    /// soon as rclone has been exec'd, which is seconds before the mount point answers.
    async fn await_ready(&self, m: &Mount) -> Result<(), SupervisorError> {
        let deadline = std::time::Instant::now() + self.ready_timeout;
        loop {
            if mountinfo::is_mounted_at(&self.live_mounts(), &m.mount_point) {
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

    async fn await_gone(&self, m: &Mount, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !mountinfo::is_mounted_at(&self.live_mounts(), &m.mount_point) {
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

            // Already serving, however it got there. Mounting again would fail on a
            // busy mount point and tell the user nothing useful, and this is the path
            // taken after a service restart, when every mount is already up.
            if mountinfo::is_mounted_at(&self.live_mounts(), &m.mount_point) {
                return Ok(());
            }

            Self::prepare_mount_point(&m.mount_point)?;
            std::fs::create_dir_all(&self.runtime_dir).map_err(|e| {
                SupervisorError::Supervision {
                    context: format!(
                        "creating the runtime directory {}",
                        self.runtime_dir.display()
                    ),
                    source: Some(Box::new(e)),
                }
            })?;

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
                umask: 0o077,
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
            let state = self.resolve(m).await?;

            if !state.is_live() {
                return Ok(());
            }
            // We did not start it, so we do not take it down on our own initiative.
            // `force` is the caller having told the user that and been told to proceed.
            if !state.is_managed() && !force {
                return Err(SupervisorError::NotManaged(name.to_string()));
            }

            // The pending-upload check lands with the rc client (#12, #21, #23). It is a
            // warning rather than a refusal (#19): nothing is lost by unmounting, since
            // the write-back cache is on disk and resumes, so proceeding while the answer
            // is unknown is the correct default rather than a gap.

            if state.is_managed() {
                self.units.stop(&m.unit_name()).await?;
                if self.await_gone(m, Duration::from_secs(30)).await {
                    return Ok(());
                }
            }

            // Either it was foreign, or stopping the unit did not clear the mount point.
            Self::fusermount(&m.mount_point, false).await?;
            if self.await_gone(m, Duration::from_secs(10)).await {
                return Ok(());
            }
            Err(SupervisorError::Busy {
                path: format!(
                    "{} is still mounted after fusermount -u; something is holding it open. \
                     A lazy unmount would detach it from processes still writing to it, so \
                     it is not done automatically.",
                    m.mount_point.display()
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
            let configured: Vec<&Path> = self
                .config
                .mounts
                .iter()
                .map(|m| m.mount_point.as_path())
                .collect();
            for e in live.iter().filter(|e| e.is_rclone()) {
                if configured.contains(&e.mount_point.as_path()) {
                    continue;
                }
                out.push(DiscoveredMount::new(foreign_name(e), MountState::Foreign));
            }
            Ok(out)
        })
    }
}

/// A display name for a mount we did not configure.
///
/// The mount point rather than the remote, because that is what distinguishes two mounts
/// of the same remote and is what the user sees in their file manager.
fn foreign_name(e: &MountEntry) -> String {
    e.mount_point
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| e.source.clone())
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
            std::env::temp_dir().join("rvt-run"),
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
    async fn the_started_unit_masks_the_rc_socket() {
        let s = supervisor("umask", false, &[], UnitStatus::Inactive);
        let _ = s.mount("backup").await;
        let started = s.units.started.lock().unwrap();
        let spec = started.first().expect("a unit should have been started");
        assert_eq!(spec.umask, 0o077, "rclone creates the rc socket 0775");
        assert!(spec.args.iter().any(|a| a.starts_with("unix://")));
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

    #[tokio::test]
    async fn reconcile_reports_configured_and_foreign_mounts() {
        let other = "/tmp/somebody-elses";
        let s = supervisor("reconcile", true, &[other], UnitStatus::Active);
        let found = s.reconcile().await.unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].name, "backup");
        assert_eq!(found[0].state, MountState::Mounted);
        // The unconfigured one is reported rather than ignored.
        assert_eq!(found[1].name, "somebody-elses");
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
