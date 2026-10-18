//! What happens to mounts when the service is told to stop.
//!
//! By default nothing: see DESIGN.md, "The lifetime rule". `unmount_on_service_stop` is
//! the one exception, and it applies only to an explicit stop. systemd sends the same
//! `SIGTERM` for a restart, and an upgrade or a `systemctl --user restart` taking every
//! filesystem down with it is exactly what the rule exists to prevent — so anything that
//! cannot be shown to be a plain stop leaves the mounts alone. A crash never gets here.

use crate::poller::MountPoller;
use crate::supervisor::rc_socket_path;
use rvt_core::supervisor::{BoxFuture, MountState, MountSupervisor};
use rvt_core::{Config, RcClient};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Why the service is stopping, as far as systemd will say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// A stop job on this service's unit: `systemctl --user stop`, the tray's Stop service,
    /// or the user manager shutting down at logout.
    Explicit,
    /// The stop half of a restart.
    Restart,
    /// Anything else — no job (a bare `kill`, `Ctrl-C` in a terminal), a job of some
    /// other type, or systemd could not be asked. Carries what was found, for the log.
    Unclear(String),
}

impl Stop {
    /// Read from the type of the job systemd has queued on this service's unit.
    pub fn from_job(job: Result<Option<String>, impl std::fmt::Display>) -> Self {
        match job {
            Ok(Some(t)) if t == "stop" => Stop::Explicit,
            Ok(Some(t)) if t == "restart" => Stop::Restart,
            Ok(Some(t)) => Stop::Unclear(format!("a {t:?} job")),
            Ok(None) => Stop::Unclear("no systemd job behind the signal".into()),
            Err(e) => Stop::Unclear(format!("systemd could not be asked: {e}")),
        }
    }
}

/// Whether this stop takes the mounts down.
pub fn unmounts(config: &Config, stop: &Stop) -> bool {
    config.global.unmount_on_service_stop && *stop == Stop::Explicit
}

/// Room left for systemd to see this process exit before it escalates to `SIGKILL`.
const MARGIN: Duration = Duration::from_secs(5);
/// Used when the unit's own stop timeout is unbounded or cannot be read.
const DEFAULT_BUDGET: Duration = Duration::from_secs(60);
/// However long systemd would wait, a logout should not.
const MAX_BUDGET: Duration = Duration::from_secs(120);

/// How long the whole unmount may take, given how long systemd will wait for this process.
pub fn budget(timeout_stop: Option<Duration>) -> Duration {
    match timeout_stop {
        Some(t) => t.saturating_sub(MARGIN).max(t / 2).min(MAX_BUDGET),
        None => DEFAULT_BUDGET,
    }
}

/// Waits until a mount has nothing visibly left to upload, or forever.
pub type Drain = Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;

/// Unmount every configured mount that is up or coming up, never forced, within `budget`.
///
/// Each is first given up to half the budget to upload what it has queued, since a stop at
/// logout is the moment "it resumes next time" is least likely to be soon. Then they are
/// unmounted concurrently. A refusal — something still using the mount — is logged and
/// leaves that mount up; the others go ahead. Whatever has not finished when the budget
/// runs out is left as it is.
///
/// Only configured mounts: a foreign one is not ours to take down, and an orphan is named
/// by nothing the user still has in their config.
pub async fn unmount_all(
    sup: Arc<dyn MountSupervisor>,
    config: &Config,
    budget: Duration,
    drain: Drain,
) {
    // Choosing what to unmount counts too: a wedged mount point can take seconds to say.
    if tokio::time::timeout(budget, unmount_within(sup, config, budget, drain))
        .await
        .is_err()
    {
        tracing::warn!(
            secs = budget.as_secs(),
            "unmount_on_service_stop ran out of time; anything not yet unmounted is left up"
        );
    }
}

