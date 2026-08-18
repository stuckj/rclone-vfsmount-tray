//! The D-Bus contract between the service and its clients.
//!
//! Both sides are built from here: the service implements the interface, clients drive
//! [`RcloneVfsmountTrayProxy`], and the types below are the only description of what
//! crosses. A payload changed on one side alone is a compile error rather than a decode
//! failure at run time.
//!
//! **Payloads are dictionaries, not structs.** `a{sv}` lets a service add a key an older
//! client ignores, and lets a newer client find one absent; a struct signature makes
//! either of those a decode error, which would tie every client to the service's exact
//! version. [`INTERFACE_VERSION`] therefore announces additive revisions, and the `1` in
//! [`INTERFACE_NAME`] changes only when something is removed or reinterpreted.
//!
//! **Subscribe, then list, then apply what arrived in between.** Signals are deltas and
//! the service sends one only when something changes, so a client that lists first and
//! subscribes afterwards can miss the change that happened in the gap and never hear of it
//! again. Subscribing first and applying the snapshot *before* the buffered signals gets
//! the ordering right in both directions: a signal older than the snapshot re-applies what
//! the snapshot already has, and a newer one lands on top of it.
//!
//! What is *not* here is as deliberate: no method takes an rc command, a path outside a
//! configured mount, or anything from rclone's own configuration. See DESIGN.md,
//! "D-Bus, and only for sandboxed callers".

use crate::capabilities::Tier;
use crate::models::Pending;
use crate::supervisor::{DiscoveredMount, MountState, SupervisorError};
use crate::transfer::{TransferFile, TransferState};
use zbus::zvariant::{DeserializeDict, SerializeDict, Type};

/// Well-known name the service takes on the session bus.
pub const BUS_NAME: &str = "io.github.stuckj.RcloneVfsmountTray";

/// The single object the service exports.
pub const OBJECT_PATH: &str = "/io/github/stuckj/RcloneVfsmountTray";

/// The interface that object carries.
pub const INTERFACE_NAME: &str = "io.github.stuckj.RcloneVfsmountTray1";

/// Revision of [`INTERFACE_NAME`], readable as the `InterfaceVersion` property.
///
/// Incremented when a method, signal, property or dictionary key is added, so a client
/// can tell whether one it wants exists without calling it to find out. A client older
/// than the service still works; one that is newer must degrade rather than fail (#52).
pub const INTERFACE_VERSION: u32 = 1;

/// What a method call failed with.
///
/// Each variant is a distinct D-Bus error name, so a client branches on the failure
/// rather than matching on prose. The string is the sentence to show; the numbers behind
/// [`Self::PendingUploads`] come from [`RcloneVfsmountTrayProxy::get_transfer_state`],
/// because a D-Bus error body carries one string and nothing else.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.stuckj.RcloneVfsmountTray1.Error")]
#[non_exhaustive]
pub enum IpcError {
    /// Transport, not application: the call never reached a method body.
    #[zbus(error)]
    ZBus(zbus::Error),
    /// No configured mount and no unit of ours answers to that name.
    UnknownMount(String),
    /// The mount point is missing, not a directory, or not writable.
    BadMountPoint(String),
    /// rclone could not be started, or exited during start-up.
    RcloneFailed(String),
    /// The unmount was not done: the write-back cache still holds unuploaded data.
    ///
    /// **Nothing raises this yet.** The check is #19, and until it lands an unmount is not
    /// weighed against what is still queued. When it does, a client is to present it as a
    /// choice — wait, unmount anyway, cancel — and not as a wall: the data is on disk and
    /// resumes on remount, so the cost is delay, not loss.
    PendingUploads(String),
    /// The mount point could not be released — something is still using it.
    Busy(String),
    /// Talking to the init system failed.
    Supervision(String),
    /// A mount we did not start, which this service will not act on.
    NotManaged(String),
}

