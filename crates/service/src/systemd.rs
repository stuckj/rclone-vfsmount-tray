//! Starting rclone as a transient systemd user unit.
//!
//! Mounts run as units rather than as children of this service, so they outlive it. That
//! is the whole reason for the systemd dependency: `apt upgrade` restarts the service,
//! and nobody expects that to unmount their filesystems. See DESIGN.md and #54.
//!
//! [`UnitManager`] exists so the supervisor can be tested without a session bus.

use rvt_core::supervisor::{BoxFuture, SupervisorError};
use std::path::PathBuf;

/// Everything needed to start one rclone unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSpec {
    pub name: String,
    pub description: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    /// Run before the main process, on every start including systemd's own restarts.
    ///
    /// Failure is ignored: this only clears leftovers, and refusing to start because the
    /// cleanup could not run would turn a recoverable state into a permanent one.
    pub pre_start: Option<(PathBuf, Vec<String>)>,
    /// rclone applies this to every file it creates inside the mount, not just its own:
    /// `--umask` defaults to the process umask. So it must stay ordinary — the rc socket
    /// is protected by the mode of the directory holding it.
    pub umask: u32,
}

/// Whether a unit is running, from systemd's point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnitStatus {
    Active,
    Activating,
    /// Being stopped. Distinct from `Activating` because the supervisor maps it to
    /// `Unmounting`, and a mount being torn down must not report as coming up.
    Deactivating,
    Failed,
    /// Not loaded, or loaded and dead. Both mean "not running and not trying to".
    #[default]
    Inactive,
}

/// What a unit's own argv says it mounts, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    /// The `remote:path` rclone was given.
    pub fs_spec: String,
    pub mount_point: PathBuf,
}

/// A unit systemd has loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedUnit {
    pub name: String,
    pub status: UnitStatus,
    /// Read back from the unit's argv, for units shaped like the ones this service
    /// starts. `None` for anything else, and for a read that failed — a unit whose
    /// mount point cannot be established is left alone rather than guessed at.
    pub serving: Option<Serving>,
}

/// Start and stop units.
///
/// Boxed futures for the same reason as `MountSupervisor`: the supervisor holds this as a
/// generic, but tests substitute a double and these calls fire at human frequency.
pub trait UnitManager: Send + Sync {
    fn start_transient<'a>(
        &'a self,
        spec: &'a UnitSpec,
    ) -> BoxFuture<'a, Result<(), SupervisorError>>;

    fn stop<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>>;

    fn status<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<UnitStatus, SupervisorError>>;

    /// Every unit systemd currently has loaded whose name begins with `prefix`.
    ///
    /// Loaded is the right set: a transient unit that stopped cleanly is collected and
    /// gone, and one that failed stays — `CollectMode` is left at its default precisely
    /// so it does. What comes back is therefore every unit of ours still accounted for
    /// by systemd, whether or not any config entry still names it.
    fn list_units<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<LoadedUnit>, SupervisorError>>;

    /// Clear a failed unit so the name can be reused. systemd keeps a failed transient
    /// unit loaded, and `StartTransientUnit` refuses a name that still is.
    fn reset_failed<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>>;

    /// The tail of the unit's log, for reporting why a mount failed. Returning nothing is
    /// acceptable; failing to read it must not turn a mount failure into a supervisor one.
    fn recent_output<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, String>;
}

/// systemd's user instance, over D-Bus.
pub mod dbus {
    use super::*;
    use zbus::zvariant::{OwnedObjectPath, Value};