async fn unmount_within(
    sup: Arc<dyn MountSupervisor>,
    config: &Config,
    budget: Duration,
    drain: Drain,
) {
    let mut targets = Vec::new();
    for m in &config.mounts {
        match sup.state(&m.name).await {
            Ok(MountState::Mounted | MountState::Mounting) => targets.push(m.name.clone()),
            Ok(_) => {}
            Err(e) => tracing::warn!(mount = %m.name, error = %e,
                "could not tell whether this mount is up; leaving it"),
        }
    }
    if targets.is_empty() {
        tracing::info!("unmount_on_service_stop: nothing is mounted");
        return;
    }
    tracing::info!(mounts = ?targets, "unmount_on_service_stop: unmounting");

    let mut tasks = tokio::task::JoinSet::new();
    for name in targets {
        let (sup, drain) = (sup.clone(), drain.clone());
        tasks.spawn(async move {
            if tokio::time::timeout(budget / 2, drain(name.clone()))
                .await
                .is_err()
            {
                tracing::warn!(mount = %name,
                    "uploads still queued; unmounting anyway — they resume on the next mount");
            }
            match sup.unmount(&name, false).await {
                Ok(()) => tracing::info!(mount = %name, "unmounted"),
                Err(e) => tracing::warn!(mount = %name, error = %e, "left mounted"),
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// A [`Drain`] that asks each mount's rclone what it still has queued.
///
/// Returns as soon as nothing is visibly pending, including when nothing can be seen at
/// all: an unobservable mount is not worth holding a logout for.
pub fn rc_drain(runtime_dir: PathBuf) -> Drain {
    Arc::new(move |name: String| {
        let socket = rc_socket_path(&runtime_dir, &name);
        Box::pin(async move {
            let mut poller = MountPoller::connect(&name, RcClient::new(&socket)).await;
            loop {
                let state = poller.poll().await;
                if state.pending.files == 0 {
                    return;
                }
                tracing::info!(mount = %name, files = state.pending.files,
                    "waiting for queued uploads before unmounting");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_core::supervisor::{DiscoveredMount, SupervisorError};
    use rvt_core::Mount;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[test]
    fn only_a_plain_stop_with_the_option_on_unmounts() {
        let mut on = Config::default();
        on.global.unmount_on_service_stop = true;
        let off = Config::default();

        let stop = Stop::from_job(Ok::<_, String>(Some("stop".into())));
        let restart = Stop::from_job(Ok::<_, String>(Some("restart".into())));
        let none = Stop::from_job(Ok::<_, String>(None));
        let unreadable = Stop::from_job(Err::<Option<String>, _>("no bus"));
        assert_eq!(stop, Stop::Explicit);
        assert_eq!(restart, Stop::Restart);

        assert!(unmounts(&on, &stop));
        assert!(
            !unmounts(&off, &stop),
            "off is the default and must stay inert"
        );
        assert!(
            !unmounts(&on, &restart),
            "a restart, an upgrade included, must never take mounts down"
        );
        assert!(!unmounts(&on, &none), "a bare signal is not a stop");
        assert!(
            !unmounts(&on, &unreadable),
            "fails closed when systemd cannot say"
        );
        let reload = Stop::from_job(Ok::<_, String>(Some("reload".into())));
        assert!(matches!(reload, Stop::Unclear(_)), "{reload:?}");
        assert!(!unmounts(&on, &reload));
    }

    #[test]
    fn the_budget_leaves_systemd_room_before_it_kills() {
        assert_eq!(
            budget(Some(Duration::from_secs(90))),
            Duration::from_secs(85)
        );
        assert_eq!(budget(Some(Duration::from_secs(6))), Duration::from_secs(3));
        assert_eq!(budget(Some(Duration::from_secs(3600))), MAX_BUDGET);
        assert_eq!(budget(None), DEFAULT_BUDGET);
    }

    /// Answers `state` from a table and records every unmount, with its `force`.
    #[derive(Default)]
    struct Recorder {
        states: BTreeMap<String, MountState>,
        refuse: Option<String>,
        hang: Option<String>,
        /// A mount whose state never comes back, as one behind a wedged mount point.
        wedged: Option<String>,
        calls: Mutex<Vec<String>>,
    }

    impl MountSupervisor for Recorder {
        fn mount<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async { panic!("stopping never mounts") })
        }
        fn unmount<'a>(
            &'a self,
            name: &'a str,
            force: bool,
        ) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("unmount {name} force={force}"));
                if self.hang.as_deref() == Some(name) {
                    std::future::pending::<()>().await;
                }
                if self.refuse.as_deref() == Some(name) {
                    return Err(SupervisorError::Busy {
                        detail: "a file is open".into(),
                    });
                }
                Ok(())
            })
        }
        fn state<'a>(
            &'a self,
            name: &'a str,
        ) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
            Box::pin(async move {
                if self.wedged.as_deref() == Some(name) {
                    std::future::pending::<()>().await;
                }
                Ok(self.states[name].clone())
            })
        }
        fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn mount(name: &str) -> Mount {
        Mount {
            name: name.into(),
            remote: "r".into(),
            path: String::new(),
            mount_point: format!("/mnt/{name}").into(),
            cache_mode: Default::default(),
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

    fn setup(states: &[(&str, MountState)]) -> (Config, Recorder) {
        let config = Config {
            mounts: states.iter().map(|(n, _)| mount(n)).collect(),
            ..Default::default()
        };
        let sup = Recorder {
            states: states
                .iter()
                .map(|(n, s)| (n.to_string(), s.clone()))
                .collect(),
            ..Default::default()
        };
        (config, sup)
    }

    fn no_drain() -> Drain {
        Arc::new(|_| Box::pin(async {}))
    }

    fn sorted(sup: &Recorder) -> Vec<String> {
        let mut c = sup.calls.lock().unwrap().clone();
        c.sort();
        c
    }

    #[tokio::test]
    async fn every_mount_of_ours_that_is_up_is_unmounted_and_none_forced() {
        let (config, sup) = setup(&[
            ("up", MountState::Mounted),
            ("coming", MountState::Mounting),
            ("down", MountState::Unmounted),
            ("theirs", MountState::Foreign),
            ("failed", MountState::Failed { reason: "x".into() }),
        ]);
        let sup = Arc::new(sup);
        unmount_all(sup.clone(), &config, Duration::from_secs(5), no_drain()).await;
        assert_eq!(
            sorted(&sup),
            ["unmount coming force=false", "unmount up force=false"]
        );
    }

    #[tokio::test]
    async fn a_refusal_does_not_strand_the_others() {
        let (config, mut sup) =
            setup(&[("busy", MountState::Mounted), ("idle", MountState::Mounted)]);
        sup.refuse = Some("busy".into());
        let sup = Arc::new(sup);
        unmount_all(sup.clone(), &config, Duration::from_secs(5), no_drain()).await;
        assert_eq!(
            sorted(&sup),
            ["unmount busy force=false", "unmount idle force=false"],
            "refused, and not retried with force"
        );
    }

    #[tokio::test]
    async fn a_mount_that_never_lets_go_cannot_hold_the_stop_past_its_budget() {
        let (config, mut sup) = setup(&[
            ("stuck", MountState::Mounted),
            ("fine", MountState::Mounted),
        ]);
        sup.hang = Some("stuck".into());
        let sup = Arc::new(sup);
        // A drain that never finishes either: both waits are bounded.
        let never: Drain = Arc::new(|_| Box::pin(std::future::pending()));
        let started = std::time::Instant::now();
        unmount_all(sup.clone(), &config, Duration::from_millis(400), never).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            sorted(&sup),
            ["unmount fine force=false", "unmount stuck force=false"],
            "a drain that does not finish still ends in an unmount"
        );
    }

    #[tokio::test]
    async fn deciding_what_to_unmount_is_inside_the_budget_too() {
        let (config, mut sup) = setup(&[("wedged", MountState::Mounted)]);
        sup.wedged = Some("wedged".into());
        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(5),
            unmount_all(
                Arc::new(sup),
                &config,
                Duration::from_millis(300),
                no_drain(),
            ),
        )
        .await
        .expect("a state that never answers held the stop past its budget");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn queued_uploads_are_waited_for_before_the_unmount() {
        let (config, sup) = setup(&[("photos", MountState::Mounted)]);
        let sup = Arc::new(sup);
        let seen = sup.clone();
        let drain: Drain = Arc::new(move |name| {
            let seen = seen.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                seen.calls.lock().unwrap().push(format!("drained {name}"));
            })
        });
        unmount_all(sup.clone(), &config, Duration::from_secs(5), drain).await;
        assert_eq!(
            *sup.calls.lock().unwrap(),
            ["drained photos", "unmount photos force=false"]
        );
    }

    #[tokio::test]
    async fn the_rc_drain_waits_while_rclone_still_has_uploads_queued() {
        use crate::poller::tests::{fixture, rc_list, FakeRc};
        let queued = fixture("vfs-queue-queued-not-uploading.json");
        let mut drained: serde_json::Value = serde_json::from_str(&queued).unwrap();
        drained["queue"] = serde_json::json!([]);

        let fake = FakeRc::new(
            "drain",
            vec![
                ("rc/list", rc_list(&["rc/list", "vfs/queue", "vfs/stats"])),
                ("vfs/queue", queued),
                ("vfs/stats", fixture("vfs-stats-upload-in-progress.json")),
            ],
        )
        .await;
        // `rc_socket_path` names a mount's socket `<name>.sock` in the runtime directory.
        let runtime_dir = fake.socket.parent().unwrap().to_path_buf();
        let waiting = tokio::spawn(rc_drain(runtime_dir)("rc".into()));

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!waiting.is_finished(), "returned with a file still queued");

        fake.set("vfs/queue", drained.to_string());
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("an emptied queue ends the wait")
            .unwrap();
    }
}