impl From<SupervisorError> for IpcError {
    fn from(e: SupervisorError) -> Self {
        // Rendered here rather than passed through, because `SupervisorError`'s `source`
        // chain does not fit in a D-Bus error and the service logs it in full anyway.
        let text = e.to_string();
        match e {
            SupervisorError::UnknownMount(_) => IpcError::UnknownMount(text),
            SupervisorError::BadMountPoint { .. } => IpcError::BadMountPoint(text),
            SupervisorError::RcloneFailed { .. } => IpcError::RcloneFailed(text),
            SupervisorError::PendingUploads(_) => IpcError::PendingUploads(text),
            SupervisorError::Busy { .. } => IpcError::Busy(text),
            SupervisorError::Supervision { .. } => IpcError::Supervision(text),
            SupervisorError::NotManaged(_) => IpcError::NotManaged(text),
            // No catch-all: `#[non_exhaustive]` binds outside this crate, not in it, so a
            // new supervisor error stops the build here until it is given a name of its
            // own rather than silently reaching clients as a generic failure.
        }
    }
}

/// One mount, as a client sees it.
///
/// `live` and `managed` travel alongside `state` so a client meeting a state name it does
/// not know still knows whether anything is mounted and whether this service owns it.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}", rename_all = "PascalCase")]
pub struct MountView {
    /// Configured name, or the mount point for one we did not start.
    pub name: String,
    /// One of [`state_name`]'s vocabulary.
    pub state: String,
    /// Whether the point is serving, however it got there.
    pub live: bool,
    /// Whether this service owns the mount and may act on it.
    pub managed: bool,
    /// Why the mount failed. Present only with `state` `"failed"`.
    pub reason: Option<String>,
    /// Where it is mounted, or configured to be. Absent when nothing recorded one.
    pub mount_point: Option<String>,
    /// `remote:path`, for a mount the config describes.
    pub remote: Option<String>,
}

impl From<&DiscoveredMount> for MountView {
    fn from(m: &DiscoveredMount) -> Self {
        Self {
            name: m.name.clone(),
            state: state_name(&m.state).to_string(),
            live: m.state.is_live(),
            managed: m.state.is_managed(),
            reason: match &m.state {
                MountState::Failed { reason } => Some(reason.clone()),
                _ => None,
            },
            mount_point: m
                .mount_point
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            remote: None,
        }
    }
}

/// The `State` key's vocabulary.
///
/// Spelled out rather than derived from the variant names: these strings are the wire, and
/// renaming a Rust variant must not silently rename a D-Bus value.
pub fn state_name(state: &MountState) -> &'static str {
    match state {
        MountState::Unmounted => "unmounted",
        MountState::Mounting => "mounting",
        MountState::Mounted => "mounted",
        MountState::Unmounting => "unmounting",
        MountState::Failed { .. } => "failed",
        MountState::Foreign => "foreign",
        MountState::Orphaned => "orphaned",
    }
}

/// Read a `State` back, for a client that wants [`MountState`]'s own predicates.
///
/// `None` for a name this build does not know, which is the case
/// [`MountView::live`] and [`MountView::managed`] exist to cover.
pub fn state_from_name(name: &str, reason: Option<&str>) -> Option<MountState> {
    Some(match name {
        "unmounted" => MountState::Unmounted,
        "mounting" => MountState::Mounting,
        "mounted" => MountState::Mounted,
        "unmounting" => MountState::Unmounting,
        "failed" => MountState::Failed {
            reason: reason.unwrap_or_default().to_string(),
        },
        "foreign" => MountState::Foreign,
        "orphaned" => MountState::Orphaned,
        _ => return None,
    })
}

/// One outstanding file.
///
/// Every field but `name` is optional, and absent means *this tier cannot say* rather
/// than zero — the same rule as [`TransferFile`], which it crosses the bus as.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}", rename_all = "PascalCase")]
pub struct TransferFileView {
    pub name: String,
    pub size: Option<u64>,
    pub in_flight: Option<bool>,
    pub tries: Option<u64>,
    pub bytes_sent: Option<u64>,
}

impl From<&TransferFile> for TransferFileView {
    fn from(f: &TransferFile) -> Self {
        Self {
            name: f.name.clone(),
            size: f.size,
            in_flight: f.in_flight,
            tries: f.tries,
            bytes_sent: f.bytes_sent,
        }
    }
}

impl From<&TransferFileView> for TransferFile {
    fn from(f: &TransferFileView) -> Self {
        Self {
            name: f.name.clone(),
            size: f.size,
            in_flight: f.in_flight,
            tries: f.tries,
            bytes_sent: f.bytes_sent,
        }
    }
}

