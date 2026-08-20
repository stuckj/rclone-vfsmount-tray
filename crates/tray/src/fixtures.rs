//! Trays to render in tests, built from the wire types the service actually sends.
//!
//! Shared by [`crate::model`] and [`crate::menu`], which test the same states from two
//! sides: what the icon derives to, and what the menu says about it.

use rvt_core::ipc::{MountView, TransferView};
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

/// `live` and `managed` follow the state, exactly as `MountView::from` derives them.
pub(crate) fn mount(name: &str, state: &str) -> MountView {
    MountView {
        name: name.into(),
        state: state.into(),
        live: matches!(state, "mounted" | "foreign" | "orphaned"),
        managed: state != "foreign",
        reason: None,
        mount_point: Some(format!("/mnt/{name}")),
        remote: Some(format!("{name}:")),
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
