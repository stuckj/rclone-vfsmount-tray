//! The service's side of the D-Bus contract: what a client can ask for, and nothing else.
//!
//! Answers come from [`Registry`] rather than from a fresh sweep, so a menu never waits on
//! `/proc` and systemd. `Mount` and `Unmount` are the exceptions — they act, and return
//! when the mount is actually up or down.

use crate::registry::{self, Change, Registry};
use crate::watch::Nudge;
use rvt_core::ipc::{self, IpcError, MountView, TransferView};
use rvt_core::supervisor::{MountSupervisor, SupervisorError};
use rvt_core::Config;
use std::sync::Arc;
use tokio::sync::Mutex;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;

/// The exported object.
pub struct MountManager {
    sup: Arc<dyn MountSupervisor>,
    registry: Arc<Mutex<Registry>>,
    /// Consulted alongside the registry, which holds only what the last *successful*
    /// sweep found.
    config: Arc<Config>,
    /// Poked after an action so the watcher publishes its result now rather than at the
    /// next sweep, and re-probes the mount that was acted on.
    nudge: Arc<Nudge>,
    rclone_version: String,
}

impl MountManager {
    pub fn new(
        sup: Arc<dyn MountSupervisor>,
        registry: Arc<Mutex<Registry>>,
        config: Arc<Config>,
        nudge: Arc<Nudge>,
        rclone_version: impl Into<String>,
    ) -> Self {
        Self {
            sup,
            registry,
            config,
            nudge,
            rclone_version: rclone_version.into(),
        }
    }
}

impl MountManager {
    /// Refuse a name no mount answers to, before anything expensive happens.
    ///
    /// The supervisor reaches the same verdict, but only after sweeping systemd to check
    /// whether the name belongs to an orphan — work an unauthenticated caller should not
    /// be able to buy with a string it made up.
    ///
    /// The config as well as the registry, because the registry holds what the last
    /// *successful* sweep found: one failed call to systemd during start-up would
    /// otherwise leave it empty, and every configured mount would answer "no such mount"
    /// until the next sweep.
    async fn must_know(&self, name: &str) -> Result<(), IpcError> {
        if self.config.mount(name).is_none() && self.registry.lock().await.mount(name).is_none() {
            return Err(IpcError::UnknownMount(format!("no mount named {name:?}")));
        }
        Ok(())
    }
}

#[zbus::interface(name = "io.github.stuckj.RcloneVfsmountTray1")]
impl MountManager {
    async fn list_mounts(&self) -> Vec<MountView> {
        self.registry.lock().await.mounts()
    }

    async fn mount(&self, name: &str) -> Result<(), IpcError> {
        self.must_know(name).await?;
        let result = self.sup.mount(name).await;
        if touched_something(&result) {
            self.nudge.acted_on(name);
        }
        result.map_err(|e| {
            tracing::warn!(mount = name, error = %e, cause = %causes(&e), "mount refused");
            e.into()
        })
    }

    async fn unmount(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        name: &str,
        force: bool,
    ) -> Result<(), IpcError> {
        // After the name is known to be one: this is the audit trail for the single
        // destructive operation here, and an entry saying a write was severed when nothing
        // was even addressed is worse than no entry. It would also let any peer write
        // arbitrary text into the journal. See DESIGN.md, "D-Bus, and only for sandboxed
        // callers".
        self.must_know(name).await?;
        if force {
            tracing::warn!(
                mount = name,
                sender = ?header.sender(),
                "forced unmount requested: a write in flight will be severed"
            );
        }
        let result = self.sup.unmount(name, force).await;
        if touched_something(&result) {
            self.nudge.acted_on(name);
        }
        result.map_err(|e| {
            tracing::warn!(mount = name, error = %e, cause = %causes(&e), "unmount refused");
            e.into()
        })
    }

