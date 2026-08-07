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
    /// Applied to rclone's own file creation. rclone creates its rc socket 0775 whatever
    /// it is asked for, and rc access is equivalent to shell access as this user, so this
    /// is what actually keeps the socket private. Raising it reopens that hole.
    pub umask: u32,
}

/// Whether a unit is running, from systemd's point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnitStatus {
    Active,
    Activating,
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

                let properties: Vec<(&str, Value<'_>)> = vec![
                    ("Description", Value::from(spec.description.clone())),
                    ("ExecStart", Value::from(exec_start)),
                    ("Type", Value::from("exec")),
                    ("UMask", Value::from(spec.umask)),
                    // Restart the mount if rclone dies, but give up rather than loop:
                    // a remote that rejects the credentials fails identically forever.
                    ("Restart", Value::from("on-failure")),
                    ("RestartSec", Value::from(5_u64)),
                    // Keep the unit loaded after it fails so its state and logs can be
                    // read; `reset_failed` clears it before a retry.
                    ("CollectMode", Value::from("inactive-or-failed")),
                    // rclone needs a SIGTERM to flush and unmount cleanly; killing the
                    // whole cgroup immediately would leave the mount point stale.
                    ("KillMode", Value::from("mixed")),
                    ("TimeoutStopSec", Value::from(30_u64)),
                ];

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
                mgr.stop_unit(unit, "replace")
                    .await
                    .map_err(|e| supervision(&format!("stopping unit {unit}"), e))?;
                Ok(())
            })
        }

        fn status<'a>(
            &'a self,
            unit: &'a str,
        ) -> BoxFuture<'a, Result<UnitStatus, SupervisorError>> {
            Box::pin(async move {
                let mgr = self.manager().await?;
                // A unit that was never started is not loaded at all, and `GetUnit`
                // errors rather than reporting a state — that is Inactive, not a fault.
                let Ok(path) = mgr.get_unit(unit).await else {
                    return Ok(UnitStatus::Inactive);
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
                    "activating" | "deactivating" => UnitStatus::Activating,
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

    #[test]
    fn the_umask_that_protects_the_rc_socket_is_carried_on_the_spec() {
        // rclone creates the socket 0775; 0077 is what brings it to 0700. This is the
        // only thing standing between a shared-group login and rc access.
        let spec = UnitSpec {
            name: "rvt-mount-backup.service".into(),
            description: "rclone mount backup:".into(),
            executable: PathBuf::from("/usr/bin/rclone"),
            args: vec!["mount".into()],
            umask: 0o077,
        };
        assert_eq!(
            spec.umask & 0o007,
            0o007,
            "group and other must be masked off"
        );
    }
}
