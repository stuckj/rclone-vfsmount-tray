//! Reaching the service, and telling the ways that fail apart.
//!
//! Shared by the one-shot subcommands in [`crate::client`] and the long-lived tray. The
//! distinctions here are the ones #52 turns on: a service that is stopped, a service too
//! different to talk to, and a bus that is not there are three separate situations, and
//! none of them says anything about whether a mount is up.

use rvt_core::ipc::{self, IpcError, RcloneVfsmountTrayProxy};
use zbus::proxy::CacheProperties;
use zbus::DBusError as _;

/// The service's user unit, which the tray's menu starts and stops.
pub(crate) const UNIT: &str = "rclone-vfsmount-trayd";

/// What to run `systemctl --user start` against, quoted once so the message and the JSON
/// `start_hint` cannot drift apart.
pub(crate) const START_HINT: &str = "systemctl --user start rclone-vfsmount-trayd";

/// The lowest interface version whose vocabulary this build uses.
///
/// Every method and property it calls was published at version 1, so a service reporting
/// anything lower is too old to answer them. No released service does — but the check turns a
/// future gap into a sentence rather than an `UnknownMethod` surfacing halfway through.
pub(crate) const MIN_INTERFACE: u32 = 1;

/// Why the service could not be reached, or could not be understood.
///
/// [`Self::Refused`] is set apart from the rest: it is the service answering and saying no,
/// and it is the only kind that says anything about a mount. Everything else is the question
/// never being answered, and is reported without ever implying a mount is absent.
#[derive(Debug)]
pub(crate) enum LinkError {
    /// No session bus to reach. The desktop session provides one; without it there is
    /// nothing to connect to, service or not.
    NoSessionBus(zbus::Error),
    /// The bus is there and nobody owns the service's name — the service is stopped.
    NotRunning,
    /// Something owns the name but does not answer the interface this build calls: a service
    /// too different to talk to, or an unrelated program holding the name.
    Incompatible,
    /// The service is older than the vocabulary a subcommand needs.
    TooOld { needed: u32, found: u32 },
    /// The service answered and refused. The sentence to show is its own.
    Refused(IpcError),
    /// The call reached the bus and failed for some other reason.
    Transport(zbus::Error),
}

impl LinkError {
    /// A distinct code per failure so a script can branch without parsing prose. Used by the
    /// subcommands; the tray shows [`Self::message`] instead.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            LinkError::Refused(_) => 1,
            LinkError::NotRunning => 3,
            LinkError::Incompatible | LinkError::TooOld { .. } => 4,
            LinkError::NoSessionBus(_) | LinkError::Transport(_) => 5,
        }
    }

    /// The sentence to print to stderr.
    pub(crate) fn message(&self) -> String {
        match self {
            LinkError::NoSessionBus(e) => format!(
                "No session bus to reach ({e}). This runs inside a desktop session; over SSH, \
                 point DBUS_SESSION_BUS_ADDRESS at the user bus."
            ),
            LinkError::NotRunning => format!(
                "The rclone-vfsmount-tray service is not running. Start it with:\n    {START_HINT}\n\
                 This says nothing about your mounts: any that were up are still up."
            ),
            LinkError::Incompatible => format!(
                "The service on this bus does not answer {}. It is a different, incompatible \
                 version of the interface; update the client and service to match.",
                ipc::INTERFACE_NAME
            ),
            LinkError::TooOld { needed, found } => format!(
                "This command needs interface version {needed}, but the service provides {found}. \
                 Update rclone-vfsmount-trayd."
            ),
            LinkError::Refused(e) => e
                .description()
                .unwrap_or("the service refused the request")
                .to_string(),
            LinkError::Transport(e) => format!("The service could not be reached: {e}"),
        }
    }
}

/// The three ways a call fails at the wire, told apart by the D-Bus error name.
enum Wire {
    NotRunning,
    Incompatible,
    Transport,
}

/// Sort a D-Bus error name into [`Wire`]. Split out from the error it came on so the mapping
/// itself is checkable without fabricating a `zbus::Error`.
fn classify_error_name(name: &str) -> Wire {
    match name {
        "org.freedesktop.DBus.Error.ServiceUnknown"
        | "org.freedesktop.DBus.Error.NameHasNoOwner" => Wire::NotRunning,
        // The name is owned, but by something that does not carry this interface, object or
        // member: a service too new or too different, not a stopped one.
        "org.freedesktop.DBus.Error.UnknownInterface"
        | "org.freedesktop.DBus.Error.UnknownObject"
        | "org.freedesktop.DBus.Error.UnknownMethod"
        | "org.freedesktop.DBus.Error.UnknownProperty" => Wire::Incompatible,
        _ => Wire::Transport,
    }
}

