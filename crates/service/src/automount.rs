//! Bringing up the mounts marked `auto_mount` when the service starts.
//!
//! Runs beside the bus rather than before it: a mount takes tens of seconds, and a client
//! should be able to watch it come up. A mount already serving is left exactly as it is —
//! adopting it is what a restart means — and so is anything this service did not start.
//!
//! A failure that may be transient is retried with backoff, so a remote that is down at
//! login does not leave its mount down until someone notices. The retrying belongs to the
//! start-up intent only: once a client mounts or unmounts a name, the decision is theirs
//! for the rest of this process's life. Nothing records it, so the next start brings the
//! mount up again.

use crate::watch::Nudge;
use rvt_core::supervisor::{MountState, MountSupervisor, SupervisorError};
use rvt_core::Config;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// The first wait before retrying, doubled after each failure up to [`RETRY_CAP`].
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_CAP: Duration = Duration::from_secs(15 * 60);

/// Which configured mounts are still wanted up, and the lock that makes deciding to mount
/// one and mounting it a single step.
///
/// The lock is held across the whole attempt, so a client acting on the name waits for
/// an attempt in flight rather than racing it: an `Unmount` landing between the check and
/// the attempt would otherwise be undone by a mount nobody asked for.
pub struct AutoMount {
    wanted: BTreeMap<String, Mutex<bool>>,
    retry_first: Duration,
    retry_cap: Duration,
}

impl AutoMount {
    pub fn new(config: &Config) -> Self {
        Self {
            wanted: config
                .mounts
                .iter()
                .filter(|m| m.auto_mount)
                .map(|m| (m.name.clone(), Mutex::new(true)))
                .collect(),
            retry_first: RETRY_FIRST,
            retry_cap: RETRY_CAP,
        }
    }

    #[cfg(test)]
    fn with_retry(mut self, first: Duration, cap: Duration) -> Self {
        self.retry_first = first;
        self.retry_cap = cap;
        self
    }

    #[cfg(test)]
    pub async fn is_wanted(&self, name: &str) -> bool {
        match self.wanted.get(name) {
            Some(w) => *w.lock().await,
            None => false,
        }
    }

    /// A client has acted on this mount, so it is no longer this module's to bring up.
    pub async fn release(&self, name: &str) {
        if let Some(wanted) = self.wanted.get(name) {
            *wanted.lock().await = false;
        }
    }

