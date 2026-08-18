//! Keeps [`Registry`] current: sweeps the mount set, polls what is serving, and hands
//! back what a client has not been told.
//!
//! Nothing here talks to D-Bus. [`Watcher::tick`] returns changes and [`Watcher::run`]
//! publishes them, so the decision of *what* changed can be tested without a bus.

use crate::poller::MountPoller;
use crate::registry::{Change, Registry};
use crate::supervisor::rc_socket_path;
use rvt_core::ipc::{MountView, TransferView};
use rvt_core::supervisor::MountSupervisor;
use rvt_core::{Config, RcClient};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

/// How often the mount set is swept when nothing has asked sooner.
///
/// Changes we cause are published immediately — `Mount` and `Unmount` poke the watcher —
/// so this only bounds how long an *external* change goes unnoticed: somebody else's
/// mount appearing, or one of ours dying in a way systemd could not restart.
const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

pub struct Watcher {
    sup: Arc<dyn MountSupervisor>,
    registry: Arc<Mutex<Registry>>,
    config: Arc<Config>,
    /// Where the rc sockets live, for the mounts this service started.
    runtime_dir: PathBuf,
    /// One per mount being polled, kept across ticks: capabilities are probed once and
    /// the rate estimator needs two readings to say anything.
    pollers: HashMap<String, MountPoller>,
    /// When each mount is next worth asking.
    due: HashMap<String, Instant>,
    sweep_due: Instant,
}

