//! Driving the service from the command line: connect, check the two sides can understand
//! each other, run one subcommand, print, exit.
//!
//! A one-shot process — it does not subscribe to signals the way the tray will. The care
//! here is in the failure modes (#52). A service that is not running, or one speaking an
//! interface this build was not compiled against, must produce a plain sentence and a
//! distinct exit code; and "cannot reach the service" must never be rendered as "nothing is
//! mounted", because the two are not the same and confusing them is how a user is told their
//! files are gone when they are not.

use std::io::Write;

use rvt_core::ipc::{
    self, IpcError, MountView, RcloneVfsmountTrayProxy, TransferFileView, TransferView,
};
use zbus::proxy::CacheProperties;
use zbus::DBusError as _;

/// What to run `systemctl --user start` against, quoted once so the message and the JSON
/// `start_hint` cannot drift apart.
const START_HINT: &str = "systemctl --user start rclone-vfsmount-trayd";

/// The lowest interface version whose vocabulary these subcommands use.
///
/// Every method and property they call was published at version 1, so a service reporting
/// anything lower is too old to answer them. No released service does — but the check turns
/// a future gap into a sentence rather than an `UnknownMethod` surfacing halfway through.
const MIN_INTERFACE: u32 = 1;

/// Why a subcommand could not be carried out.
///
/// [`Self::Refused`] is set apart from the rest: it is the service answering and saying no,
/// and it is the only kind that says anything about a mount. Everything else is the question
/// never being answered, and is reported without ever implying a mount is absent.
#[derive(Debug)]
pub(crate) enum CliError {
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

impl CliError {
    /// A distinct code per failure so a script can branch without parsing prose.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            CliError::Refused(_) => 1,
            CliError::NotRunning => 3,
            CliError::Incompatible | CliError::TooOld { .. } => 4,
            CliError::NoSessionBus(_) | CliError::Transport(_) => 5,
        }
    }

    /// The sentence to print to stderr.
    pub(crate) fn message(&self) -> String {
        match self {
            CliError::NoSessionBus(e) => format!(
                "No session bus to reach ({e}). This runs inside a desktop session; over SSH, \
                 point DBUS_SESSION_BUS_ADDRESS at the user bus."
            ),
            CliError::NotRunning => format!(
                "The rclone-vfsmount-tray service is not running. Start it with:\n    {START_HINT}\n\
                 This says nothing about your mounts: any that were up are still up."
            ),
            CliError::Incompatible => format!(
                "The service on this bus does not answer {}. It is a different, incompatible \
                 version of the interface; update the client and service to match.",
                ipc::INTERFACE_NAME
            ),
            CliError::TooOld { needed, found } => format!(
                "This command needs interface version {needed}, but the service provides {found}. \
                 Update rclone-vfsmount-trayd."
            ),
            CliError::Refused(e) => e
                .description()
                .unwrap_or("the service refused the request")
                .to_string(),
            CliError::Transport(e) => format!("The service could not be reached: {e}"),
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
fn from_zbus(e: zbus::Error) -> CliError {
    match classify(&e) {
        Wire::NotRunning => CliError::NotRunning,
        Wire::Incompatible => CliError::Incompatible,
        Wire::Transport => CliError::Transport(e),
    }
}

/// Turn an [`IpcError`] from a call into a [`CliError`]. An application refusal keeps its
/// identity; a transport failure is sorted into the connection-level kinds.
fn from_ipc(e: IpcError) -> CliError {
    match e {
        IpcError::ZBus(z) => from_zbus(z),
        // `#[non_exhaustive]`: a new application error still reaches the user as its own
        // refusal rather than being mistaken for a transport failure.
        other => CliError::Refused(other),
    }
}

/// Build the proxy and confirm the service speaks a version these commands can use.
///
/// Reading `InterfaceVersion` first is the handshake: it is the cheapest call, and it is
/// where an absent or incompatible interface surfaces as one clear failure instead of each
/// later method failing on its own. Property caching is off — a one-shot process reads each
/// once and has no use for a `PropertiesChanged` subscription.
async fn open(conn: &zbus::Connection) -> Result<RcloneVfsmountTrayProxy<'_>, CliError> {
    let proxy = RcloneVfsmountTrayProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(from_zbus)?;
    let version = proxy.interface_version().await.map_err(from_ipc)?;
    if version < MIN_INTERFACE {
        return Err(CliError::TooOld {
            needed: MIN_INTERFACE,
            found: version,
        });
    }
    Ok(proxy)
}

/// Run one subcommand against a connection that may itself have failed to open.
///
/// `conn` carries the connect failure rather than being unwrapped by the caller so that
/// `status` can still emit a document describing the disconnection — the case a script most
/// needs to tell apart from an empty mount list.
pub(crate) async fn execute(
    conn: Result<zbus::Connection, CliError>,
    cmd: &crate::Command,
    out: &mut impl Write,
) -> Result<(), CliError> {
    match cmd {
        crate::Command::Status { json } => run_status(conn, *json, out).await,
        crate::Command::List => {
            let conn = conn?;
            let proxy = open(&conn).await?;
            let mounts = proxy.list_mounts().await.map_err(from_ipc)?;
            render_list(&mounts, out);
            Ok(())
        }
        crate::Command::Mount { name } => {
            let conn = conn?;
            let proxy = open(&conn).await?;
            proxy.mount(name).await.map_err(from_ipc)?;
            let _ = writeln!(out, "mounted {name}");
            Ok(())
        }
        crate::Command::Unmount { name, force } => {
            let conn = conn?;
            let proxy = open(&conn).await?;
            proxy.unmount(name, *force).await.map_err(from_ipc)?;
            let _ = writeln!(out, "unmounted {name}");
            Ok(())
        }
    }
}

/// One mount plus what it still has to upload, as `status` reports it.
struct MountEntry {
    view: MountView,
    transfer: Option<TransferView>,
}

/// Everything `status` prints when the service answers.
struct StatusSnapshot {
    interface_version: u32,
    service_version: String,
    rclone_version: String,
    capability_tier: String,
    mounts: Vec<MountEntry>,
}

async fn run_status(
    conn: Result<zbus::Connection, CliError>,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    match conn {
        Ok(conn) => match gather_status(&conn).await {
            Ok(snap) => {
                emit_status(&snap, json, out);
                Ok(())
            }
            Err(e) => {
                emit_disconnected(&e, json, out);
                Err(e)
            }
        },
        Err(e) => {
            emit_disconnected(&e, json, out);
            Err(e)
        }
    }
}

async fn gather_status(conn: &zbus::Connection) -> Result<StatusSnapshot, CliError> {
    let proxy = open(conn).await?;
    let interface_version = proxy.interface_version().await.map_err(from_ipc)?;
    let service_version = proxy.service_version().await.map_err(from_ipc)?;
    let rclone_version = proxy.rclone_version().await.map_err(from_ipc)?;
    let capability_tier = proxy.capability_tier().await.map_err(from_ipc)?;
    let mounts = proxy.list_mounts().await.map_err(from_ipc)?;

    let mut entries = Vec::with_capacity(mounts.len());
    for view in mounts {
        // A per-mount transfer read that fails is not a reason to fail the whole status:
        // the mount is still reported, without its outstanding work. `get_transfer_state`
        // on a listed mount only fails at the wire, and a dead wire would already have
        // shown up in the reads above.
        let transfer = proxy.get_transfer_state(&view.name).await.ok();
        entries.push(MountEntry { view, transfer });
    }
    Ok(StatusSnapshot {
        interface_version,
        service_version,
        rclone_version,
        capability_tier,
        mounts: entries,
    })
}

fn emit_status(snap: &StatusSnapshot, json: bool, out: &mut impl Write) {
    if json {
        let _ = writeln!(out, "{}", status_json(snap));
    } else {
        render_status_human(snap, out);
    }
}

/// For `status --json`, a failure still produces a JSON document — one that says the service
/// is unreachable and leaves `mounts` **null**. `null` is not `[]`: an empty array would read
/// as "the service is up and has no mounts", the exact claim this must never make.
fn emit_disconnected(e: &CliError, json: bool, out: &mut impl Write) {
    if json {
        let _ = writeln!(out, "{}", disconnected_json(e));
    }
    // Non-JSON callers get the sentence on stderr from `main`; nothing on stdout, so a
    // pipeline sees no rows rather than a misleading blank table.
}

fn status_json(snap: &StatusSnapshot) -> String {
    let mounts: Vec<serde_json::Value> = snap.mounts.iter().map(mount_entry_json).collect();
    serde_json::json!({
        "connected": true,
        "service": {
            "service_version": snap.service_version,
            "interface_version": snap.interface_version,
            "client_interface_version": ipc::INTERFACE_VERSION,
            "rclone_version": snap.rclone_version,
            "capability_tier": snap.capability_tier,
        },
        "mounts": mounts,
    })
    .to_string()
}

fn disconnected_json(e: &CliError) -> String {
    let reason = match e {
        CliError::NotRunning => "service not running",
        CliError::NoSessionBus(_) => "no session bus",
        CliError::Incompatible => "interface incompatible",
        CliError::TooOld { .. } => "service too old",
        CliError::Transport(_) => "service unreachable",
        CliError::Refused(_) => "request refused",
    };
    let mut doc = serde_json::json!({
        "connected": false,
        "reason": reason,
        "detail": e.message(),
        "mounts": serde_json::Value::Null,
    });
    if matches!(e, CliError::NotRunning) {
        doc["start_hint"] = serde_json::json!(START_HINT);
    }
    doc.to_string()
}

fn mount_entry_json(e: &MountEntry) -> serde_json::Value {
    serde_json::json!({
        "name": e.view.name,
        "state": e.view.state,
        "live": e.view.live,
        "managed": e.view.managed,
        "reason": e.view.reason,
        "mount_point": e.view.mount_point,
        "remote": e.view.remote,
        "transfer": e.transfer.as_ref().map(transfer_json),
    })
}

fn transfer_json(t: &TransferView) -> serde_json::Value {
    serde_json::json!({
        "fidelity": t.fidelity,
        "outstanding_known": t.outstanding_known,
        "has_progress": t.has_progress,
        "pending_files": t.pending_files,
        "pending_known_bytes": t.pending_known_bytes,
        "pending_unknown_size_files": t.pending_unknown_size_files,
        "uploading": t.uploading,
        "errored_files": t.errored_files,
        "out_of_space": t.out_of_space,
        "rate_bytes_per_sec": t.rate_bytes_per_sec,
        "degraded_reason": t.degraded_reason,
        "files": t.files.iter().map(transfer_file_json).collect::<Vec<_>>(),
    })
}

fn transfer_file_json(f: &TransferFileView) -> serde_json::Value {
    serde_json::json!({
        "name": f.name,
        "size": f.size,
        "in_flight": f.in_flight,
        "tries": f.tries,
        "bytes_sent": f.bytes_sent,
    })
}

/// The name column, wide enough for the longest name and never narrower than its header.
fn name_width(names: impl Iterator<Item = usize>) -> usize {
    names
        .chain(std::iter::once("NAME".len()))
        .max()
        .unwrap_or(4)
}

fn render_list(mounts: &[MountView], out: &mut impl Write) {
    if mounts.is_empty() {
        let _ = writeln!(out, "No mounts configured.");
        return;
    }
    let w = name_width(mounts.iter().map(|m| m.name.len()));
    let _ = writeln!(
        out,
        "{:<w$}  {:<10}  {:<7}  MOUNT POINT",
        "NAME", "STATE", "LIVE"
    );
    for m in mounts {
        let _ = writeln!(
            out,
            "{:<w$}  {:<10}  {:<7}  {}",
            m.name,
            m.state,
            if m.live { "live" } else { "-" },
            m.mount_point.as_deref().unwrap_or("-"),
        );
    }
}

fn render_status_human(snap: &StatusSnapshot, out: &mut impl Write) {
    let _ = writeln!(
        out,
        "service {}  (interface {}, client {})",
        snap.service_version,
        snap.interface_version,
        ipc::INTERFACE_VERSION,
    );
    let _ = writeln!(out, "rclone  {}", snap.rclone_version);
    let _ = writeln!(out, "capability tier: {}", snap.capability_tier);
    let _ = writeln!(out);

    if snap.mounts.is_empty() {
        let _ = writeln!(out, "No mounts configured.");
        return;
    }
    let w = name_width(snap.mounts.iter().map(|e| e.view.name.len()));
    let _ = writeln!(out, "{:<w$}  {:<10}  PENDING", "NAME", "STATE");
    for e in &snap.mounts {
        let _ = writeln!(
            out,
            "{:<w$}  {:<10}  {}",
            e.view.name,
            e.view.state,
            pending_summary(e)
        );
    }
}

/// One line of what a mount still has to upload, or a dash when there is nothing to say.
fn pending_summary(e: &MountEntry) -> String {
    let Some(t) = &e.transfer else {
        return "-".to_string();
    };
    if !t.outstanding_known {
        // The tier that produced the reading could not vouch for a total. Saying "0 files"
        // here would claim the mount is clear when the truth is it cannot be measured.
        return "unknown".to_string();
    }
    if t.pending_files == 0 {
        return "up to date".to_string();
    }
    format!(
        "{} file(s), {} bytes",
        t.pending_files, t.pending_known_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A minimal service on the far end of a socket pair, so the client is exercised against
    /// a real zbus server and the real wire types rather than a mock of the proxy.
    #[derive(Clone)]
    struct Fake {
        mounts: Vec<MountView>,
        interface_version: u32,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "io.github.stuckj.RcloneVfsmountTray1")]
    impl Fake {
        async fn list_mounts(&self) -> Vec<MountView> {
            self.mounts.clone()
        }

        async fn mount(&self, name: &str) -> Result<(), IpcError> {
            self.calls.lock().unwrap().push(format!("mount {name}"));
            if self.mounts.iter().any(|m| m.name == name) {
                Ok(())
            } else {
                Err(IpcError::UnknownMount(format!("no mount named {name:?}")))
            }
        }

        async fn unmount(&self, name: &str, force: bool) -> Result<(), IpcError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("unmount {name} force={force}"));
            Ok(())
        }

        async fn get_transfer_state(&self, name: &str) -> Result<TransferView, IpcError> {
            Ok(a_transfer(name))
        }

        #[zbus(property)]
        async fn interface_version(&self) -> u32 {
            self.interface_version
        }
        #[zbus(property)]
        async fn service_version(&self) -> String {
            "9.9.9".into()
        }
        #[zbus(property)]
        async fn rclone_version(&self) -> String {
            "1.75.0".into()
        }
        #[zbus(property)]
        async fn capability_tier(&self) -> String {
            "T2".into()
        }
    }

    /// A program on the service's object that carries a different, incompatible interface.
    struct Stranger;

    #[zbus::interface(name = "io.github.stuckj.RcloneVfsmountTray2")]
    impl Stranger {
        #[zbus(property)]
        async fn interface_version(&self) -> u32 {
            2
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
            remote: Some(format!("remote:{name}")),
        }
    }

    fn a_transfer(mount: &str) -> TransferView {
        TransferView {
            mount: mount.into(),
            fidelity: Some("T2".into()),
            outstanding_known: true,
            has_progress: false,
            pending_files: 3,
            pending_known_bytes: 1_288_490_188,
            pending_unknown_size_files: 1,
            uploading: Some(1),
            errored_files: Some(0),
            out_of_space: Some(false),
            rate_bytes_per_sec: Some(4_194_304),
            files: vec![],
            degraded_reason: None,
        }
    }

    /// Serve one interface over a socket pair and hand back the client's end.
    async fn serve<I>(iface: I) -> (zbus::Connection, zbus::Connection)
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

    fn fake(mounts: Vec<MountView>) -> (Fake, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Fake {
                mounts,
                interface_version: 1,
                calls: calls.clone(),
            },
            calls,
        )
    }

    #[tokio::test]
    async fn list_prints_the_mounts_the_service_returns() {
        let (f, _) = fake(vec![
            a_mount("photos", "mounted"),
            a_mount("docs", "unmounted"),
        ]);
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        execute(Ok(client), &crate::Command::List, &mut out)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("photos"), "{text}");
        assert!(text.contains("docs"), "{text}");
        assert!(text.contains("/mnt/photos"), "{text}");
    }

    #[tokio::test]
    async fn mount_reaches_the_service() {
        let (f, calls) = fake(vec![a_mount("photos", "unmounted")]);
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        execute(
            Ok(client),
            &crate::Command::Mount {
                name: "photos".into(),
            },
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), ["mount photos"]);
    }

    #[tokio::test]
    async fn unmount_passes_force_through() {
        let (f, calls) = fake(vec![a_mount("photos", "mounted")]);
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        execute(
            Ok(client),
            &crate::Command::Unmount {
                name: "photos".into(),
                force: true,
            },
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["unmount photos force=true"]
        );
    }

    #[tokio::test]
    async fn a_refused_call_keeps_the_services_sentence_and_exits_one() {
        let (f, _) = fake(vec![a_mount("photos", "mounted")]);
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        let err = execute(
            Ok(client),
            &crate::Command::Mount {
                name: "nope".into(),
            },
            &mut out,
        )
        .await
        .expect_err("mounting an unknown name must be refused");
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.message().contains("no mount named \"nope\""),
            "the message must be the service's own: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn a_service_one_version_ahead_is_still_usable() {
        // A newer service is forward-compatible by design: the client one release behind
        // must keep working, not refuse to talk.
        let (mut f, _) = fake(vec![a_mount("photos", "mounted")]);
        f.interface_version = 2;
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        execute(Ok(client), &crate::Command::List, &mut out)
            .await
            .expect("a newer service must still answer");
    }

    #[tokio::test]
    async fn a_service_too_old_for_the_command_is_named_not_left_to_fail_late() {
        let (mut f, _) = fake(vec![a_mount("photos", "mounted")]);
        f.interface_version = 0;
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        let err = execute(Ok(client), &crate::Command::List, &mut out)
            .await
            .expect_err("version 0 is below what the commands need");
        assert!(matches!(
            err,
            CliError::TooOld {
                needed: 1,
                found: 0
            }
        ));
        assert_eq!(err.exit_code(), 4);
    }

    #[tokio::test]
    async fn a_stranger_on_the_interface_reads_as_incompatible_not_stopped() {
        let (_s, client) = serve(Stranger).await;
        let mut out = Vec::new();
        let err = execute(Ok(client), &crate::Command::List, &mut out)
            .await
            .expect_err("the wrong interface must not read as success");
        assert!(matches!(err, CliError::Incompatible), "{err:?}");
        assert_eq!(err.exit_code(), 4);
    }

    #[tokio::test]
    async fn status_json_carries_the_stable_keys() {
        let (f, _) = fake(vec![a_mount("photos", "mounted")]);
        let (_s, client) = serve(f).await;
        let mut out = Vec::new();
        execute(Ok(client), &crate::Command::Status { json: true }, &mut out)
            .await
            .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["connected"], serde_json::json!(true));
        assert_eq!(doc["service"]["interface_version"], serde_json::json!(1));
        assert_eq!(
            doc["service"]["client_interface_version"],
            serde_json::json!(ipc::INTERFACE_VERSION)
        );
        assert_eq!(doc["mounts"][0]["name"], serde_json::json!("photos"));
        assert_eq!(doc["mounts"][0]["state"], serde_json::json!("mounted"));
        assert_eq!(
            doc["mounts"][0]["transfer"]["pending_files"],
            serde_json::json!(3)
        );
    }

    #[test]
    fn disconnected_status_is_null_mounts_not_empty() {
        // The invariant #52 exists to protect: a stopped service must never be reported as a
        // service with no mounts. `null` and `[]` are different answers and a script keys on
        // the difference.
        let doc: serde_json::Value =
            serde_json::from_str(&disconnected_json(&CliError::NotRunning)).unwrap();
        assert_eq!(doc["connected"], serde_json::json!(false));
        assert_eq!(doc["mounts"], serde_json::Value::Null);
        assert_eq!(doc["reason"], serde_json::json!("service not running"));
        assert_eq!(doc["start_hint"], serde_json::json!(START_HINT));
    }

    #[test]
    fn the_not_running_message_offers_the_start_command_and_denies_the_inference() {
        let m = CliError::NotRunning.message();
        assert!(m.contains(START_HINT), "{m}");
        assert!(
            m.contains("still up"),
            "the message must refuse the disconnected-means-unmounted reading: {m}"
        );
        assert_eq!(CliError::NotRunning.exit_code(), 3);
    }

    #[test]
    fn error_names_sort_into_the_right_bucket() {
        assert!(matches!(
            classify_error_name("org.freedesktop.DBus.Error.ServiceUnknown"),
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
}