/// What is outstanding for one mount.
///
/// [`TransferState`] flattened: `Pending`'s three numbers become three keys, and
/// `fidelity` becomes [`tier_name`]'s spelling. `Fidelity` absent means no source
/// produced a total, which is not the same as a total of zero.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}", rename_all = "PascalCase")]
pub struct TransferView {
    pub mount: String,
    pub fidelity: Option<String>,
    pub outstanding_known: bool,
    pub has_progress: bool,
    pub pending_files: u64,
    pub pending_known_bytes: u64,
    pub pending_unknown_size_files: u64,
    pub uploading: Option<u64>,
    pub errored_files: Option<u64>,
    pub out_of_space: Option<bool>,
    pub rate_bytes_per_sec: Option<u64>,
    pub files: Vec<TransferFileView>,
    pub degraded_reason: Option<String>,
}

impl From<&TransferState> for TransferView {
    fn from(s: &TransferState) -> Self {
        Self {
            mount: s.mount.clone(),
            fidelity: s.fidelity.map(|t| tier_name(t).to_string()),
            outstanding_known: s.outstanding_known,
            has_progress: s.has_progress,
            pending_files: s.pending.files,
            pending_known_bytes: s.pending.known_bytes,
            pending_unknown_size_files: s.pending.unknown_size_files,
            uploading: s.uploading,
            errored_files: s.errored_files,
            out_of_space: s.out_of_space,
            rate_bytes_per_sec: s.rate_bytes_per_sec,
            files: s.files.iter().map(TransferFileView::from).collect(),
            degraded_reason: s.degraded_reason.clone(),
        }
    }
}

impl From<&TransferView> for TransferState {
    /// An unknown `Fidelity` reads as absent, which is the honest floor: a client that
    /// cannot name the tier must not claim the figures meet the bar.
    fn from(v: &TransferView) -> Self {
        TransferState {
            mount: v.mount.clone(),
            fidelity: v.fidelity.as_deref().and_then(tier_from_name),
            outstanding_known: v.outstanding_known,
            has_progress: v.has_progress,
            pending: Pending::new(
                v.pending_files,
                v.pending_known_bytes,
                v.pending_unknown_size_files,
            ),
            uploading: v.uploading,
            errored_files: v.errored_files,
            out_of_space: v.out_of_space,
            rate_bytes_per_sec: v.rate_bytes_per_sec,
            files: v.files.iter().map(TransferFile::from).collect(),
            degraded_reason: v.degraded_reason.clone(),
        }
    }
}

/// The `Fidelity` key's vocabulary, spelled out for the same reason as [`state_name`].
pub fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::T1 => "T1",
        Tier::T2 => "T2",
        Tier::T3 => "T3",
        Tier::T4 => "T4",
    }
}

/// Read a `Fidelity` back. `None` for a tier this build does not know.
pub fn tier_from_name(name: &str) -> Option<Tier> {
    Some(match name {
        "T1" => Tier::T1,
        "T2" => Tier::T2,
        "T3" => Tier::T3,
        "T4" => Tier::T4,
        _ => return None,
    })
}

/// What the service serves and the clients call.
///
/// Deliberately narrow. Anything that would need rclone's configuration, or that would
/// pass a caller's string to rclone, is absent by design rather than pending.
#[zbus::proxy(
    interface = "io.github.stuckj.RcloneVfsmountTray1",
    default_service = "io.github.stuckj.RcloneVfsmountTray",
    default_path = "/io/github/stuckj/RcloneVfsmountTray"
)]
pub trait RcloneVfsmountTray {
    /// Every configured mount, plus a row for every live mount no unit of ours is serving
    /// where the config puts it. A configured mount that is down is listed as
    /// `"unmounted"`, never omitted.
    fn list_mounts(&self) -> Result<Vec<MountView>, IpcError>;

    /// Bring up a mount. Returns once it is serving, not once rclone has been spawned.
    fn mount(&self, name: &str) -> Result<(), IpcError>;

    /// Tear one down. `force` severs whatever is still using the mount point, so it is
    /// always the caller's explicit decision.
    fn unmount(&self, name: &str, force: bool) -> Result<(), IpcError>;

    /// What one mount still has to upload, as of the last poll.
    fn get_transfer_state(&self, name: &str) -> Result<TransferView, IpcError>;

