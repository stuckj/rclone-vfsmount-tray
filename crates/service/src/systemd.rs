//! Starting rclone as a transient systemd user unit.
//!
//! Mounts run as units rather than as children of this service, so they outlive it. That
//! is the whole reason for the systemd dependency: the service crashes, and it gets
//! restarted, and nobody expects either to unmount their filesystems. See DESIGN.md
//! and #54.
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

impl UnitStatus {
    /// Whether this unit can be what is mounted at a point right now.
    ///
    /// `Failed` cannot: its main process is gone, and `CollectMode`'s default keeps the
    /// unit loaded afterwards purely so its state and log survive. Neither can
    /// `Activating`, which is both the moments before rclone is exec'd *and* the gap
    /// systemd leaves before a restart — in that gap the previous rclone has already
    /// exited, so the unit is running and serving nothing.
    ///
    /// Matched exhaustively so a future variant cannot default to serving.
    pub fn is_serving(self) -> bool {
        match self {
            UnitStatus::Active | UnitStatus::Deactivating => true,
            UnitStatus::Activating | UnitStatus::Failed | UnitStatus::Inactive => false,
        }
    }
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
                    // Only a service has an `ExecStart`. A `.mount` or `.timer` sharing
                    // the prefix would answer the property read with an interface error
                    // on every sweep, which is worth neither the round trip nor the log.
                    let serving = if name.ends_with(".service") {
                        self.serving(path).await
                    } else {
                        None
                    };
                    out.push(LoadedUnit {
                        name,
                        status: unit_status(&active),
                        serving,
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
                // `json`, not `cat`, so the unit's own output can be told from systemd's
                // narration about it — see [`what_the_unit_said`].
                let out = tokio::process::Command::new("journalctl")
                    .args(["--user", "-u", unit, "-n", "20", "--no-pager", "-o", "json"])
                    .output()
                    .await;
                match out {
                    Ok(o) => what_the_unit_said(&String::from_utf8_lossy(&o.stdout)),
                    Err(_) => String::new(),
                }
            })
        }
    }
}

