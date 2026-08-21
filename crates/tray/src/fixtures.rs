//! Trays to render in tests, built from the wire types the service actually sends.
//!
//! Shared by [`crate::model`] and [`crate::menu`], which test the same states from two
//! sides: what the icon derives to, and what the menu says about it.

use std::sync::{Arc, Mutex};

use rvt_core::ipc::{self, IpcError, MountView, TransferView};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::model::{Action, ServiceInfo, TrayModel};

/// A tray and the queue its menu clicks arrive on. Hold the receiver: a dropped one closes
/// the channel, and a dispatch that never happened would look like a click that did nothing.
pub(crate) type Tray = (TrayModel, UnboundedReceiver<Action>);

pub(crate) fn service() -> ServiceInfo {
    ServiceInfo {
        service_version: "0.1.0".into(),
        interface_version: 1,
        rclone_version: "1.75.0".into(),
        capability_tier: "T2".into(),
    }
}

/// A row as the service would publish it.
///
/// Built through the real conversion rather than by filling the fields in. `live` and
/// `managed` are derived from the state, and a fixture that writes them out by hand asks the
/// same question twice: it agrees with the code until one of the two changes, and then it
/// asserts the old answer.
pub(crate) fn mount(name: &str, state: &str) -> MountView {
    let parsed =
        rvt_core::ipc::state_from_name(state, Some("rclone exited before the mount appeared"))
            .unwrap_or_else(|| panic!("{state:?} is not a state this build knows"));
    let found = rvt_core::DiscoveredMount::new(name, parsed).at(format!("/mnt/{name}"));
    MountView {
        // The service fills this from the configuration; `DiscoveredMount` has no remote.
        remote: Some(format!("{name}:")),
        ..MountView::from(&found)
    }
}

/// A clean `vfs/queue` reading with nothing outstanding.
pub(crate) fn idle_transfer(name: &str) -> TransferView {
    TransferView {
        mount: name.into(),
        fidelity: Some("T2".into()),
        outstanding_known: true,
        has_progress: false,
        pending_files: 0,
        pending_known_bytes: 0,
        pending_unknown_size_files: 0,
        uploading: Some(0),
        errored_files: Some(0),
        out_of_space: Some(false),
        rate_bytes_per_sec: None,
        files: Vec::new(),
        degraded_reason: None,
    }
}

/// `files`, `known bytes`, and how many of those files have no size.
pub(crate) fn pending(t: &mut TransferView, files: u64, bytes: u64, unsized_files: u64) {
    t.pending_files = files;
    t.pending_known_bytes = bytes;
    t.pending_unknown_size_files = unsized_files;
}

pub(crate) fn blank() -> Tray {
    let (tx, rx) = mpsc::unbounded_channel();
    (TrayModel::new(tx), rx)
}

pub(crate) fn connected(mounts: Vec<MountView>, transfers: Vec<TransferView>) -> Tray {
    let (mut m, rx) = blank();
    m.go_up(service(), mounts);
    for t in &transfers {
        m.upsert_transfer(t);
    }
    (m, rx)
}

/// Serve one interface over a socket pair and hand back both ends.
///
/// No bus daemon: tier-1 CI has none, and the wire is what is being tested, not the daemon.
/// Keep the server end alive for as long as the client is used — dropping it closes the
/// socket, and every call then fails for a reason the test did not intend.
pub(crate) async fn serve<I>(iface: I) -> (zbus::Connection, zbus::Connection)
where
    I: zbus::object_server::Interface,
{
    let (server_sock, client_sock) = tokio::net::UnixStream::pair().unwrap();
    let guid = zbus::Guid::generate();
    let server = zbus::connection::Builder::socket(server_sock)
        .server(guid)
        .unwrap()
        .p2p()
        .auth_mechanism(zbus::AuthMechanism::Anonymous)
        .serve_at(ipc::OBJECT_PATH, iface)
        .unwrap()
        .build();
    let client = zbus::connection::Builder::socket(client_sock)
        .p2p()
        .auth_mechanism(zbus::AuthMechanism::Anonymous)
        .build();
    let (server, client) = tokio::join!(server, client);
    (server.unwrap(), client.unwrap())
}

/// A service that answers the handshake and writes down every call, arguments included.
pub(crate) struct Recorder {
    pub calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "io.github.stuckj.RcloneVfsmountTray1")]
impl Recorder {
    async fn mount(&self, name: &str) -> Result<(), IpcError> {
        self.calls.lock().unwrap().push(format!("mount {name}"));
        Ok(())
    }

    async fn unmount(&self, name: &str, force: bool) -> Result<(), IpcError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("unmount {name} force={force}"));
        Ok(())
    }

    #[zbus(property)]
    async fn interface_version(&self) -> u32 {
        ipc::INTERFACE_VERSION
    }
}

pub(crate) fn recorder() -> (Recorder, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    (
        Recorder {
            calls: calls.clone(),
        },
        calls,
    )
}