    /// A mount's state changed, or a row appeared.
    #[zbus(signal)]
    fn mount_state_changed(&self, mount: MountView) -> zbus::Result<()>;

    /// A row went away: a foreign mount unmounted, or an orphan stopped. Configured
    /// mounts do not vanish, so this never fires for one.
    #[zbus(signal)]
    fn mount_removed(&self, name: &str) -> zbus::Result<()>;

    /// A mount's outstanding work changed.
    #[zbus(signal)]
    fn transfer_state_changed(&self, state: TransferView) -> zbus::Result<()>;

    /// [`INTERFACE_VERSION`] of the running service.
    #[zbus(property)]
    fn interface_version(&self) -> Result<u32, IpcError>;

    /// The service's own package version.
    #[zbus(property)]
    fn service_version(&self) -> Result<String, IpcError>;

    /// The rclone this service found, as rclone reports it.
    #[zbus(property)]
    fn rclone_version(&self) -> Result<String, IpcError>;

    /// The best tier any mount has resolved, or `"unknown"` before one has.
    ///
    /// A property of this rclone, not of a reading: what a given mount can actually say
    /// is its own `TransferView`'s `Fidelity`, which is lower whenever its cache mode
    /// hides writes.
    #[zbus(property)]
    fn capability_tier(&self) -> Result<String, IpcError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{serialized::Context, Endian, Value};