/// The unit's own output, taken out of a journal window that also carries systemd's
/// narration about the unit.
///
/// This becomes a mount's failure reason, and the whole point of it is to say what rclone
/// objected to. A window of twenty entries around a failure is mostly `Starting…`,
/// `Started…`, `Main process exited…`, `Failed with result…` and `Scheduled restart job…`,
/// repeated once per attempt, with rclone's one line somewhere inside — so a reader is given
/// several screens of systemd before reaching the part that helps.
///
/// Told apart by `_COMM`, which is the process that logged the entry: the user manager logs
/// as `systemd`, and everything the unit runs logs as itself. Not by matching the message
/// text, which systemd translates.
///
/// Falls back to everything when that leaves nothing, since a unit that died before it could
/// say anything still has systemd's account, and that beats an empty reason.
fn what_the_unit_said(journal: &str) -> String {
    let entries: Vec<(bool, String)> = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|e| {
            let text = match e.get("MESSAGE") {
                Some(serde_json::Value::String(s)) => s.clone(),
                // Non-UTF-8 output arrives as an array of bytes.
                Some(serde_json::Value::Array(bytes)) => {
                    let raw: Vec<u8> = bytes
                        .iter()
                        .filter_map(|b| b.as_u64())
                        .map(|b| b as u8)
                        .collect();
                    String::from_utf8_lossy(&raw).into_owned()
                }
                _ => return None,
            };
            // rclone ends its fatal messages with a newline, which the journal keeps as an
            // entry of its own. Dropped here, so the repeats around it end up adjacent and
            // can be collapsed below.
            if text.trim().is_empty() {
                return None;
            }
            let from_systemd = e.get("_COMM").and_then(|c| c.as_str()) == Some("systemd");
            Some((from_systemd, text))
        })
        .collect();

    let own: Vec<&str> = entries
        .iter()
        .filter(|(from_systemd, _)| !from_systemd)
        .map(|(_, text)| text.as_str())
        .collect();
    let shown: Vec<&str> = if own.is_empty() {
        entries.iter().map(|(_, text)| text.as_str()).collect()
    } else {
        own
    };
    // `Restart=on-failure` runs the same command into the same wall, so the window holds one
    // complaint per attempt. Saying it three times does not make it any more informative.
    let mut said: Vec<&str> = Vec::with_capacity(shown.len());
    for line in shown {
        if said.last() != Some(&line) {
            said.push(line);
        }
    }
    said.join("\n").trim().to_string()
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

        // Torn down before anything is asserted: the stub sleeps for 30s under
        // `Restart=on-failure`, so a failing assertion would leave it running and the
        // next run would start another beside it under a fresh pid.
        let seen = found.iter().find(|u| u.name == name).cloned();
        let _ = units.stop(&name).await;
        let _ = units.reset_failed(&name).await;

        let unit = seen.unwrap_or_else(|| panic!("the unit just started is not in {found:?}"));
        assert_eq!(
            unit.serving,
            Some(Serving {
                fs_spec: "selftest:".into(),
                mount_point: PathBuf::from(point),
            }),
            "the mount point has to come back out of the unit's own argv, or an orphan \
             can never be matched to what it is serving"
        );
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

    /// A window around a real failure, captured from `journalctl -o json` on 2026-08-20:
    /// a mount point that was not empty, retried until systemd gave up. Trimmed to the
    /// fields this reads, and to one restart cycle of the three that were there.
    const A_REAL_WINDOW: &str = concat!(
        r#"{"_COMM":"systemd","MESSAGE":"Starting rvt-mount-bad.service - rclone mount jsrc: at /home/j/mnt..."}"#,
        "\n",
        r#"{"_COMM":"systemd","MESSAGE":"Started rvt-mount-bad.service - rclone mount jsrc: at /home/j/mnt."}"#,
        "\n",
        r#"{"_COMM":"rclone","MESSAGE":"ERROR+4: Fatal error: failed to mount FUSE fs: \"/home/j/mnt\" is not empty, use --allow-non-empty to mount anyway"}"#,
        "\n",
        r#"{"_COMM":"systemd","MESSAGE":"rvt-mount-bad.service: Main process exited, code=exited, status=1/FAILURE"}"#,
        "\n",
        r#"{"_COMM":"systemd","MESSAGE":"rvt-mount-bad.service: Failed with result 'exit-code'."}"#,
        "\n",
        r#"{"_COMM":"systemd","MESSAGE":"rvt-mount-bad.service: Scheduled restart job, restart counter is at 1."}"#,
        "\n",
        r#"{"_COMM":"systemd","MESSAGE":"rvt-mount-bad.service: Start request repeated too quickly."}"#,
        "\n",
        r#"{"_COMM":"systemd","MESSAGE":"Failed to start rvt-mount-bad.service - rclone mount jsrc: at /home/j/mnt."}"#,
        "\n",
    );

    #[test]
    fn a_failure_reason_is_what_rclone_said_not_what_systemd_said_about_it() {
        // Three lines in twenty were rclone's in the window this came from. Handing the
        // whole thing over buries the one sentence that says what to fix.
        let said = what_the_unit_said(A_REAL_WINDOW);
        assert_eq!(
            said,
            "ERROR+4: Fatal error: failed to mount FUSE fs: \"/home/j/mnt\" is not empty, \
             use --allow-non-empty to mount anyway"
        );
    }

    #[test]
    fn one_complaint_repeated_by_a_restart_loop_is_said_once() {
        // systemd retries three times before giving up, and rclone meets the same wall each
        // time, so the window carries the same sentence three times over — separated by the
        // blank entry rclone's trailing newline leaves behind.
        let thrice = A_REAL_WINDOW.repeat(3);
        let said = what_the_unit_said(&thrice);
        assert_eq!(said.lines().count(), 1, "{said}");
        assert!(said.contains("use --allow-non-empty"), "{said}");
    }

    #[test]
    fn a_unit_that_died_before_it_spoke_still_gets_systemds_account() {
        // Better than an empty reason: an exec that could not start, or a binary that is
        // not there, leaves nothing of its own in the window.
        let only_systemd = concat!(
            r#"{"_COMM":"systemd","MESSAGE":"rvt-mount-bad.service: Failed to locate executable /usr/bin/rclone: No such file or directory"}"#,
            "\n",
            r#"{"_COMM":"systemd","MESSAGE":"rvt-mount-bad.service: Failed with result 'exit-code'."}"#,
            "\n",
        );
        let said = what_the_unit_said(only_systemd);
        assert!(said.contains("Failed to locate executable"), "{said}");
    }

    #[test]
    fn a_window_that_cannot_be_read_reports_nothing_rather_than_failing() {
        assert_eq!(what_the_unit_said(""), "");
        assert_eq!(what_the_unit_said("not json at all\n"), "");
    }

    #[test]
    fn output_that_is_not_utf8_still_reaches_the_reason() {
        // journalctl hands those over as an array of bytes rather than a string.
        let raw = r#"{"_COMM":"rclone","MESSAGE":[104,105,255,33]}"#;
        assert!(
            what_the_unit_said(raw).starts_with("hi"),
            "{}",
            what_the_unit_said(raw)
        );
    }
}
