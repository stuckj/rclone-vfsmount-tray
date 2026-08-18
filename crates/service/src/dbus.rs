//! The service's side of the D-Bus contract: what a client can ask for, and nothing else.
//!
//! Answers come from [`Registry`] rather than from a fresh sweep, so a menu never waits on
//! `/proc` and systemd. `Mount` and `Unmount` are the exceptions — they act, and return
//! when the mount is actually up or down.

use crate::registry::{Change, Registry};
use rvt_core::ipc::{self, IpcError, MountView, TransferView};
use rvt_core::supervisor::{MountState, MountSupervisor};
use rvt_core::transfer::TransferState;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;

/// The exported object.
pub struct MountManager {
    sup: Arc<dyn MountSupervisor>,
    registry: Arc<Mutex<Registry>>,
    /// Poked after an action so the watcher publishes its result now rather than at the
    /// next sweep.
    resweep: Arc<Notify>,
    rclone_version: String,
}

impl MountManager {
    pub fn new(
        sup: Arc<dyn MountSupervisor>,
        registry: Arc<Mutex<Registry>>,
        resweep: Arc<Notify>,
        rclone_version: impl Into<String>,
    ) -> Self {
        Self {
            sup,
            registry,
            resweep,
            rclone_version: rclone_version.into(),
        }
    }
}

#[zbus::interface(name = "io.github.stuckj.RcloneVfsmountTray1")]
impl MountManager {
    async fn list_mounts(&self) -> Vec<MountView> {
        self.registry.lock().await.mounts()
    }

    async fn mount(&self, name: &str) -> Result<(), IpcError> {
        let result = self.sup.mount(name).await;
        // Whether it worked or not: a failed attempt leaves a state worth publishing.
        self.resweep.notify_one();
        result.map_err(|e| {
            tracing::warn!(mount = name, error = %e, "mount refused");
            e.into()
        })
    }

    async fn unmount(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        name: &str,
        force: bool,
    ) -> Result<(), IpcError> {
        if force {
            // The one destructive thing this interface can do, and the bus cannot say who
            // asked for it beyond a connection name. Logging it is the whole of the audit
            // trail. See DESIGN.md, "D-Bus, and only for sandboxed callers".
            tracing::warn!(
                mount = name,
                sender = ?header.sender(),
                "forced unmount requested: a write in flight will be severed"
            );
        }
        let result = self.sup.unmount(name, force).await;
        self.resweep.notify_one();
        result.map_err(|e| {
            tracing::warn!(mount = name, error = %e, "unmount refused");
            e.into()
        })
    }

    async fn get_transfer_state(&self, name: &str) -> Result<TransferView, IpcError> {
        let registry = self.registry.lock().await;
        if let Some(view) = registry.transfer(name) {
            return Ok(view.clone());
        }
        let mount = registry
            .mount(name)
            .ok_or_else(|| IpcError::UnknownMount(format!("no mount named {name:?}")))?;
        Ok(TransferView::from(&TransferState::unmonitored(
            name,
            why_not_polled(mount),
        )))
    }