impl Watcher {
    pub fn new(
        sup: Arc<dyn MountSupervisor>,
        registry: Arc<Mutex<Registry>>,
        config: Arc<Config>,
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            sup,
            registry,
            config,
            runtime_dir,
            pollers: HashMap::new(),
            due: HashMap::new(),
            sweep_due: Instant::now(),
        }
    }

    /// Sweep and poll whatever is due, and say how long until the next thing is.
    pub async fn tick(&mut self) -> (Vec<Change>, Duration) {
        let now = Instant::now();
        let mut changes = Vec::new();

        if self.sweep_due <= now {
            changes.extend(self.sweep().await);
            // Measured from the end of the sweep, not its start. A systemd slow enough to
            // take longer than the interval would otherwise leave the deadline already
            // past and sweep without pause.
            self.sweep_due = Instant::now() + SWEEP_INTERVAL;
        }
        changes.extend(self.poll_due(now).await);

        let next_poll = self.due.values().min().copied();
        let next = next_poll.map_or(self.sweep_due, |p| p.min(self.sweep_due));
        (changes, next.saturating_duration_since(Instant::now()))
    }

    /// Publish changes as they are found, for as long as the caller lets this run.
    ///
    /// `report` decides what each change is worth in the journal, which is the caller's
    /// judgement rather than this loop's.
    pub async fn run(
        mut self,
        emitter: zbus::object_server::SignalEmitter<'_>,
        resweep: Arc<Notify>,
        report: fn(&Change),
    ) {
        loop {
            let (changes, wait) = self.tick().await;
            for change in changes {
                report(&change);
                if let Err(e) = crate::dbus::announce(&emitter, change).await {
                    tracing::warn!(error = %e, "could not publish a change");
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                // An action of ours has landed; publish its result now.
                _ = resweep.notified() => self.sweep_due = Instant::now(),
            }
        }
    }

    /// Reconcile against reality and fold the result into the registry.
    ///
    /// A sweep that fails leaves the registry alone: one failed call to systemd is not
    /// evidence that every mount went away, and reporting it as such would empty every
    /// client's list.
    async fn sweep(&mut self) -> Vec<Change> {
        let found = match self.sup.reconcile().await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, "could not sweep the mount set; keeping what is known");
                return Vec::new();
            }
        };

        let views: Vec<MountView> = found
            .iter()
            .map(|m| {
                let mut view = MountView::from(m);
                view.remote = self.config.mount(&m.name).map(|c| c.fs_spec());
                view
            })
            .collect();
        self.registry.lock().await.observe_mounts(views)
    }

    async fn poll_due(&mut self, now: Instant) -> Vec<Change> {
        let pollable = self.registry.lock().await.pollable();
        // A mount that stopped serving keeps no poller: a remount is a different rclone
        // with different capabilities, and its rate estimator must not span the gap.
        self.pollers.retain(|name, _| pollable.contains(name));
        self.due.retain(|name, _| pollable.contains(name));

        let mut changes = Vec::new();
        for name in pollable {
            if self.due.get(&name).is_some_and(|due| *due > now) {
                continue;
            }
            if !self.pollers.contains_key(&name) {
                let socket = rc_socket_path(&self.runtime_dir, &name);
                let poller = MountPoller::connect(&name, RcClient::new(&socket)).await;
                self.pollers.insert(name.clone(), poller);
            }

            let poller = self.pollers.get_mut(&name).expect("just inserted");
            let state = poller.poll().await;
            // Only from a probe that answered. An unreachable rclone reports T4 as well,
            // and publishing that as this build's capability is a claim taken from a
            // refusal.
            let tier = poller.rc_answered().then(|| poller.tier());
            let wait = MountPoller::interval(&state, &self.config.global.poll);
            self.due.insert(name.clone(), Instant::now() + wait);

            let mut registry = self.registry.lock().await;
            changes.extend(tier.and_then(|t| registry.note_tier(t)));
            if let Some(change) = registry.observe_transfer(TransferView::from(&state)) {
                changes.push(change);
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_core::supervisor::{BoxFuture, DiscoveredMount, MountState, SupervisorError};
    use rvt_testutil::Scratch;
    use std::sync::Mutex as StdMutex;

    /// Answers `reconcile` from a script, one entry per call, repeating the last.
    struct ScriptedSupervisor {
        sweeps: StdMutex<Vec<Result<Vec<DiscoveredMount>, SupervisorError>>>,
    }

    impl ScriptedSupervisor {
        fn new(sweeps: Vec<Result<Vec<DiscoveredMount>, SupervisorError>>) -> Arc<Self> {
            Arc::new(Self {
                sweeps: StdMutex::new(sweeps),
            })
        }
    }

    impl MountSupervisor for ScriptedSupervisor {
        fn mount<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async { Ok(()) })
        }
        fn unmount<'a>(
            &'a self,
            _name: &'a str,
            _force: bool,
        ) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async { Ok(()) })
        }
        fn state<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
            Box::pin(async { Ok(MountState::Unmounted) })
        }
        fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
            let mut sweeps = self.sweeps.lock().unwrap();
            let next = if sweeps.len() > 1 {
                sweeps.remove(0)
            } else {
                sweeps
                    .first()
                    .map(|r| match r {
                        Ok(found) => Ok(found.clone()),
                        Err(e) => Err(SupervisorError::Supervision {
                            context: e.to_string(),
                            source: None,
                        }),
                    })
                    .unwrap_or_else(|| Ok(Vec::new()))
            };
            Box::pin(async move { next })
        }
    }

    /// Built from the TOML a user would write, and validated, so a fixture cannot describe
    /// a configuration the service would refuse to load.
    fn config_with(names: &[&str]) -> Arc<Config> {
        let mounts = names
            .iter()
            .map(|n| {
                format!(
                    "[[mount]]\nname = \"{n}\"\nremote = \"drive\"\nmount_point = \"/mnt/{n}\"\n"
                )
            })
            .collect::<String>();
        let config: Config =
            toml::from_str(&format!("version = 1\n{mounts}")).expect("fixture config");
        config.validate().expect("fixture config is not valid");
        Arc::new(config)
    }

    /// The same, with a cadence slower than [`SWEEP_INTERVAL`] — which a user with a
    /// rarely-touched mount would set, and which is the only case where the two deadlines
    /// disagree about which comes first.
    fn config_polling_slowly(names: &[&str]) -> Arc<Config> {
        let mut config = Config::clone(&config_with(names));
        config.global.poll.idle_secs = 600;
        config.validate().expect("fixture config is not valid");
        Arc::new(config)
    }

    fn watcher(
        sup: Arc<dyn MountSupervisor>,
        config: Arc<Config>,
        runtime_dir: PathBuf,
    ) -> (Watcher, Arc<Mutex<Registry>>) {
        let registry = Arc::new(Mutex::new(Registry::default()));
        (
            Watcher::new(sup, registry.clone(), config, runtime_dir),
            registry,
        )
    }

    #[tokio::test]
    async fn a_sweep_fills_the_registry_and_names_the_remote() {
        let scratch = Scratch::new("watch-first-sweep");
        let sup = ScriptedSupervisor::new(vec![Ok(vec![DiscoveredMount::new(
            "photos",
            MountState::Unmounted,
        )
        .at("/mnt/photos")])]);
        let (mut w, registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        let (changes, _) = w.tick().await;
        assert_eq!(changes.len(), 1);

        let mounts = registry.lock().await.mounts();
        assert_eq!(mounts[0].remote.as_deref(), Some("drive:"));
        assert_eq!(mounts[0].mount_point.as_deref(), Some("/mnt/photos"));
    }

    #[tokio::test]
    async fn a_second_sweep_with_nothing_new_says_nothing() {
        let scratch = Scratch::new("watch-quiet-sweep");
        let sup = ScriptedSupervisor::new(vec![Ok(vec![DiscoveredMount::new(
            "photos",
            MountState::Unmounted,
        )])]);
        let (mut w, _registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        w.tick().await;
        w.sweep_due = Instant::now();
        assert!(w.tick().await.0.is_empty());
    }

    #[tokio::test]
    async fn a_failed_sweep_keeps_what_was_known() {
        // One unanswered call to systemd is not evidence every mount went away, and
        // publishing it as one would empty every client's list.
        let scratch = Scratch::new("watch-failed-sweep");
        let sup = ScriptedSupervisor::new(vec![
            Ok(vec![DiscoveredMount::new("photos", MountState::Mounted)]),
            Err(SupervisorError::Supervision {
                context: "systemd is not answering".into(),
                source: None,
            }),
        ]);
        let (mut w, registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        w.tick().await;
        w.sweep_due = Instant::now();
        let (changes, _) = w.tick().await;

        assert!(changes.is_empty(), "a failed sweep must announce nothing");
        assert_eq!(registry.lock().await.mounts().len(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_rclone_is_reported_rather_than_left_blank() {
        // The socket does not exist, which is what a mount whose rclone has died looks
        // like. The mount must stay on screen with a reason, not vanish or read as idle.
        let scratch = Scratch::new("watch-dead-rclone");
        let sup = ScriptedSupervisor::new(vec![Ok(vec![DiscoveredMount::new(
            "photos",
            MountState::Mounted,
        )])]);
        let (mut w, registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        let (changes, _) = w.tick().await;

        assert!(
            changes.iter().any(|c| matches!(c, Change::Transfer(_))),
            "a mount that cannot be reached still has something to say"
        );
        let view = registry.lock().await.transfer("photos").cloned().unwrap();
        assert_eq!(view.fidelity, None);
        assert!(!view.outstanding_known);
        assert!(view.degraded_reason.is_some());
    }

    #[tokio::test]
    async fn nothing_that_is_not_serving_is_polled() {
        let scratch = Scratch::new("watch-nothing-to-poll");
        let sup = ScriptedSupervisor::new(vec![Ok(vec![DiscoveredMount::new(
            "photos",
            MountState::Unmounted,
        )])]);
        let (mut w, _registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        let (changes, _) = w.tick().await;
        assert!(!changes.iter().any(|c| matches!(c, Change::Transfer(_))));
        assert!(w.pollers.is_empty());
    }

    #[tokio::test]
    async fn a_mount_that_goes_down_loses_its_poller() {
        // A remount is a different rclone on a different socket, so its capabilities are
        // re-probed and its rate estimator must not span the gap.
        let scratch = Scratch::new("watch-poller-lifetime");
        let sup = ScriptedSupervisor::new(vec![
            Ok(vec![DiscoveredMount::new("photos", MountState::Mounted)]),
            Ok(vec![DiscoveredMount::new("photos", MountState::Unmounted)]),
        ]);
        let (mut w, _registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        w.tick().await;
        assert_eq!(w.pollers.len(), 1);

        w.sweep_due = Instant::now();
        w.tick().await;
        assert!(w.pollers.is_empty());
    }

    #[tokio::test]
    async fn the_wait_never_outruns_the_next_sweep() {
        // A mount whose own cadence is 600s, so its poll deadline is far past the sweep's
        // and the sweep is what the wait has to be taken from. With nothing to poll this
        // holds trivially and proves nothing.
        let scratch = Scratch::new("watch-wait");
        let sup = ScriptedSupervisor::new(vec![Ok(vec![DiscoveredMount::new(
            "photos",
            MountState::Mounted,
        )])]);
        let (mut w, _registry) = watcher(
            sup,
            config_polling_slowly(&["photos"]),
            scratch.path().to_path_buf(),
        );

        let (_, wait) = w.tick().await;
        assert!(
            !w.due.is_empty(),
            "nothing was polled, so nothing is proved"
        );
        assert!(wait <= SWEEP_INTERVAL, "{wait:?}");
    }

    #[tokio::test]
    async fn a_tier_is_not_claimed_from_a_probe_that_was_refused() {
        // The socket does not exist, so the capability set is empty and `tier()` falls
        // back to T4 — the disk scan, which needs nothing. Publishing that as what this
        // rclone supports would be a capability read off a refusal.
        let scratch = Scratch::new("watch-refused-probe");
        let sup = ScriptedSupervisor::new(vec![Ok(vec![DiscoveredMount::new(
            "photos",
            MountState::Mounted,
        )])]);
        let (mut w, registry) =
            watcher(sup, config_with(&["photos"]), scratch.path().to_path_buf());

        let (changes, _) = w.tick().await;

        assert_eq!(registry.lock().await.tier(), None);
        assert!(!changes.iter().any(|c| matches!(c, Change::CapabilityTier)));
    }
}
