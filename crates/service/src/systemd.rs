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
    /// The unit's umask, which rclone applies to every file and directory it creates
    /// inside the mount as well as to its own files: `--umask` defaults to the process
    /// umask, and rclone masks its 0777/0666 defaults with it.
    ///
    /// So this must stay the ordinary value a login shell would have. The rc socket is
    /// protected by the mode of the directory holding it, not by this.
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

    /// Clear a failed unit so the name can be reused.
    ///
    /// systemd keeps a failed transient unit loaded, and `StartTransientUnit` on a name
    /// that is still loaded fails with "unit already exists" — so a mount that failed
    /// once could never be retried without this.
    fn reset_failed<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>>;

    /// The tail of the unit's log, for reporting why a mount failed.
    ///
    /// Returning nothing is acceptable: a missing explanation is worse than none, but it
    /// must not turn a mount failure into a supervisor error.
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
                    // Restart the mount if rclone dies, but give up rather than loop: a
                    // remote that rejects the credentials fails identically forever.
                    //
                    // The limit has to be set explicitly. systemd's defaults are a burst
                    // of 5 within 10s, and restarts spaced 5s apart never put five starts
                    // in a 10s window — so the unit would retry for the whole login
                    // session and never reach `failed`, which is the state that carries
                    // rclone's reason to the user.
                    ("Restart", Value::from("on-failure")),
                    ("StartLimitIntervalUSec", Value::from(60_000_000_u64)),
                    ("StartLimitBurst", Value::from(3_u32)),
                    // These take systemd's *D-Bus property* names, which are not the
                    // unit-file directive names: there is no `RestartSec` or
                    // `TimeoutStopSec` property — `systemd-run` renames those client-side
                    // — and both of these are microseconds. An unrecognised name fails
                    // the whole call rather than being ignored, so a wrong name here
                    // means no mount can start at all.
                    ("RestartUSec", Value::from(5_000_000_u64)),
                    // rclone needs a SIGTERM to flush and unmount cleanly; killing the
                    // whole cgroup immediately would leave the mount point stale.
                    ("KillMode", Value::from("mixed")),
                    ("TimeoutStopUSec", Value::from(30_000_000_u64)),
                    // `CollectMode` is deliberately unset. It defaults to `inactive`,
                    // which keeps a failed unit loaded so its state and log can still be
                    // read. `inactive-or-failed` would garbage-collect precisely the
                    // failure that has to be reported, leaving the mount looking merely
                    // absent.
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
                    // It stopped on its own between the status read and here — rclone
                    // exited, or somebody ran `fusermount -u` — and systemd collected it.
                    // The caller asked for it to be stopped, and it is.
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
                // A unit that was never started is not loaded at all, and `GetUnit`
                // answers `NoSuchUnit` rather than reporting a state — that is Inactive,
                // not a fault.
                //
                // Every *other* failure must be reported. Treating an unreachable manager
                // as "no unit" would make every live mount resolve to `Foreign`, telling
                // the user their own mounts belong to somebody else and refusing to
                // unmount them, rather than saying the init system cannot be reached.
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
                Ok(match state.as_str() {
                    "active" | "reloading" => UnitStatus::Active,
                    "activating" => UnitStatus::Activating,
                    "deactivating" => UnitStatus::Deactivating,
                    "failed" => UnitStatus::Failed,
                    _ => UnitStatus::Inactive,
                })
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

        let marker = std::env::temp_dir().join(format!("rvt-pre-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        std::fs::write(&marker, b"x").unwrap();

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
        let _ = std::fs::remove_file(&marker);
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