    /// A mount's state changed, or a row appeared.
    #[zbus(signal)]
    async fn mount_state_changed(emitter: &SignalEmitter<'_>, mount: MountView)
        -> zbus::Result<()>;

    /// A row went away.
    #[zbus(signal)]
    async fn mount_removed(emitter: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;

    /// A mount's outstanding work changed.
    #[zbus(signal)]
    async fn transfer_state_changed(
        emitter: &SignalEmitter<'_>,
        state: TransferView,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    async fn interface_version(&self) -> u32 {
        ipc::INTERFACE_VERSION
    }

    #[zbus(property)]
    async fn service_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    #[zbus(property)]
    async fn rclone_version(&self) -> String {
        self.rclone_version.clone()
    }

    #[zbus(property)]
    async fn capability_tier(&self) -> String {
        self.registry
            .lock()
            .await
            .tier()
            .map_or_else(|| "unknown".to_string(), |t| ipc::tier_name(t).to_string())
    }
}

/// Why a mount has no reading, in words a client can show.
///
/// Every one of these is an ordinary state rather than a failure, which is why this is a
/// reason attached to an empty answer and not an error.
///
/// Read back through [`ipc::state_from_name`] rather than matched as strings, so the
/// vocabulary lives in one place and a renamed state cannot quietly fall through to the
/// last arm.
fn why_not_polled(mount: &MountView) -> String {
    match ipc::state_from_name(&mount.state, mount.reason.as_deref()) {
        Some(MountState::Foreign) => {
            "started outside this service, so its rc socket is unknown".into()
        }
        Some(MountState::Orphaned) => "no configuration describes this mount any more".into(),
        Some(MountState::Mounted) => "not polled yet".into(),
        _ => "the mount is not serving".into(),
    }
}

/// Publish one change to whoever is listening.
pub async fn announce(emitter: &SignalEmitter<'_>, change: Change) -> zbus::Result<()> {
    match change {
        Change::Mount(view) => MountManager::mount_state_changed(emitter, view).await,
        Change::Removed(name) => MountManager::mount_removed(emitter, &name).await,
        Change::Transfer(view) => MountManager::transfer_state_changed(emitter, view).await,
        // A property, so it travels as `PropertiesChanged` and has to be emitted through
        // the object server, which holds the instance the new value is read from.
        Change::CapabilityTier => {
            let iface = emitter
                .connection()
                .object_server()
                .interface::<_, MountManager>(emitter.path())
                .await?;
            let published = iface.get().await;
            published
                .capability_tier_changed(iface.signal_emitter())
                .await
        }
    }
}

/// Export the object and take the well-known name.
///
/// `DoNotQueue`, and no `AllowReplacement`: a second service would start mounts against
/// the same config and the same unit names, so the right outcome for one is to fail here
/// and say so, not to wait for a turn it should never get.
pub async fn serve(manager: MountManager) -> anyhow::Result<zbus::Connection> {
    let conn = zbus::connection::Builder::session()?
        .serve_at(ipc::OBJECT_PATH, manager)?
        .build()
        .await?;

    match conn
        .request_name_with_flags(ipc::BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner) => Ok(conn),
        // Under `DoNotQueue` an already-owned name comes back as this error rather than as
        // a reply, so this arm is the ordinary second-instance case, not a bus failure.
        Err(zbus::Error::NameTaken) => anyhow::bail!(
            "another rclone-vfsmount-trayd already owns {} on this session bus",
            ipc::BUS_NAME
        ),
        Ok(reply) => anyhow::bail!("could not take {}: the bus said {reply:?}", ipc::BUS_NAME),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_core::ipc::RcloneVfsmountTrayProxy;
    use rvt_core::supervisor::{BoxFuture, MountState, SupervisorError};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use zbus::object_server::Interface;

    /// Records what it was asked to do, and answers however the test set it up to.
    #[derive(Default)]
    struct FakeSupervisor {
        calls: StdMutex<Vec<String>>,
        refuse_unmount: Option<String>,
    }

    impl MountSupervisor for FakeSupervisor {
        fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            self.calls.lock().unwrap().push(format!("mount {name}"));
            Box::pin(async { Ok(()) })
        }

        fn unmount<'a>(
            &'a self,
            name: &'a str,
            force: bool,
        ) -> BoxFuture<'a, Result<(), SupervisorError>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("unmount {name} force={force}"));
            let refusal = self.refuse_unmount.clone();
            Box::pin(async move {
                match refusal {
                    Some(detail) => Err(SupervisorError::Busy { detail }),
                    None => Ok(()),
                }
            })
        }

        fn state<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
            Box::pin(async { Ok(MountState::Unmounted) })
        }