    /// Encode and decode through the D-Bus format, exactly as a call would.
    fn round_trip<T>(v: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Type,
    {
        let ctx = Context::new_dbus(Endian::native(), 0);
        let bytes = zbus::zvariant::to_bytes(ctx, v).expect("serialize");
        bytes.deserialize::<T>().expect("deserialize").0
    }

    fn a_mount_view() -> MountView {
        MountView {
            name: "photos".into(),
            state: "mounted".into(),
            live: true,
            managed: true,
            reason: None,
            mount_point: Some("/home/j/mnt/photos".into()),
            remote: Some("gdrive:Photos".into()),
        }
    }

    #[test]
    fn a_mount_view_survives_the_wire() {
        assert_eq!(round_trip(&a_mount_view()), a_mount_view());
    }

    #[test]
    fn a_key_this_build_does_not_know_is_ignored() {
        // The whole reason the payloads are dictionaries: a client must keep working
        // against a service that has learned to say something more.
        let mut dict: HashMap<&str, Value<'_>> = HashMap::new();
        dict.insert("Name", "photos".into());
        dict.insert("State", "mounted".into());
        dict.insert("Live", true.into());
        dict.insert("Managed", true.into());
        dict.insert("SomethingAddedLater", 42u32.into());

        let ctx = Context::new_dbus(Endian::native(), 0);
        let bytes = zbus::zvariant::to_bytes(ctx, &dict).expect("serialize");
        let got = bytes.deserialize::<MountView>().expect("deserialize").0;

        assert_eq!(got.name, "photos");
        assert_eq!(
            got.mount_point, None,
            "an absent optional key is not an error"
        );
    }

    #[test]
    fn every_mount_state_has_a_name_that_reads_back() {
        for state in [
            MountState::Unmounted,
            MountState::Mounting,
            MountState::Mounted,
            MountState::Unmounting,
            MountState::Failed {
                reason: "rclone exited".into(),
            },
            MountState::Foreign,
            MountState::Orphaned,
        ] {
            let view = MountView::from(&DiscoveredMount::new("m", state.clone()));
            let back = state_from_name(&view.state, view.reason.as_deref());
            assert_eq!(
                back.as_ref(),
                Some(&state),
                "{} did not read back",
                view.state
            );
        }
    }

    #[test]
    fn an_unknown_state_still_says_whether_anything_is_mounted() {
        // What `Live` and `Managed` are for: a client one release behind must not have to
        // guess whether a state it cannot name means a filesystem is serving.
        assert_eq!(state_from_name("hibernating", None), None);
        let view = MountView {
            state: "hibernating".into(),
            ..a_mount_view()
        };
        assert!(round_trip(&view).live);
    }

    #[test]
    fn a_failed_mount_carries_its_reason_across() {
        let found = DiscoveredMount::new(
            "photos",
            MountState::Failed {
                reason: "mount point /mnt/photos is held by another unit".into(),
            },
        );
        let view = round_trip(&MountView::from(&found));
        assert_eq!(view.state, "failed");
        assert!(view.reason.unwrap().contains("held by another unit"));
    }

    #[test]
    fn a_discovered_mount_point_reaches_the_wire() {
        let found = DiscoveredMount::new("photos", MountState::Mounted).at("/mnt/photos");
        assert_eq!(
            round_trip(&MountView::from(&found)).mount_point.as_deref(),
            Some("/mnt/photos")
        );
    }

    fn a_transfer_state() -> TransferState {
        TransferState {
            mount: "photos".into(),
            fidelity: Some(Tier::T2),
            outstanding_known: true,
            has_progress: false,
            pending: Pending::new(3, 1_288_490_188, 1),
            uploading: Some(1),
            errored_files: Some(0),
            out_of_space: Some(false),
            rate_bytes_per_sec: Some(4_194_304),
            files: vec![TransferFile {
                name: "holiday.mp4".into(),
                size: Some(1_288_490_188),
                in_flight: Some(true),
                tries: Some(2),
                bytes_sent: None,
            }],
            degraded_reason: None,
        }
    }

    #[test]
    fn a_transfer_state_survives_the_round_trip_intact() {
        let sent = a_transfer_state();
        let back = TransferState::from(&round_trip(&TransferView::from(&sent)));
        assert_eq!(back, sent);
    }

    #[test]
    fn an_unsayable_total_stays_unsayable_across_the_wire() {
        // `fidelity: None` means no source produced a total. Reading it back as a tier —
        // any tier — would let a client call the figures behind it trustworthy.
        let mut sent = a_transfer_state();
        sent.fidelity = None;
        sent.outstanding_known = false;
        sent.degraded_reason = Some("rclone unreachable".into());

        let back = TransferState::from(&round_trip(&TransferView::from(&sent)));
        assert_eq!(back.fidelity, None);
        assert!(!back.outstanding_known);
        assert!(!back.safe_to_unmount());
    }

    #[test]
    fn a_tier_this_build_cannot_name_reads_as_no_tier() {
        assert_eq!(tier_from_name("T0"), None);
        let view = TransferView {
            fidelity: Some("T0".into()),
            ..TransferView::from(&a_transfer_state())
        };
        assert_eq!(TransferState::from(&view).fidelity, None);
    }

    #[test]
    fn every_tier_has_a_name_that_reads_back() {
        for tier in [Tier::T1, Tier::T2, Tier::T3, Tier::T4] {
            assert_eq!(tier_from_name(tier_name(tier)), Some(tier));
        }
    }

    #[test]
    fn each_supervisor_failure_gets_its_own_error_name() {
        use zbus::DBusError as _;

        // Every variant `From<SupervisorError>` maps. The compiler catches a *new* one, but
        // not one mapped to the wrong name, and two failures sharing a name is a client
        // that cannot tell them apart.
        let cases = [
            (
                SupervisorError::UnknownMount("photos".into()),
                "UnknownMount",
            ),
            (
                SupervisorError::BadMountPoint {
                    path: "/mnt/photos".into(),
                    reason: "not a directory".into(),
                    source: None,
                },
                "BadMountPoint",
            ),
            (
                SupervisorError::RcloneFailed {
                    reason: "exited 1".into(),
                    source: None,
                },
                "RcloneFailed",
            ),
            (
                SupervisorError::PendingUploads(Pending::new(3, 1024, 0)),
                "PendingUploads",
            ),
            (
                SupervisorError::Busy {
                    detail: "still in use".into(),
                },
                "Busy",
            ),
            (
                SupervisorError::Supervision {
                    context: "starting unit".into(),
                    source: None,
                },
                "Supervision",
            ),
            (SupervisorError::NotManaged("photos".into()), "NotManaged"),
        ];

        let mut seen = std::collections::BTreeSet::new();
        for (err, expected) in cases {
            let text = err.to_string();
            let ipc = IpcError::from(err);
            let name = ipc.name().to_string();
            assert_eq!(
                name,
                format!("io.github.stuckj.RcloneVfsmountTray1.Error.{expected}")
            );
            assert_eq!(
                ipc.description(),
                Some(text.as_str()),
                "the sentence a client shows must be the supervisor's own"
            );
            assert!(seen.insert(name.clone()), "{name} is used twice");
        }
        assert_eq!(seen.len(), 7);
    }
}