    async fn get_transfer_state(&self, name: &str) -> Result<TransferView, IpcError> {
        // Through the same gate as `Mount` and `Unmount`: a name those two accept and this
        // one calls unknown is a client dropping a row for a mount it can still act on.
        self.must_know(name).await?;

        let registry = self.registry.lock().await;
        if let Some(view) = registry.transfer(name) {
            return Ok(view.clone());
        }
        // The same empty answer the registry publishes when it drops a reading, so a
        // client that asks and a client that listens cannot be told different things. A
        // mount the registry has not seen yet is described from the config instead.
        Ok(match registry.mount(name) {
            Some(mount) => registry::nothing_to_say(mount),
            None => registry::not_swept_yet(name),
        })
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

/// The `source` chain under an error, as one line.
///
/// `Display` on these stops at the outermost message — that is what crosses D-Bus, since a
/// D-Bus error body is a single string — so whatever said *why* systemd or rclone refused
/// is only ever visible if it is logged here. Empty when there is no cause.
fn causes(e: &SupervisorError) -> String {
    let mut out = String::new();
    let mut source = std::error::Error::source(e);
    while let Some(e) = source {
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// Whether an operation got far enough to leave anything for a sweep to find.
///
/// A sweep is not free — it lists units, reads `/proc`, canonicalises every configured
/// point and shells out to `journalctl` for anything failed — so a refusal that acted on
/// nothing must not force one. Both of these are decided before the supervisor touches a
/// unit, and neither leaves a row whose state has moved.
fn touched_something(result: &Result<(), SupervisorError>) -> bool {
    !matches!(
        result,
        Err(SupervisorError::UnknownMount(_) | SupervisorError::NotManaged(_))
    )
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
    use rvt_core::transfer::TransferState;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use zbus::object_server::Interface;

    /// Records what it was asked to do, and answers however the test set it up to.
    #[derive(Default)]
    struct FakeSupervisor {
        calls: StdMutex<Vec<String>>,
        refuse_unmount: Option<String>,
        refuse_mount: Option<SupervisorError>,
    }

    impl MountSupervisor for FakeSupervisor {
        fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
            self.calls.lock().unwrap().push(format!("mount {name}"));
            let refusal = self.refuse_mount.as_ref().map(|e| e.to_string());
            let unknown = matches!(self.refuse_mount, Some(SupervisorError::UnknownMount(_)));
            Box::pin(async move {
                match refusal {
                    None => Ok(()),
                    Some(text) if unknown => Err(SupervisorError::UnknownMount(text)),
                    Some(text) => Err(SupervisorError::RcloneFailed {
                        reason: text,
                        source: None,
                    }),
                }
            })
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

    /// Built through the real conversion, so `live` and `managed` follow the state.
    fn a_mount(name: &str, state: &str) -> MountView {
        let found = rvt_core::DiscoveredMount::new(
            name,
            rvt_core::ipc::state_from_name(state, None).expect("a state this build knows"),
        );
        MountView {
            mount_point: Some(format!("/mnt/{name}")),
            remote: Some(format!("drive:{name}")),
            ..MountView::from(&found)
        }
    }

    /// A server and a client joined by a socket pair, so calls cross a real D-Bus
    /// connection with no bus daemon anywhere.
    async fn connected(
        sup: Arc<dyn MountSupervisor>,
        mounts: Vec<MountView>,
    ) -> (zbus::Connection, zbus::Connection, Arc<Mutex<Registry>>) {
        connected_with(sup, mounts, Arc::new(Nudge::default())).await
    }

    async fn connected_with_config(
        sup: Arc<dyn MountSupervisor>,
        mounts: Vec<MountView>,
        config: Arc<Config>,
    ) -> (zbus::Connection, zbus::Connection, Arc<Mutex<Registry>>) {
        connected_inner(sup, mounts, Arc::new(Nudge::default()), config).await
    }

    async fn connected_with(
        sup: Arc<dyn MountSupervisor>,
        mounts: Vec<MountView>,
        nudge: Arc<Nudge>,
    ) -> (zbus::Connection, zbus::Connection, Arc<Mutex<Registry>>) {
        connected_inner(sup, mounts, nudge, Arc::new(Config::default())).await
    }

    async fn connected_inner(
        sup: Arc<dyn MountSupervisor>,
        mounts: Vec<MountView>,
        nudge: Arc<Nudge>,
        config: Arc<Config>,
    ) -> (zbus::Connection, zbus::Connection, Arc<Mutex<Registry>>) {
        let registry = Arc::new(Mutex::new(Registry::default()));
        registry.lock().await.observe_mounts(mounts);

        let (server_sock, client_sock) = tokio::net::UnixStream::pair().unwrap();
        let guid = zbus::Guid::generate();
        let manager =
            MountManager::new(sup, registry.clone(), config, nudge, "v1.75.0".to_string());

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
    /// every name asked for. The absence of one is the *only* thing that skips this — a
    /// `serve` that cannot take its name has to fail here, not read as a missing bus, and
    /// this is the one test that covers the service's own start-up path.
    #[tokio::test]
    async fn a_second_service_is_refused_rather_than_left_waiting() {
        if zbus::Connection::session().await.is_err() {
            eprintln!("skipped: no session bus");
            return;
        }

        let manager = || {
            MountManager::new(
                Arc::new(FakeSupervisor::default()),
                Arc::new(Mutex::new(Registry::default())),
                Arc::new(Config::default()),
                Arc::new(Nudge::default()),
                "v1.75.0".to_string(),
            )
        };

        // Something already owning the name — a real service on a developer's machine — is
        // the same refusal under test, so it is asserted on rather than skipped past.
        let _first = match serve(manager()).await {
            Ok(conn) => conn,
            Err(already) => {
                assert!(
                    already.to_string().contains(ipc::BUS_NAME),
                    "serve failed for a reason that is not the name being taken: {already}"
                );
                return;
            }
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

    #[test]
    fn the_reason_a_failure_had_is_recoverable_from_the_log() {
        // Only the outermost message crosses D-Bus, so unless this is logged, why systemd
        // or rclone refused exists nowhere a person can read it.
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let e = SupervisorError::Supervision {
            context: "starting unit".into(),
            source: Some(Box::new(io)),
        };

        assert!(
            !e.to_string().contains("access denied"),
            "if Display carried the cause there would be nothing here to recover"
        );
        assert!(causes(&e).contains("access denied"));
        assert_eq!(
            causes(&SupervisorError::UnknownMount("photos".into())),
            "",
            "an error with no cause must not log an empty field as though it had one"
        );
    }

    #[tokio::test]
    async fn a_configured_mount_is_known_even_before_a_sweep_has_succeeded() {
        // The registry holds what the last *successful* sweep found, so one failed call to
        // systemd at start-up leaves it empty. Telling a client that a mount plainly in its
        // config does not exist is worse than saying the sweep failed.
        let config: rvt_core::Config = toml::from_str(
            "version = 1\n[[mount]]\nname = \"photos\"\nremote = \"drive\"\nmount_point = \"/mnt/photos\"\n",
        )
        .expect("fixture config");

        let sup = Arc::new(FakeSupervisor::default());
        let registry = Arc::new(Mutex::new(Registry::default()));
        let manager = MountManager::new(
            sup.clone(),
            registry,
            Arc::new(config),
            Arc::new(Nudge::default()),
            "v1.75.0".to_string(),
        );

        manager
            .must_know("photos")
            .await
            .expect("a configured mount is not an unknown one");
        assert!(manager.must_know("nope").await.is_err());
    }

    #[tokio::test]
    async fn the_three_methods_agree_about_which_names_exist() {
        // A name Mount accepts and GetTransferState calls unknown is a client dropping a
        // row for a mount it can still act on. The registry is empty here, standing in for
        // a first sweep that has not run or did not succeed.
        let config: rvt_core::Config = toml::from_str(
            "version = 1\n[[mount]]\nname = \"photos\"\nremote = \"drive\"\nmount_point = \"/mnt/photos\"\n",
        )
        .expect("fixture config");

        let (_server, client, _reg) = connected_with_config(
            Arc::new(FakeSupervisor::default()),
            Vec::new(),
            Arc::new(config),
        )
        .await;
        let p = proxy(&client).await;

        p.mount("photos").await.expect("a configured mount");
        let view = p
            .get_transfer_state("photos")
            .await
            .expect("the same name, so the same verdict");
        assert!(
            !view.outstanding_known,
            "nothing has been read, and an exact zero would read as safe to unmount"
        );
        assert!(view.degraded_reason.is_some());

        assert!(p.get_transfer_state("nope").await.is_err());
    }

    #[tokio::test]
    async fn a_call_that_did_nothing_does_not_buy_a_sweep() {
        // A sweep lists units, reads /proc and shells out to journalctl. Any peer on the
        // session bus can call `Mount`, so a refusal must not be a way to spend its CPU —
        // and the init system's — in a loop.
        //
        // The name has to be one the registry holds, or `must_know` refuses it first and
        // the supervisor is never reached. A foreign row is the real shape of that: it is
        // listed, so a client has the name, and `Mount` on it refuses because no config
        // entry says how to start it.
        let sup = Arc::new(FakeSupervisor {
            refuse_mount: Some(SupervisorError::UnknownMount("/mnt/theirs".into())),
            ..Default::default()
        });
        let nudge = Arc::new(Nudge::default());
        let (_server, client, _reg) = connected_with(
            sup.clone(),
            vec![a_mount("/mnt/theirs", "foreign")],
            nudge.clone(),
        )
        .await;

        assert!(proxy(&client).await.mount("/mnt/theirs").await.is_err());
        assert!(
            !sup.calls.lock().unwrap().is_empty(),
            "the supervisor was never asked, so nothing here exercised the refusal"
        );
        assert!(
            nudge.nothing_pending(),
            "a refusal that touched nothing asked for a sweep anyway"
        );

        // A refusal that *did* act still publishes: the mount tried and failed, and that
        // is a state a client has to see.
        let acted = Arc::new(FakeSupervisor {
            refuse_mount: Some(SupervisorError::RcloneFailed {
                reason: "exited 1".into(),
                source: None,
            }),
            ..Default::default()
        });
        let nudge = Arc::new(Nudge::default());
        let (_s2, c2, _r2) =
            connected_with(acted, vec![a_mount("photos", "unmounted")], nudge.clone()).await;

        assert!(proxy(&c2).await.mount("photos").await.is_err());
        assert!(!nudge.nothing_pending());
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