/// The D-Bus error name behind a failure, whichever shape zbus surfaced it in.
///
/// A remote error arrives as [`zbus::Error::MethodError`] when its name is unrecognised, but
/// zbus lifts the well-known `org.freedesktop.DBus.Error.*` names — the ones that separate a
/// stopped service from a mismatched one — into [`zbus::Error::FDO`]. Both carry the name;
/// classification needs it from either.
fn dbus_error_name(e: &zbus::Error) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_owned()),
        zbus::Error::FDO(fdo) => Some(fdo.name().as_str().to_owned()),
        _ => None,
    }
}

fn classify(e: &zbus::Error) -> Wire {
    match dbus_error_name(e) {
        Some(name) => classify_error_name(&name),
        None => Wire::Transport,
    }
}

/// Sort a bare wire failure into the connection-level kinds.
///
/// A stopped service surfaces its `ServiceUnknown` at whichever call reaches for it first —
/// building the proxy resolves the name, so it can land there rather than at the property
/// read. Both paths run through here so the verdict does not depend on which call tripped.
pub(crate) fn from_zbus(e: zbus::Error) -> LinkError {
    match classify(&e) {
        Wire::NotRunning => LinkError::NotRunning,
        Wire::Incompatible => LinkError::Incompatible,
        Wire::Transport => LinkError::Transport(e),
    }
}

/// Turn an [`IpcError`] from a call into a [`LinkError`]. An application refusal keeps its
/// identity; a transport failure is sorted into the connection-level kinds.
pub(crate) fn from_ipc(e: IpcError) -> LinkError {
    match e {
        IpcError::ZBus(z) => from_zbus(z),
        // `#[non_exhaustive]`: a new application error still reaches the user as its own
        // refusal rather than being mistaken for a transport failure.
        other => LinkError::Refused(other),
    }
}

/// Build the proxy and confirm the service speaks a version these commands can use.
///
/// Reading `InterfaceVersion` first is the handshake: it is the cheapest call, and it is
/// where an absent or incompatible interface surfaces as one clear failure instead of each
/// later method failing on its own. Property caching is off: `CapabilityTier` is the one of
/// the four that changes, the service announces it, and a caller that wants to follow it
/// subscribes to `PropertiesChanged` directly — which works either way, where the cache-fed
/// property streams do not exist at all without the cache.
pub(crate) async fn open(
    conn: &zbus::Connection,
) -> Result<RcloneVfsmountTrayProxy<'static>, LinkError> {
    match owner_on(conn).await? {
        Some(owner) => open_owner(conn, owner).await,
        None => build(RcloneVfsmountTrayProxy::builder(conn)).await,
    }
}

/// Who holds the service's name, asked of the bus daemon rather than of the service.
///
/// `None` for a connection with no bus behind it — a socket pair in a test — where there is
/// no daemon to ask, no well-known name in play, and nothing that could be activated.
pub(crate) async fn owner_on(
    conn: &zbus::Connection,
) -> Result<Option<zbus::names::OwnedUniqueName>, LinkError> {
    if conn.unique_name().is_none() {
        return Ok(None);
    }
    let name = zbus::names::BusName::try_from(ipc::BUS_NAME).expect("BUS_NAME is well formed");
    let dbus = zbus::fdo::DBusProxy::new(conn).await.map_err(from_zbus)?;
    match dbus.get_name_owner(name).await {
        Ok(owner) => Ok(Some(owner)),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => Err(LinkError::NotRunning),
        Err(e) => Err(from_zbus(e.into())),
    }
}

/// Addressed to one connection rather than to the service's well-known name.
///
/// A unique name cannot be activated. Addressing the calls to it means that if the service
/// exits while the proxy is held, the next call fails instead of starting a new one — which
/// is what #52 asks for, and what asking who owns the name cannot guarantee on its own, since
/// the service can go in the moment between the two.
pub(crate) async fn open_owner(
    conn: &zbus::Connection,
    owner: zbus::names::OwnedUniqueName,
) -> Result<RcloneVfsmountTrayProxy<'static>, LinkError> {
    let builder = RcloneVfsmountTrayProxy::builder(conn)
        .destination(zbus::names::BusName::from(owner))
        .map_err(from_zbus)?;
    build(builder).await
}