    #[zbus::proxy(
        interface = "org.freedesktop.systemd1.Manager",
        default_service = "org.freedesktop.systemd1",
        default_path = "/org/freedesktop/systemd1"
    )]
    trait Manager {
        fn start_transient_unit(
            &self,
            name: &str,
            mode: &str,
            properties: &[(&str, Value<'_>)],
            aux: &[(&str, Vec<(&str, Value<'_>)>)],
        ) -> zbus::Result<OwnedObjectPath>;

        fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

        fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;

        fn get_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

        /// Loaded units whose names match any of the globs, optionally narrowed to a set
        /// of active states. An empty `states` means every state.
        fn list_units_by_patterns(
            &self,
            states: &[&str],
            patterns: &[&str],
        ) -> zbus::Result<Vec<UnitRecord>>;
    }

    /// One row of `ListUnitsByPatterns`, signature `(ssssssouso)`: unit name,
    /// description, load state, active state, sub state, the unit it is followed by,
    /// its object path, then the id, type and object path of any queued job.
    type UnitRecord = (
        String,
        String,
        String,
        String,
        String,
        String,
        OwnedObjectPath,
        u32,
        String,
        OwnedObjectPath,
    );

    /// One `ExecStart=` entry, signature `(sasbttttuii)`: the binary, its full argv,
    /// whether a non-zero exit is ignored, then start and exit timestamps, the last
    /// PID, and the exit code and status.
    type ExecCommand = (String, Vec<String>, bool, u64, u64, u64, u64, u32, i32, i32);

    #[zbus::proxy(
        interface = "org.freedesktop.systemd1.Service",
        default_service = "org.freedesktop.systemd1"
    )]
    trait Service {
        #[zbus(property)]
        fn exec_start(&self) -> zbus::Result<Vec<ExecCommand>>;
    }

    #[zbus::proxy(
        interface = "org.freedesktop.systemd1.Unit",
        default_service = "org.freedesktop.systemd1"
    )]
    trait Unit {
        #[zbus(property)]
        fn active_state(&self) -> zbus::Result<String>;
    }

    /// Talks to the systemd user instance on the session bus.
    pub struct SystemdUnits {
        conn: zbus::Connection,
    }

    impl SystemdUnits {
        pub async fn connect() -> Result<Self, SupervisorError> {
            let conn = zbus::Connection::session().await.map_err(|e| {
                supervision(
                    "connecting to the session bus — the service needs a systemd user instance",
                    e,
                )
            })?;
            Ok(Self { conn })
        }

        async fn manager(&self) -> Result<ManagerProxy<'_>, SupervisorError> {
            ManagerProxy::new(&self.conn)
                .await
                .map_err(|e| supervision("opening the systemd manager interface", e))
        }

        /// What a unit mounts, read back from the argv systemd holds for it.
        ///
        /// `None` rather than an error: this only adds detail to a unit already listed,
        /// and a unit whose argv is not one of ours is not something to fail a sweep
        /// over. A failure to *read* it is different — the unit then goes unrecognised
        /// and its mount reads as somebody else's, so it is logged rather than passed
        /// over in the same silence.
        async fn serving(&self, unit: OwnedObjectPath) -> Option<Serving> {
            let name = unit.as_str().to_string();
            // `CacheProperties::No`: the default caches lazily, which turns each read
            // into a `PropertiesChanged` match rule plus a `GetAll` of every property on
            // the interface — measured at ~12 kB against ~390 bytes for this one — for a
            // proxy that is dropped at the end of this call.
            let built = match ServiceProxy::builder(&self.conn).path(unit) {
                Ok(b) => {
                    b.cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                }
                Err(e) => Err(e),
            };
            let svc = match built {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(unit = %name, error = %e, "cannot address this unit");
                    return None;
                }
            };
            let exec = match svc.exec_start().await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        unit = %name,
                        error = %e,
                        "cannot read this unit's ExecStart, so it cannot be matched to a \
                         mount point; anything it is serving will read as unmanaged"
                    );
                    return None;
                }
            };
            let (_, argv, ..) = exec.first()?;
            // argv[0] is the binary, and `Mount::mount_args` puts the subcommand and its
            // two positional arguments immediately after it.
            if argv.get(1).map(String::as_str) != Some("mount") {
                return None;
            }
            Some(Serving {
                fs_spec: argv.get(2)?.clone(),
                mount_point: PathBuf::from(argv.get(3)?),
            })
        }
    }

    /// systemd's `ActiveState` as this service reads it.
    fn unit_status(active_state: &str) -> UnitStatus {
        match active_state {
            "active" | "reloading" => UnitStatus::Active,
            "activating" => UnitStatus::Activating,
            "deactivating" => UnitStatus::Deactivating,
            "failed" => UnitStatus::Failed,
            _ => UnitStatus::Inactive,
        }
    }

    fn supervision(
        context: &str,
        e: impl std::error::Error + Send + Sync + 'static,
    ) -> SupervisorError {
        SupervisorError::Supervision {
            context: context.to_string(),
            source: Some(Box::new(e)),
        }
    }

    impl UnitManager for SystemdUnits {
        fn start_transient<'a>(
            &'a self,
            spec: &'a UnitSpec,
        ) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                let mgr = self.manager().await?;
                let exec = spec.executable.to_string_lossy().into_owned();
                // a(sasb): the binary, its full argv including argv[0], and whether a
                // non-zero exit should be ignored.
                let mut argv = vec![exec.clone()];
                argv.extend(spec.args.iter().cloned());
                let exec_start = vec![(exec.clone(), argv, false)];

                let mut properties: Vec<(&str, Value<'_>)> = vec![
                    ("Description", Value::from(spec.description.clone())),
                    ("ExecStart", Value::from(exec_start)),
                    ("Type", Value::from("exec")),
                    ("UMask", Value::from(spec.umask)),
                    // Restart if rclone dies, but give up rather than loop. The limit is
                    // explicit because systemd's default burst of 5 within 10s is never
                    // reached by restarts 5s apart, so the unit would never reach `failed`
                    // — the state that carries rclone's reason to the user.
                    ("Restart", Value::from("on-failure")),
                    ("StartLimitIntervalUSec", Value::from(60_000_000_u64)),
                    ("StartLimitBurst", Value::from(3_u32)),
                    // D-Bus property names, not unit-file directives: there is no
                    // `RestartSec` or `TimeoutStopSec` property, both are microseconds, and
                    // an unrecognised name fails the whole call rather than being ignored.
                    ("RestartUSec", Value::from(5_000_000_u64)),
                    // rclone unmounts on a SIGTERM; killing the whole cgroup immediately
                    // would leave the mount point stale. It does *not* flush the
                    // write-back cache first — measured on v1.75.0, it exits in under a
                    // second with the queue still full, and the next mount on the same
                    // `--cache-dir` uploads what was left.
                    ("KillMode", Value::from("mixed")),
                    ("TimeoutStopUSec", Value::from(30_000_000_u64)),
                    // `CollectMode` is deliberately unset: its `inactive` default keeps a
                    // failed unit loaded so its state and log survive to be reported.
                ];

                if let Some((bin, args)) = &spec.pre_start {
                    let exe = bin.to_string_lossy().into_owned();
                    let mut argv = vec![exe.clone()];
                    argv.extend(args.iter().cloned());
                    // The trailing `true` is `ignore_errors`.
                    properties.push(("ExecStartPre", Value::from(vec![(exe, argv, true)])));
                }

                // "fail" rather than "replace": if something already holds this unit
                // name we want to hear about it, not silently displace it.
                mgr.start_transient_unit(&spec.name, "fail", &properties, &[])
                    .await
                    .map_err(|e| supervision(&format!("starting unit {}", spec.name), e))?;
                Ok(())
            })
        }

        fn stop<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                let mgr = self.manager().await?;
                match mgr.stop_unit(unit, "replace").await {
                    Ok(_) => Ok(()),
                    // It stopped between the status read and here, and systemd collected
                    // it. The caller asked for it to be stopped, and it is.
                    Err(zbus::Error::MethodError(name, msg, _))
                        if name.as_str().ends_with(".NoSuchUnit")
                            || msg.as_deref().is_some_and(|m| m.contains("not loaded")) =>
                    {
                        Ok(())
                    }
                    Err(e) => Err(supervision(&format!("stopping unit {unit}"), e)),
                }
            })
        }

        fn status<'a>(
            &'a self,
            unit: &'a str,
        ) -> BoxFuture<'a, Result<UnitStatus, SupervisorError>> {
            Box::pin(async move {
                let mgr = self.manager().await?;
                // A unit that was never started answers `NoSuchUnit`, which is Inactive
                // rather than a fault. Every other failure must be reported: an
                // unreachable manager read as "no unit" would resolve every live mount to
                // `Foreign`.
                let path = match mgr.get_unit(unit).await {
                    Ok(p) => p,
                    Err(zbus::Error::MethodError(name, _, _))
                        if name.as_str().ends_with(".NoSuchUnit") =>
                    {
                        return Ok(UnitStatus::Inactive)
                    }
                    Err(e) => return Err(supervision(&format!("looking up unit {unit}"), e)),
                };
                let proxy = UnitProxy::builder(&self.conn)
                    .path(path)
                    .map_err(|e| supervision("addressing the unit", e))?
                    .build()
                    .await
                    .map_err(|e| supervision("opening the unit interface", e))?;
                let state = proxy
                    .active_state()
                    .await
                    .map_err(|e| supervision(&format!("reading state of {unit}"), e))?;
                Ok(unit_status(&state))
            })
        }

        fn list_units<'a>(
            &'a self,
            prefix: &'a str,
        ) -> BoxFuture<'a, Result<Vec<LoadedUnit>, SupervisorError>> {
            Box::pin(async move {
                let mgr = self.manager().await?;
                let pattern = format!("{prefix}*");
                let rows = mgr
                    .list_units_by_patterns(&[], &[&pattern])
                    .await
                    .map_err(|e| supervision(&format!("listing units matching {pattern}"), e))?;

                let mut out = Vec::with_capacity(rows.len());
                for (name, _, _, active, _, _, path, ..) in rows {
                    out.push(LoadedUnit {
                        name,
                        status: unit_status(&active),
                        serving: self.serving(path).await,
                    });
                }
                Ok(out)
            })
        }

        fn reset_failed<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                let mgr = self.manager().await?;
                // Not an error if there was nothing to reset.
                let _ = mgr.reset_failed_unit(unit).await;
                Ok(())
            })
        }

        fn recent_output<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, String> {
            Box::pin(async move {
                let out = tokio::process::Command::new("journalctl")
                    .args(["--user", "-u", unit, "-n", "20", "--no-pager", "-o", "cat"])
                    .output()
                    .await;
                match out {
                    Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    Err(_) => String::new(),
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_testutil::Scratch;

    /// Exercise the real property set against the running systemd.
    ///
    /// `StartTransientUnit` rejects the whole call on one unrecognised property name, and
    /// the D-Bus names differ from the unit-file directives — `RestartSec` does not exist
    /// as a property, only `RestartUSec`. No amount of testing against a double catches
    /// that, because the double accepts whatever it is handed.
    ///
    /// Skipped where there is no systemd user instance, which includes most CI runners.
    #[tokio::test]
    async fn the_property_set_is_one_systemd_actually_accepts() {
        let Ok(units) = dbus::SystemdUnits::connect().await else {
            eprintln!("skipped: no session bus");
            return;
        };

        let name = format!("rvt-selftest-{}.service", std::process::id());
        // `sleep` stands in for rclone: the point is the property set, not the payload.
        let spec = UnitSpec {
            name: name.clone(),
            description: "rclone-vfsmount-tray self test".into(),
            executable: PathBuf::from("/bin/sleep"),
            args: vec!["30".into()],
            pre_start: None,
            umask: 0o077,
        };

        let started = units.start_transient(&spec).await;
        // A systemd that is not the user instance (no --user manager) shows up here
        // rather than at connect, and is a skip rather than a failure.
        if let Err(e) = &started {
            let msg = e.to_string();
            if msg.contains("ServiceUnknown") || msg.contains("not supported") {
                eprintln!("skipped: no systemd user instance ({msg})");
                return;
            }
        }
        started.expect("systemd must accept every property this sends");

        let mut status = UnitStatus::Inactive;
        for _ in 0..40 {
            status = units.status(&name).await.expect("status must be readable");
            if status == UnitStatus::Active {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(status, UnitStatus::Active, "the unit should have started");

        units.stop(&name).await.expect("stop must succeed");
        let _ = units.reset_failed(&name).await;
    }

    /// The restart policy must actually give up.
    ///
    /// Testing only a unit that starts cleanly says nothing about the failure path, which
    /// is the path that has to produce `Failed` for rclone's reason to reach the user.
    #[tokio::test]
    async fn a_unit_that_keeps_failing_reaches_failed_rather_than_looping() {
        let Ok(units) = dbus::SystemdUnits::connect().await else {
            eprintln!("skipped: no session bus");
            return;
        };

        let name = format!("rvt-failtest-{}.service", std::process::id());
        let _ = units.reset_failed(&name).await;
        let spec = UnitSpec {
            name: name.clone(),
            description: "rclone-vfsmount-tray failure self test".into(),
            executable: PathBuf::from("/bin/false"),
            args: vec![],
            pre_start: None,
            umask: 0o022,
        };
        if units.start_transient(&spec).await.is_err() {
            eprintln!("skipped: no systemd user instance");
            return;
        }

        // Three starts at 5s apart inside a 60s window, so it should be spent well
        // before this deadline; a policy that never gives up runs out the clock.
        let mut last = UnitStatus::Inactive;
        for _ in 0..90 {
            last = units.status(&name).await.unwrap_or(UnitStatus::Inactive);
            if last == UnitStatus::Failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let _ = units.stop(&name).await;
        let _ = units.reset_failed(&name).await;

        assert_eq!(
            last,
            UnitStatus::Failed,
            "the unit must stop retrying and settle in failed, or MountState::Failed is \
             unreachable and rclone's error never reaches the user"
        );
    }

    #[tokio::test]
    async fn exec_start_pre_is_accepted_and_actually_runs() {
        // The recovery step that makes systemd's *automatic* restarts able to succeed.
        // `StartTransientUnit` rejects the whole call on an unrecognised property, and a
        // pre-start that silently never ran would leave the restart policy just as broken
        // as having none — so both halves need checking against a real manager.
        let Ok(units) = dbus::SystemdUnits::connect().await else {
            eprintln!("skipped: no session bus");
            return;
        };

        let dir = Scratch::new("pre");
        let marker = dir.write("marker", b"x");

        let name = format!("rvt-pretest-{}.service", std::process::id());
        let spec = UnitSpec {
            name: name.clone(),
            description: "pre-start self test".into(),
            executable: PathBuf::from("/bin/sleep"),
            args: vec!["5".into()],
            pre_start: Some((
                PathBuf::from("/bin/rm"),
                vec!["-f".into(), marker.to_string_lossy().into_owned()],
            )),
            umask: 0o022,
        };

        let started = units.start_transient(&spec).await;
        if let Err(e) = &started {
            let msg = e.to_string();
            if msg.contains("ServiceUnknown") || msg.contains("not supported") {
                eprintln!("skipped: no systemd user instance");
                return;
            }
        }
        started.expect("systemd must accept ExecStartPre");

        for _ in 0..40 {
            if !marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !marker.exists(),
            "ExecStartPre was accepted but never ran, so nothing clears a stale socket"
        );

        let _ = units.stop(&name).await;
        let _ = units.reset_failed(&name).await;
    }

    #[tokio::test]
    async fn a_killed_unit_is_restarted_and_the_hook_runs_again() {
        // `Restart=on-failure` and the pre-start hook only earn their place if systemd
        // actually re-runs both after a hard kill. Deleting either leaves a suite that
        // asserts a unit *settles in failed* — which "never retried at all" also
        // satisfies — so this is what stops the whole recovery path becoming dead weight.
        let Ok(units) = dbus::SystemdUnits::connect().await else {
            eprintln!("skipped: no session bus");
            return;
        };

        let dir = Scratch::new("restart");
        let marker = dir.join("marker");

        let name = format!("rvt-restarttest-{}.service", std::process::id());
        let sleep_arg = format!("{}.5", 600 + std::process::id() % 100);
        let _ = units.reset_failed(&name).await;
        let spec = UnitSpec {
            name: name.clone(),
            description: "restart self test".into(),
            executable: PathBuf::from("/bin/sleep"),
            // A duration no other test uses, so the kill below cannot reach theirs — the
            // suite runs in parallel and `pkill -f` matches on the command line.
            args: vec![sleep_arg.clone()],
            // Appends one line per start, so the count is the number of starts.
            pre_start: Some((
                PathBuf::from("/bin/sh"),
                vec![
                    "-c".into(),
                    format!("echo x >> {}", marker.to_string_lossy()),
                ],
            )),
            umask: 0o022,
        };
        if units.start_transient(&spec).await.is_err() {
            eprintln!("skipped: no systemd user instance");
            return;
        }

        // Wait for it to be up, then kill the payload so `on-failure` fires.
        for _ in 0..40 {
            if units.status(&name).await.ok() == Some(UnitStatus::Active) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = tokio::process::Command::new("pkill")
            .args(["-9", "-f", &format!("sleep {sleep_arg}")])
            .output()
            .await;

        // RestartUSec is 5s, so allow for one backoff plus slack.
        let mut starts = 0;
        for _ in 0..60 {
            starts = std::fs::read_to_string(&marker)
                .map(|s| s.lines().count())
                .unwrap_or(0);
            if starts >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(
            starts >= 2,
            "the unit ran its pre-start {starts} time(s): systemd either did not restart \
             it, or restarted it without re-running the hook that clears the leftovers"
        );

        let _ = units.stop(&name).await;
        let _ = units.reset_failed(&name).await;
    }

    /// `ListUnitsByPatterns` and the `ExecStart` property are decoded through tuple
    /// signatures written out by hand, and zbus checks them when the message arrives, not
    /// when the code is built. A double cannot catch a wrong one — it hands back whatever
    /// the test constructed — and a silent decode failure would leave the orphan sweep
    /// finding nothing, for ever.
    ///
    /// Swept under its own prefix rather than the real one, so a service running on the
    /// same machine does not watch a unit appear inside the namespace it owns.
    #[tokio::test]
    async fn the_unit_sweep_decodes_what_systemd_actually_sends() {
        let Ok(units) = dbus::SystemdUnits::connect().await else {
            eprintln!("skipped: no session bus");
            return;
        };

        // A stub that ignores its arguments and stays up, so the unit can carry a mount
        // unit's argv without the payload rejecting it. A unit that fails instead settles
        // in `failed` on its own schedule and cannot be reliably cleared afterwards.
        let dir = Scratch::new("sweep");
        let stub = dir.write("stub", "#!/bin/sh\nexec sleep 30\n");
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("the stub has to be executable");

        let prefix = format!("rvt-sweeptest-{}-", std::process::id());
        let name = format!("{prefix}one.service");
        let point = "/nonexistent/rvt-sweeptest";
        let _ = units.reset_failed(&name).await;
        let spec = UnitSpec {
            name: name.clone(),
            description: "sweep self test".into(),
            executable: stub,
            // Shaped like a mount unit's argv, so the mount point can be read back out.
            args: vec!["mount".into(), "selftest:".into(), point.into()],
            pre_start: None,
            umask: 0o022,
        };
        if units.start_transient(&spec).await.is_err() {
            eprintln!("skipped: no systemd user instance");
            return;
        }

        let mut found = Vec::new();
        for _ in 0..20 {
            found = units
                .list_units(&prefix)
                .await
                .expect("the sweep must decode");
            if !found.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let unit = found
            .iter()
            .find(|u| u.name == name)
            .unwrap_or_else(|| panic!("the unit just started is not in {found:?}"));
        assert_eq!(
            unit.serving,
            Some(Serving {
                fs_spec: "selftest:".into(),
                mount_point: PathBuf::from(point),
            }),
            "the mount point has to come back out of the unit's own argv, or an orphan \
             can never be matched to what it is serving"
        );

        let _ = units.stop(&name).await;
        let _ = units.reset_failed(&name).await;
    }

    #[tokio::test]
    async fn a_unit_that_was_never_started_reads_as_inactive_not_as_an_error() {
        // The supervisor calls this for every configured mount on every poll, including
        // ones that have never run. Surfacing that as an error would make a down mount
        // indistinguishable from a broken bus.
        let Ok(units) = dbus::SystemdUnits::connect().await else {
            eprintln!("skipped: no session bus");
            return;
        };
        let name = format!("rvt-absent-{}.service", std::process::id());
        match units.status(&name).await {
            Ok(s) => assert_eq!(s, UnitStatus::Inactive),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("ServiceUnknown") || msg.contains("not supported"),
                    "an absent unit must not error: {msg}"
                );
            }
        }
    }
}