    /// Bring every wanted mount up, concurrently, retrying each until it is up, a
    /// client takes it over, or it fails in a way retrying cannot fix.
    pub async fn run(self: Arc<Self>, sup: Arc<dyn MountSupervisor>, nudge: Arc<Nudge>) {
        let mut tasks = tokio::task::JoinSet::new();
        for name in self.wanted.keys() {
            let (this, sup, nudge, name) = (self.clone(), sup.clone(), nudge.clone(), name.clone());
            tasks.spawn(async move { this.bring_up(&name, &*sup, &nudge).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn bring_up(&self, name: &str, sup: &dyn MountSupervisor, nudge: &Nudge) {
        let Some(wanted) = self.wanted.get(name) else {
            return;
        };
        let mut wait = self.retry_first;
        loop {
            {
                let mut still_wanted = wanted.lock().await;
                if !*still_wanted {
                    return;
                }
                match self.attempt(name, sup, nudge).await {
                    Attempt::Done => {
                        *still_wanted = false;
                        return;
                    }
                    Attempt::Retry(e) => tracing::warn!(
                        mount = name,
                        error = %e,
                        retry_in_secs = wait.as_secs(),
                        "auto-mount failed; will try again"
                    ),
                    Attempt::Settling => tracing::debug!(
                        mount = name,
                        "auto-mount: on its way up; looking again later"
                    ),
                }
            }
            tokio::time::sleep(wait).await;
            wait = next_wait(wait, self.retry_cap);
        }
    }

    async fn attempt(&self, name: &str, sup: &dyn MountSupervisor, nudge: &Nudge) -> Attempt {
        match sup.state(name).await {
            // Down, or left failed by an rclone that died: both are a mount to bring up.
            Ok(MountState::Unmounted | MountState::Failed { .. }) => {}
            // Coming up — or systemd between restarts, which can still end in `Failed`.
            Ok(MountState::Mounting) => return Attempt::Settling,
            // Serving, on its way down, or somebody else's: not ours to touch.
            Ok(state) => {
                tracing::debug!(mount = name, state = ?state, "auto-mount: leaving as found");
                return Attempt::Done;
            }
            Err(e) => return Attempt::Retry(e),
        }

        tracing::info!(mount = name, "auto-mounting");
        let result = sup.mount(name).await;
        if !matches!(
            result,
            Err(SupervisorError::UnknownMount(_) | SupervisorError::NotManaged(_))
        ) {
            nudge.acted_on(name);
        }
        match result {
            Ok(()) => {
                tracing::info!(mount = name, "auto-mounted");
                Attempt::Done
            }
            Err(e) if retryable(&e) => Attempt::Retry(e),
            Err(e) => {
                tracing::warn!(mount = name, error = %e, "auto-mount failed; not retrying");
                Attempt::Done
            }
        }
    }
}

fn next_wait(wait: Duration, cap: Duration) -> Duration {
    (wait * 2).min(cap)
}

enum Attempt {
    /// Up, left as found, or failed for good. Nothing more to do either way.
    Done,
    Retry(SupervisorError),
    /// Not settled yet; look again after the next wait.
    Settling,
}

/// Whether trying again later could succeed without anyone changing anything.
///
/// rclone failing to come up is the remote-down-at-login case, and a systemd call failing
/// is a manager that was busy. The rest need a person: a bad mount point, a path somebody
/// else holds, a name that does not exist.
fn retryable(e: &SupervisorError) -> bool {
    matches!(
        e,
        SupervisorError::RcloneFailed { .. } | SupervisorError::Supervision { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_core::supervisor::{BoxFuture, DiscoveredMount};
    use rvt_core::Mount;
    use std::sync::Mutex as StdMutex;

    /// Reports a fixed state per name and records every call to `mount`. Each mount call
    /// takes the next result from `results`, defaulting to success, and success makes the
    /// mount report `Mounted` from then on.
    #[derive(Default)]
    struct Recorder {
        states: StdMutex<BTreeMap<String, MountState>>,
        results: StdMutex<Vec<Result<(), SupervisorError>>>,
        mounted: StdMutex<Vec<String>>,
        unmounted: StdMutex<Vec<String>>,
    }

    impl MountSupervisor for Recorder {
        fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.mounted.lock().unwrap().push(name.to_string());
                let mut results = self.results.lock().unwrap();
                let r = if results.is_empty() {
                    Ok(())
                } else {
                    results.remove(0)
                };
                let state = match &r {
                    Ok(()) => MountState::Mounted,
                    Err(e) => MountState::Failed {
                        reason: e.to_string(),
                    },
                };
                self.states.lock().unwrap().insert(name.to_string(), state);
                r
            })
        }
        fn unmount<'a>(
            &'a self,
            name: &'a str,
            _force: bool,
        ) -> BoxFuture<'a, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.unmounted.lock().unwrap().push(name.to_string());
                Ok(())
            })
        }
        fn state<'a>(
            &'a self,
            name: &'a str,
        ) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
            Box::pin(async move {
                Ok(self
                    .states
                    .lock()
                    .unwrap()
                    .get(name)
                    .cloned()
                    .unwrap_or(MountState::Unmounted))
            })
        }
        fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn mount(name: &str, auto_mount: bool) -> Mount {
        Mount {
            name: name.into(),
            remote: "r".into(),
            path: String::new(),
            mount_point: format!("/mnt/{name}").into(),
            cache_mode: Default::default(),
            cache_max_size: None,
            cache_max_age: None,
            auto_mount,
            read_only: false,
            allow_other: false,
            uid: None,
            gid: None,
            umask: None,
            extra_args: Vec::new(),
        }
    }

    fn config(mounts: Vec<Mount>) -> Config {
        Config {
            mounts,
            ..Default::default()
        }
    }

    fn rclone_failed() -> SupervisorError {
        SupervisorError::RcloneFailed {
            reason: "couldn't connect".into(),
            source: None,
        }
    }

    async fn run(auto: AutoMount, sup: &Arc<Recorder>) {
        Arc::new(auto)
            .run(sup.clone(), Arc::new(Nudge::default()))
            .await;
    }

    #[tokio::test]
    async fn only_down_mounts_marked_auto_mount_are_brought_up() {
        let c = config(vec![
            mount("down", true),
            mount("failed", true),
            mount("adopted", true),
            mount("going", true),
            mount("theirs", true),
            mount("manual", false),
        ]);
        let sup = Arc::new(Recorder::default());
        sup.states.lock().unwrap().extend([
            (
                "failed".to_string(),
                MountState::Failed {
                    reason: "rclone exited".into(),
                },
            ),
            ("adopted".to_string(), MountState::Mounted),
            ("going".to_string(), MountState::Unmounting),
            ("theirs".to_string(), MountState::Foreign),
        ]);

        run(AutoMount::new(&c), &sup).await;

        let mut mounted = sup.mounted.lock().unwrap().clone();
        mounted.sort();
        assert_eq!(
            mounted,
            ["down", "failed"],
            "an adopted, stopping or foreign mount must not be touched, nor one that is \
             not marked auto_mount"
        );
        assert!(sup.unmounted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_mount_still_coming_up_is_looked_at_again_rather_than_given_up_on() {
        // systemd between restarts reads as `Mounting`, and can still end in `Failed`.
        let c = config(vec![mount("coming", true)]);
        let sup = Arc::new(Recorder::default());
        sup.states
            .lock()
            .unwrap()
            .insert("coming".into(), MountState::Mounting);
        let auto =
            AutoMount::new(&c).with_retry(Duration::from_millis(5), Duration::from_millis(5));
        let running = tokio::spawn(Arc::new(auto).run(sup.clone(), Arc::new(Nudge::default())));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            sup.mounted.lock().unwrap().is_empty(),
            "left alone while coming up"
        );
        sup.states.lock().unwrap().insert(
            "coming".into(),
            MountState::Failed {
                reason: "start limit hit".into(),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("brought up once it failed")
            .unwrap();
        assert_eq!(*sup.mounted.lock().unwrap(), ["coming"]);
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_comes_up() {
        let c = config(vec![mount("flaky", true)]);
        let sup = Arc::new(Recorder::default());
        *sup.results.lock().unwrap() = vec![Err(rclone_failed()), Err(rclone_failed())];

        let auto =
            AutoMount::new(&c).with_retry(Duration::from_millis(1), Duration::from_millis(4));
        run(auto, &sup).await;

        assert_eq!(
            sup.mounted.lock().unwrap().len(),
            3,
            "two failures, then up"
        );
    }

    #[test]
    fn retries_back_off_from_thirty_seconds_to_every_fifteen_minutes() {
        // What config.example.toml promises about `auto_mount`.
        let mut wait = RETRY_FIRST;
        let mut seen = vec![wait.as_secs()];
        for _ in 0..7 {
            wait = next_wait(wait, RETRY_CAP);
            seen.push(wait.as_secs());
        }
        assert_eq!(seen, [30, 60, 120, 240, 480, 900, 900, 900]);
    }

    #[tokio::test]
    async fn an_auto_mount_is_published_as_promptly_as_a_clients() {
        // The watcher only learns a mount cycled when told; otherwise the new state waits
        // for the next sweep and the poller spans two rclone processes.
        // A refusal decided before anything was touched is not worth a sweep, as for a
        // client's (see `dbus::touched_something`).
        for (result, nudged) in [
            (Ok(()), true),
            (Err(SupervisorError::NotManaged("held".into())), false),
        ] {
            let c = config(vec![mount("one", true)]);
            let sup = Arc::new(Recorder::default());
            *sup.results.lock().unwrap() = vec![result];
            let nudge = Arc::new(Nudge::default());
            Arc::new(AutoMount::new(&c))
                .run(sup.clone(), nudge.clone())
                .await;
            assert_eq!(!nudge.nothing_pending(), nudged);
        }
    }

    #[tokio::test]
    async fn a_failure_retrying_cannot_fix_is_not_retried() {
        let c = config(vec![mount("blocked", true)]);
        let sup = Arc::new(Recorder::default());
        *sup.results.lock().unwrap() = vec![Err(SupervisorError::NotManaged(
            "held by something else".into(),
        ))];

        let auto =
            AutoMount::new(&c).with_retry(Duration::from_millis(1), Duration::from_millis(1));
        run(auto, &sup).await;

        assert_eq!(sup.mounted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_client_acting_on_a_mount_stops_the_retries() {
        let c = config(vec![mount("flaky", true)]);
        let sup = Arc::new(Recorder::default());
        *sup.results.lock().unwrap() = (0..1000).map(|_| Err(rclone_failed())).collect();

        let auto = Arc::new(
            AutoMount::new(&c).with_retry(Duration::from_millis(20), Duration::from_millis(20)),
        );
        let running = tokio::spawn(auto.clone().run(sup.clone(), Arc::new(Nudge::default())));
        tokio::time::sleep(Duration::from_millis(50)).await;
        auto.release("flaky").await;
        let after_release = sup.mounted.lock().unwrap().len();

        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("releasing the only wanted mount ends the run")
            .unwrap();
        assert_eq!(
            sup.mounted.lock().unwrap().len(),
            after_release,
            "no attempt may follow a client's own action on the mount"
        );
    }
}