async fn build(
    builder: zbus::proxy::Builder<'static, RcloneVfsmountTrayProxy<'static>>,
) -> Result<RcloneVfsmountTrayProxy<'static>, LinkError> {
    let proxy = builder
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(from_zbus)?;
    let version = proxy.interface_version().await.map_err(from_ipc)?;
    if version < MIN_INTERFACE {
        return Err(LinkError::TooOld {
            needed: MIN_INTERFACE,
            found: version,
        });
    }
    Ok(proxy)
}

/// How long to wait before trying the link again.
///
/// Only failures that are *not* a stopped service back off: an absent service is waited for
/// on `NameOwnerChanged` and costs nothing, while a bus that refuses the connection would
/// otherwise be retried in a tight loop for as long as the session lasts.
pub(crate) struct Backoff {
    next: std::time::Duration,
}

impl Backoff {
    /// First retry after a second, doubling to a minute.
    const FIRST: std::time::Duration = std::time::Duration::from_secs(1);
    const CEILING: std::time::Duration = std::time::Duration::from_secs(60);

    pub(crate) fn new() -> Self {
        Self { next: Self::FIRST }
    }

    /// The delay to wait now, lengthening the one after it.
    pub(crate) fn take(&mut self) -> std::time::Duration {
        let now = self.next;
        self.next = (now * 2).min(Self::CEILING);
        now
    }

    /// Back to the first interval, after a connection that worked.
    pub(crate) fn reset(&mut self) {
        self.next = Self::FIRST;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_not_running_message_offers_the_start_command_and_denies_the_inference() {
        let m = LinkError::NotRunning.message();
        assert!(m.contains(START_HINT), "{m}");
        assert!(
            m.contains("still up"),
            "the message must refuse the disconnected-means-unmounted reading: {m}"
        );
        assert_eq!(LinkError::NotRunning.exit_code(), 3);
    }

    #[test]
    fn error_names_sort_into_the_right_bucket() {
        assert!(matches!(
            classify_error_name("org.freedesktop.DBus.Error.ServiceUnknown"),
            Wire::NotRunning
        ));
        // Both names a bus daemon uses for "nobody is answering to that".
        assert!(matches!(
            classify_error_name("org.freedesktop.DBus.Error.NameHasNoOwner"),
            Wire::NotRunning
        ));
        assert!(matches!(
            classify_error_name("org.freedesktop.DBus.Error.UnknownInterface"),
            Wire::Incompatible
        ));
        assert!(matches!(
            classify_error_name("org.freedesktop.DBus.Error.AccessDenied"),
            Wire::Transport
        ));
    }

    #[test]
    fn the_start_hint_names_the_unit_the_menu_acts_on() {
        // Two spellings of one unit: the sentence the CLI prints and the argument the tray's
        // "Start service" item passes to systemctl.
        assert!(
            START_HINT.ends_with(UNIT),
            "{START_HINT:?} does not start {UNIT:?}"
        );
    }

    #[test]
    fn the_wait_doubles_up_to_the_ceiling_and_stays_there() {
        let mut b = Backoff::new();
        let waits: Vec<Duration> = (0..10).map(|_| b.take()).collect();
        assert_eq!(waits[0], Backoff::FIRST);
        assert_eq!(waits[1], Duration::from_secs(2));
        assert_eq!(waits[2], Duration::from_secs(4));
        assert_eq!(
            *waits.last().unwrap(),
            Backoff::CEILING,
            "a long outage must not grow the wait without bound"
        );
        assert!(
            waits.windows(2).all(|w| w[0] <= w[1]),
            "the wait never shortens without a reset: {waits:?}"
        );
    }

    #[test]
    fn a_connection_that_worked_starts_the_wait_over() {
        // A service that restarts every few minutes must not inherit the ceiling from an
        // outage hours earlier — the next restart would take a minute to notice.
        let mut b = Backoff::new();
        for _ in 0..10 {
            b.take();
        }
        b.reset();
        assert_eq!(b.take(), Backoff::FIRST);
    }
}