        fn reconcile(
            &self,
        ) -> BoxFuture<'_, Result<Vec<rvt_core::DiscoveredMount>, SupervisorError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn a_mount(name: &str, state: &str) -> MountView {
        MountView {
            name: name.into(),
            state: state.into(),
            live: state == "mounted",
            managed: true,
            reason: None,
            mount_point: Some(format!("/mnt/{name}")),
            remote: Some(format!("drive:{name}")),
        }
    }

    /// A server and a client joined by a socket pair, so calls cross a real D-Bus
    /// connection with no bus daemon anywhere.
    async fn connected(
        sup: Arc<dyn MountSupervisor>,
        mounts: Vec<MountView>,
    ) -> (zbus::Connection, zbus::Connection, Arc<Mutex<Registry>>) {
        let registry = Arc::new(Mutex::new(Registry::default()));
        registry.lock().await.observe_mounts(mounts);

        let (server_sock, client_sock) = tokio::net::UnixStream::pair().unwrap();
        let guid = zbus::Guid::generate();
        let manager = MountManager::new(
            sup,
            registry.clone(),
            Arc::new(Notify::new()),
            "v1.75.0".to_string(),
        );

        let server = zbus::connection::Builder::socket(server_sock)
            .server(guid)
            .unwrap()
            .p2p()
            .auth_mechanism(zbus::AuthMechanism::Anonymous)
            .serve_at(ipc::OBJECT_PATH, manager)
            .unwrap()
            .build();
        let client = zbus::connection::Builder::socket(client_sock)
            .p2p()
            .auth_mechanism(zbus::AuthMechanism::Anonymous)
            .build();
        let (server, client) = tokio::join!(server, client);
        (server.unwrap(), client.unwrap(), registry)
    }

    async fn proxy(conn: &zbus::Connection) -> RcloneVfsmountTrayProxy<'_> {
        RcloneVfsmountTrayProxy::builder(conn)
            .path(ipc::OBJECT_PATH)
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    /// Two services would start mounts against the same config and the same unit names,
    /// so the second must be told rather than left waiting for a turn.
    ///
    /// Needs a real bus: a peer-to-peer connection keeps no name registry and hands out
    /// every name asked for. Skipped where there is none, as the systemd tests are — and
    /// skipped too where something already owns the name, since then the first `serve`
    /// here is the one that is refused.
    #[tokio::test]
    async fn a_second_service_is_refused_rather_than_left_waiting() {
        let manager = || {
            MountManager::new(
                Arc::new(FakeSupervisor::default()),
                Arc::new(Mutex::new(Registry::default())),
                Arc::new(Notify::new()),
                "v1.75.0".to_string(),
            )
        };

        let Ok(_first) = serve(manager()).await else {
            eprintln!("skipped: no session bus, or the name is already owned");
            return;
        };
        let refused = serve(manager())
            .await
            .expect_err("two services must not both own the mounts");
        assert!(
            refused.to_string().contains(ipc::BUS_NAME),
            "the refusal has to name what is already taken: {refused}"
        );
    }

    #[test]
    fn the_exported_interface_is_the_one_the_contract_names() {
        // The name is a literal in the attribute above because the macro takes no const.
        // A client built from `ipc` would call into nothing if the two ever parted.
        assert_eq!(MountManager::name(), ipc::INTERFACE_NAME);
    }

    #[tokio::test]
    async fn a_client_sees_the_mounts_the_registry_holds() {
        let (_server, client, _reg) = connected(
            Arc::new(FakeSupervisor::default()),
            vec![a_mount("photos", "mounted"), a_mount("docs", "unmounted")],
        )
        .await;

        let got = proxy(&client).await.list_mounts().await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "docs");
        assert_eq!(got[1].mount_point.as_deref(), Some("/mnt/photos"));
        assert!(got[1].live);
    }

    #[tokio::test]
    async fn mounting_reaches_the_supervisor() {
        let sup = Arc::new(FakeSupervisor::default());
        let (_server, client, _reg) =
            connected(sup.clone(), vec![a_mount("photos", "unmounted")]).await;

        proxy(&client).await.mount("photos").await.unwrap();
        assert_eq!(sup.calls.lock().unwrap().as_slice(), ["mount photos"]);
    }

    #[tokio::test]
    async fn force_is_carried_across_rather_than_inferred() {
        let sup = Arc::new(FakeSupervisor::default());
        let (_server, client, _reg) =
            connected(sup.clone(), vec![a_mount("photos", "mounted")]).await;

        let p = proxy(&client).await;
        p.unmount("photos", false).await.unwrap();
        p.unmount("photos", true).await.unwrap();
        assert_eq!(
            sup.calls.lock().unwrap().as_slice(),
            ["unmount photos force=false", "unmount photos force=true"]
        );
    }

    #[tokio::test]
    async fn a_refusal_arrives_as_its_own_error_rather_than_as_prose() {
        let sup = Arc::new(FakeSupervisor {
            refuse_unmount: Some("/mnt/photos is still in use".into()),
            ..Default::default()
        });
        let (_server, client, _reg) = connected(sup, vec![a_mount("photos", "mounted")]).await;

        match proxy(&client).await.unmount("photos", false).await {
            Err(IpcError::Busy(detail)) => assert!(detail.contains("still in use"), "{detail}"),
            other => panic!("expected a Busy refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn asking_about_a_mount_that_does_not_exist_is_an_error_a_client_can_branch_on() {
        let (_server, client, _reg) =
            connected(Arc::new(FakeSupervisor::default()), Vec::new()).await;

        match proxy(&client).await.get_transfer_state("nope").await {
            Err(IpcError::UnknownMount(_)) => {}
            other => panic!("expected UnknownMount, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_mount_with_no_reading_yet_says_so_rather_than_reporting_nothing_outstanding() {
        // An empty answer that claims to be exact is the failure the tier rules exist to
        // prevent: it reads as "nothing left to upload, safe to unmount".
        let (_server, client, _reg) = connected(
            Arc::new(FakeSupervisor::default()),
            vec![a_mount("photos", "mounted")],
        )
        .await;

        let view = proxy(&client)
            .await
            .get_transfer_state("photos")
            .await
            .unwrap();
        assert_eq!(view.fidelity, None);
        assert!(!view.outstanding_known);
        assert!(view.degraded_reason.is_some());
        assert!(!TransferState::from(&view).safe_to_unmount());
    }

    #[tokio::test]
    async fn the_versions_a_client_checks_before_talking_are_readable() {
        let (_server, client, _reg) =
            connected(Arc::new(FakeSupervisor::default()), Vec::new()).await;
        let p = proxy(&client).await;

        assert_eq!(p.interface_version().await.unwrap(), ipc::INTERFACE_VERSION);
        assert_eq!(p.rclone_version().await.unwrap(), "v1.75.0");
        assert_eq!(
            p.service_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            p.capability_tier().await.unwrap(),
            "unknown",
            "no mount has been polled, and a tier nothing stands behind must not be named"
        );
    }

    #[tokio::test]
    async fn a_change_reaches_a_listening_client() {
        use futures_util::StreamExt as _;

        let (server, client, _reg) = connected(
            Arc::new(FakeSupervisor::default()),
            vec![a_mount("photos", "unmounted")],
        )
        .await;

        let mut changes = proxy(&client)
            .await
            .receive_mount_state_changed()
            .await
            .unwrap();
        let emitter = SignalEmitter::new(&server, ipc::OBJECT_PATH).unwrap();
        announce(&emitter, Change::Mount(a_mount("photos", "mounted")))
            .await
            .unwrap();

        // Bounded, because the way this breaks is a signal that never comes — the emitter
        // and the proxy naming different interfaces, say. Waiting for one forever hangs
        // the whole suite instead of reporting the fault.
        let signal = tokio::time::timeout(Duration::from_secs(10), changes.next())
            .await
            .expect("no signal arrived: the emitter and the proxy do not agree")
            .expect("the signal stream ended");
        assert_eq!(signal.args().unwrap().mount.state, "mounted");
    }

    #[tokio::test]
    async fn a_resolved_tier_reaches_a_client_that_already_read_the_property() {
        use futures_util::StreamExt as _;

        // A D-Bus proxy caches a property and refreshes it only from `PropertiesChanged`,
        // so a client that connects before any mount is up — which the service invites by
        // taking its name before the first sweep — would hold "unknown" for the life of
        // the connection unless the change is published.
        let (server, client, registry) =
            connected(Arc::new(FakeSupervisor::default()), Vec::new()).await;

        let p = proxy(&client).await;
        assert_eq!(p.capability_tier().await.unwrap(), "unknown");

        // zbus hands the current value out first and only then waits for changes, so the
        // opening event is the "unknown" the client already has. What is being tested is
        // that a second one ever arrives.
        let mut changed = p.receive_capability_tier_changed().await;
        let opening = tokio::time::timeout(Duration::from_secs(10), changed.next())
            .await
            .expect("the stream did not yield its current value")
            .expect("the stream ended");
        assert_eq!(opening.get().await.unwrap(), "unknown");

        let change = registry.lock().await.note_tier(rvt_core::Tier::T1);
        let emitter = SignalEmitter::new(&server, ipc::OBJECT_PATH).unwrap();
        announce(&emitter, change.expect("the first tier is a change"))
            .await
            .unwrap();

        let update = tokio::time::timeout(Duration::from_secs(10), changed.next())
            .await
            .expect("the property change was never published")
            .expect("the stream ended");
        assert_eq!(update.get().await.unwrap(), "T1");
        assert_eq!(
            p.capability_tier().await.unwrap(),
            "T1",
            "the cached value has to move with it, since that is what a client reads"
        );
    }
}
