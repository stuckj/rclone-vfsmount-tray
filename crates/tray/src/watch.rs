//! Keeping the tray in step with the service, across the service's whole lifetime.
//!
//! Two tasks. The *linker* owns the connection: it attaches, republishes everything, follows
//! the signals, and reattaches when the service comes and goes. The *actor* carries out what
//! the menu asked for, one task per action, so a `mount` that takes ten seconds to return
//! delays neither the next action nor Quit.
//!
//! Three orderings here are not free choices:
//!
//! - **Subscribe, then list.** The service emits a signal only when something changes, so a
//!   change landing between a list and a later subscription is never heard of again.
//! - **Ask who owns the name; do not infer it from a call failing.** A method call to an
//!   absent name is what starts a bus-activated service, and the tray must never start the
//!   service by looking at it (#52).
//! - **Drop everything when the link goes.** State is not carried across a service lifetime,
//!   and a stale mount list rendered as current is exactly the claim #52 forbids.

use std::sync::Arc;

use futures_util::StreamExt;
use ksni::Handle;
use rvt_core::ipc::{self, IpcError, RcloneVfsmountTrayProxy};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{watch, Notify};

use crate::link::{self, Backoff, LinkError};
use crate::model::{Action, ServiceInfo, TrayModel};

type Proxy = RcloneVfsmountTrayProxy<'static>;

/// Run the tray until it is asked to stop.
pub(crate) async fn run(handle: Handle<TrayModel>, mut actions: UnboundedReceiver<Action>) {
    let (proxy_tx, proxy_rx) = watch::channel(None);
    let refresh = Arc::new(Notify::new());
    let mut linker = tokio::spawn(link(handle.clone(), proxy_tx, refresh.clone()));

    loop {
        let action = tokio::select! {
            a = actions.recv() => a,
            // The linker returns only when the tray itself has gone.
            _ = &mut linker => None,
        };
        let Some(action) = action else { break };
        match action {
            Action::Quit => break,
            Action::Refresh => refresh.notify_one(),
            other => {
                tokio::spawn(carry_out(handle.clone(), proxy_rx.clone(), other));
            }
        }
    }

    linker.abort();
    handle.shutdown().await;
}

/// What ended an attempt to attach, and what to do next.
enum Step {
    /// The tray has shut down. Stop.
    Gone,
    /// Attach again at once.
    Now,
    /// Attach again, but not immediately.
    Backoff,
    /// This connection to the bus is finished.
    Bus,
}

/// Hold a link to the service open for as long as the tray runs.
async fn link(
    handle: Handle<TrayModel>,
    proxy_tx: watch::Sender<Option<Proxy>>,
    refresh: Arc<Notify>,
) {
    let mut backoff = Backoff::new();
    loop {
        // Two connections, because each has one reader and zbus gives a match rule a queue
        // of 64 with no overflow: a burst larger than that blocks the reader until something
        // drains it. On one connection the thing that would drain it is the loop below, and
        // the loop is at that moment waiting on a method reply that has to come through the
        // same reader — so it never arrives, and the tray freezes rendering the state it had.
        // Split, a burst only delays the signals until the loop is polling them again.
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                if !down(&handle, &proxy_tx, &LinkError::NoSessionBus(e)).await {
                    return;
                }
                // The one wait with no signal to key on: without a bus there is nothing to
                // subscribe to, so this is a timer.
                tokio::select! {
                    _ = tokio::time::sleep(backoff.take()) => {}
                    _ = refresh.notified() => backoff.reset(),
                }
                continue;
            }
        };

        let calls = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                if !down(&handle, &proxy_tx, &LinkError::NoSessionBus(e)).await {
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff.take()) => {}
                    _ = refresh.notified() => backoff.reset(),
                }
                continue;
            }
        };

        match follow(&handle, &conn, &calls, &proxy_tx, &refresh, &mut backoff).await {
            Step::Gone => return,
            _ => {
                tokio::select! {
                    _ = tokio::time::sleep(backoff.take()) => {}
                    _ = refresh.notified() => backoff.reset(),
                }
            }
        }
    }
}

/// Attach and reattach for as long as one bus connection lasts.
async fn follow(
    handle: &Handle<TrayModel>,
    conn: &zbus::Connection,
    calls: &zbus::Connection,
    proxy_tx: &watch::Sender<Option<Proxy>>,
    refresh: &Notify,
    backoff: &mut Backoff,
) -> Step {
    let dbus = match zbus::fdo::DBusProxy::new(conn).await {
        Ok(d) => d,
        Err(e) => {
            down(handle, proxy_tx, &link::from_zbus(e)).await;
            return Step::Bus;
        }
    };
    // Filtered on the service's name at the daemon, so the tray is not woken by every other
    // program on the session bus coming and going.
    let mut owners = match dbus
        .receive_name_owner_changed_with_args(&[(0, ipc::BUS_NAME)])
        .await
    {
        Ok(s) => s,
        Err(e) => {
            down(handle, proxy_tx, &link::from_zbus(e)).await;
            return Step::Bus;
        }
    };

    loop {
        match attach_and_serve(
            handle,
            conn,
            calls,
            &dbus,
            proxy_tx,
            &mut owners,
            refresh,
            backoff,
        )
        .await
        {
            Step::Gone => return Step::Gone,
            Step::Bus => return Step::Bus,
            // The name changed hands. Nothing waits here: the next attempt is against
            // whoever holds it now, and `backoff` is reset by an attach that works.
            Step::Now => continue,
            Step::Backoff => {
                tokio::select! {
                    _ = tokio::time::sleep(backoff.take()) => {}
                    _ = refresh.notified() => backoff.reset(),
                    ev = owners.next() => if ev.is_none() {
                        return bus_lost(handle, proxy_tx).await
                    },
                }
            }
        }
    }
}

/// One attempt: is anyone there, can we talk to them, and then follow them until they go.
#[allow(clippy::too_many_arguments)]
async fn attach_and_serve(
    handle: &Handle<TrayModel>,
    conn: &zbus::Connection,
    calls: &zbus::Connection,
    dbus: &zbus::fdo::DBusProxy<'_>,
    proxy_tx: &watch::Sender<Option<Proxy>>,
    owners: &mut zbus::fdo::NameOwnerChangedStream,
    refresh: &Notify,
    backoff: &mut Backoff,
) -> Step {
    let name = zbus::names::BusName::try_from(ipc::BUS_NAME).expect("BUS_NAME is well formed");
    // `GetNameOwner`, not a call to the service: it is answered by the bus daemon, so it can
    // never activate anything, and it returns the unique name in the same breath.
    let owner = match dbus.get_name_owner(name).await {
        Ok(o) => o,
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
            if !down(handle, proxy_tx, &LinkError::NotRunning).await {
                return Step::Gone;
            }
            // Nothing to retry: the name being taken is itself the signal.
            return tokio::select! {
                ev = owners.next() => if ev.is_some() {
                    Step::Now
                } else {
                    bus_lost(handle, proxy_tx).await
                },
                _ = refresh.notified() => Step::Now,
            };
        }
        Err(e) => {
            down(handle, proxy_tx, &link::from_zbus(e.into())).await;
            return bus_lost(handle, proxy_tx).await;
        }
    };

    let subscriptions = match link::open_owner(conn, owner.clone()).await {
        Ok(p) => p,
        Err(e) => {
            return if down(handle, proxy_tx, &e).await {
                Step::Backoff
            } else {
                Step::Gone
            };
        }
    };
    let proxy = match link::open_owner(calls, owner).await {
        Ok(p) => p,
        Err(e) => {
            // The name is owned but the handshake failed: an incompatible service, or one
            // that is up but not yet answering. Neither is fixed by asking again at once.
            return if down(handle, proxy_tx, &e).await {
                Step::Backoff
            } else {
                Step::Gone
            };
        }
    };

    let mut states = match subscriptions.receive_mount_state_changed().await {
        Ok(s) => s,
        Err(e) => return retry_after(handle, proxy_tx, e).await,
    };
    let mut removals = match subscriptions.receive_mount_removed().await {
        Ok(s) => s,
        Err(e) => return retry_after(handle, proxy_tx, e).await,
    };
    let mut transfers = match subscriptions.receive_transfer_state_changed().await {
        Ok(s) => s,
        Err(e) => return retry_after(handle, proxy_tx, e).await,
    };

    let snapshot = match gather(&proxy).await {
        Ok(s) => s,
        Err(e) => {
            return if down(handle, proxy_tx, &e).await {
                Step::Backoff
            } else {
                Step::Gone
            }
        }
    };
    let (info, mounts, first_transfers) = snapshot;
    tracing::info!(
        service = %info.service_version,
        interface = info.interface_version,
        mounts = mounts.len(),
        "attached to the service"
    );
    // The link works, so the next outage starts its wait from the beginning rather than
    // inheriting a ceiling reached hours ago.
    backoff.reset();
    proxy_tx.send_replace(Some(proxy.clone()));
    let alive = handle
        .update(move |m| {
            m.go_up(info, mounts);
            for t in &first_transfers {
                m.upsert_transfer(t);
            }
        })
        .await
        .is_some();
    if !alive {
        return Step::Gone;
    }

    loop {
        tokio::select! {
            sig = states.next() => {
                let Some(sig) = sig else { return bus_lost(handle, proxy_tx).await };
                if let Ok(args) = sig.args() {
                    if !edit(handle, |m| m.upsert_mount(args.mount)).await { return Step::Gone }
                }
            }
            sig = removals.next() => {
                let Some(sig) = sig else { return bus_lost(handle, proxy_tx).await };
                if let Ok(args) = sig.args() {
                    let name = args.name.to_string();
                    if !edit(handle, |m| m.remove_mount(&name)).await { return Step::Gone }
                }
            }
            sig = transfers.next() => {
                let Some(sig) = sig else { return bus_lost(handle, proxy_tx).await };
                if let Ok(args) = sig.args() {
                    if !edit(handle, |m| m.upsert_transfer(&args.state)).await { return Step::Gone }
                }
            }
            ev = owners.next() => {
                // Whether the name was dropped or handed to a new process, this session is
                // over and the rows go with it. A new instance can be slow to answer the
                // handshake — mid-reconcile, say — and until it does, the previous one's
                // mount table is not something the tray knows to be true.
                let Some(_) = ev else { return bus_lost(handle, proxy_tx).await };
                return if down(handle, proxy_tx, &LinkError::NotRunning).await {
                    Step::Now
                } else {
                    Step::Gone
                };
            }
            _ = refresh.notified() => {
                // Re-read through the proxy already open rather than starting the link
                // over. Reattaching would re-resolve the name, re-add three match rules and
                // drop the notice explaining why the last action was refused — all to learn
                // the same thing this asks for directly.
                match gather(&proxy).await {
                    Ok((info, mounts, readings)) => {
                        let applied = handle
                            .update(move |m| {
                                m.resync(info, mounts);
                                for t in &readings {
                                    m.upsert_transfer(t);
                                }
                            })
                            .await
                            .is_some();
                        if !applied { return Step::Gone }
                    }
                    Err(e) => {
                        return if down(handle, proxy_tx, &e).await {
                            Step::Backoff
                        } else {
                            Step::Gone
                        }
                    }
                }
            }
        }
    }
}

/// Everything a fresh attachment publishes.
type Snapshot = (ServiceInfo, Vec<ipc::MountView>, Vec<ipc::TransferView>);

async fn gather(proxy: &Proxy) -> Result<Snapshot, LinkError> {
    let info = ServiceInfo {
        interface_version: proxy.interface_version().await.map_err(link::from_ipc)?,
        service_version: proxy.service_version().await.map_err(link::from_ipc)?,
        rclone_version: proxy.rclone_version().await.map_err(link::from_ipc)?,
        capability_tier: proxy.capability_tier().await.map_err(link::from_ipc)?,
    };
    let mounts = proxy.list_mounts().await.map_err(link::from_ipc)?;
    let mut transfers = Vec::with_capacity(mounts.len());
    for m in &mounts {
        // A mount whose transfer read fails is still a mount, and keeping the row loses
        // nothing: a live mount with no reading counts as unreadable in the summary, so the
        // tray says it cannot see that one rather than leaving it out of the total.
        if let Ok(t) = proxy.get_transfer_state(&m.name).await {
            transfers.push(t);
        }
    }
    Ok((info, mounts, transfers))
}

async fn retry_after(
    handle: &Handle<TrayModel>,
    proxy_tx: &watch::Sender<Option<Proxy>>,
    e: zbus::Error,
) -> Step {
    if down(handle, proxy_tx, &link::from_zbus(e)).await {
        Step::Backoff
    } else {
        Step::Gone
    }
}

/// The bus connection itself has ended, which every arm of the session loop can discover.
///
/// Routed through one function so that none of them can return leaving the model rendering
/// the mounts this connection last reported. Nothing said over a connection that no longer
/// exists is current, and a stale list drawn as current is what #52 forbids.
async fn bus_lost(handle: &Handle<TrayModel>, proxy_tx: &watch::Sender<Option<Proxy>>) -> Step {
    let ended = zbus::Error::Failure("the connection to the session bus ended".into());
    if down(handle, proxy_tx, &LinkError::NoSessionBus(ended)).await {
        Step::Bus
    } else {
        Step::Gone
    }
}

/// Report the link as unusable and forget what it told us. `false` once the tray has gone.
async fn down(
    handle: &Handle<TrayModel>,
    proxy_tx: &watch::Sender<Option<Proxy>>,
    e: &LinkError,
) -> bool {
    tracing::info!(reason = %e.message(), "no link to the service");
    proxy_tx.send_replace(None);
    handle.update(|m| m.go_down(e)).await.is_some()
}

/// Apply a change and let the panel redraw. `false` once the tray has gone.
async fn edit(handle: &Handle<TrayModel>, f: impl FnOnce(&mut TrayModel)) -> bool {
    handle.update(f).await.is_some()
}

/// Carry out one menu action.
async fn carry_out(handle: Handle<TrayModel>, proxy: watch::Receiver<Option<Proxy>>, a: Action) {
    match a {
        Action::Mount(name) => {
            let Some(p) = current(&proxy) else {
                return unreachable_now(&handle, &name).await;
            };
            let r = call_mount(&p, &name).await;
            report(&handle, &name, r).await;
        }
        Action::Unmount(name) => {
            let Some(p) = current(&proxy) else {
                return unreachable_now(&handle, &name).await;
            };
            let r = call_unmount(&p, &name).await;
            report(&handle, &name, r).await;
        }
        Action::Open(path) => open_in_file_manager(&handle, &path).await,
        Action::StartService => unit(&handle, "start").await,
        Action::StopService => unit(&handle, "stop").await,
        // Both are the linker's or the dispatcher's, not an action to carry out here.
        Action::Refresh | Action::Quit => {}
    }
}

async fn call_mount(p: &Proxy, name: &str) -> Result<(), IpcError> {
    p.mount(name).await
}

/// Take a mount down, never forcing it.
///
/// Forcing detaches the mount point from whatever still holds it, severing a write in flight;
/// rclone then uploads the partial file as though it were complete. That is a decision to make
/// deliberately, at the command line, so the menu has no way to express it — which is why this
/// takes no flag rather than passing `false`. See DESIGN.md, "The unmount order".
async fn call_unmount(p: &Proxy, name: &str) -> Result<(), IpcError> {
    p.unmount(name, false).await
}

/// The proxy as it stands, cloned so nothing holds the watch lock across an await.
fn current(proxy: &watch::Receiver<Option<Proxy>>) -> Option<Proxy> {
    proxy.borrow().clone()
}

async fn unreachable_now(handle: &Handle<TrayModel>, name: &str) {
    let _ = handle
        .update(|m| m.set_notice(Some(name.to_string()), "The service is not reachable"))
        .await;
}

async fn report(handle: &Handle<TrayModel>, name: &str, r: Result<(), IpcError>) {
    match r {
        Ok(()) => {
            let mount = name.to_string();
            let _ = handle
                .update(move |m| {
                    if m.notice().and_then(|n| n.mount.as_deref()) == Some(mount.as_str()) {
                        m.clear_notice();
                    }
                })
                .await;
        }
        Err(e) => {
            let text = link::from_ipc(e).message();
            tracing::warn!(mount = name, %text, "the service refused");
            let notice = format!("{name}: {text}");
            let mount = name.to_string();
            let _ = handle.update(|m| m.set_notice(Some(mount), notice)).await;
        }
    }
}

/// Hand a mount point to whatever the desktop opens directories with.
async fn open_in_file_manager(handle: &Handle<TrayModel>, path: &str) {
    // `status`, not `output`: xdg-open hands the path to a file manager that inherits its
    // standard error, and a captured pipe would stay open until that program exited.
    let text = match tokio::process::Command::new("xdg-open")
        .arg(path)
        .status()
        .await
    {
        Ok(s) if s.success() => return,
        Ok(s) => format!("xdg-open could not open {path}: {s}"),
        Err(e) => format!("could not run xdg-open: {e}"),
    };
    say(handle, text).await;
}

/// Start or stop the service's unit.
async fn unit(handle: &Handle<TrayModel>, verb: &str) {
    // `output` here, because systemd's refusals are only in its standard error — an exit
    // status of 5 says nothing, "Unit ... not found" says what to fix. systemctl writes a
    // line or two and exits, so nothing is waiting on a pipe a grandchild holds open.
    let text = match tokio::process::Command::new("systemctl")
        .args(["--user", verb, link::UNIT])
        .output()
        .await
    {
        Ok(o) if o.status.success() => return,
        Ok(o) => match complaint(&o.stderr) {
            Some(line) => format!("Could not {verb} the service: {line}"),
            None => format!(
                "Could not {verb} the service: systemctl exited with {}",
                o.status
            ),
        },
        Err(e) => format!("could not run systemctl: {e}"),
    };
    say(handle, text).await;
}

/// The last thing a program said before failing, if it said anything.
///
/// Capped, because this becomes a menu row: systemd is terse, but nothing here guarantees
/// that of whatever else may end up being run.
fn complaint(stderr: &[u8]) -> Option<String> {
    const CAP: usize = 200;
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(CAP) {
        Some((cut, _)) => format!("{} …", &line[..cut]),
        None => line.to_string(),
    })
}

async fn say(handle: &Handle<TrayModel>, text: String) {
    tracing::warn!(%text, "an action did not complete");
    let _ = handle.update(|m| m.set_notice(None, text)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{recorder, serve};

    /// The one destructive thing this crate can ask for, and the one argument that makes it
    /// destructive. Driven over a real socket so the value on the wire is what is asserted.
    #[tokio::test]
    async fn taking_a_mount_down_from_the_menu_never_forces_it() {
        let (iface, calls) = recorder();
        let (_server, client) = serve(iface).await;
        let proxy = link::open(&client)
            .await
            .expect("the handshake is answered");

        call_unmount(&proxy, "photos").await.expect("accepted");
        call_mount(&proxy, "photos").await.expect("accepted");

        assert_eq!(
            *calls.lock().unwrap(),
            ["unmount photos force=false", "mount photos"]
        );
    }

    #[test]
    fn a_failing_program_is_quoted_rather_than_reduced_to_its_exit_status() {
        // What systemd writes when the unit is not installed. "exited with 5" sends the
        // reader to the journal; this sentence says what to fix.
        let said = "Failed to start rclone-vfsmount-trayd.service: Unit rclone-vfsmount-trayd.service not found.";
        assert_eq!(
            complaint(format!("{said}\n").as_bytes()).as_deref(),
            Some(said)
        );
    }

    #[test]
    fn the_last_thing_said_is_the_one_shown() {
        assert_eq!(
            complaint(b"warming up\nthe real problem\n\n").as_deref(),
            Some("the real problem")
        );
        assert_eq!(complaint(b"").as_deref(), None);
        assert_eq!(complaint(b"   \n \n").as_deref(), None);
    }

    #[test]
    fn a_program_that_will_not_stop_talking_does_not_become_the_whole_menu() {
        let long = "x".repeat(500);
        let got = complaint(long.as_bytes()).expect("something was said");
        assert!(got.chars().count() < 210, "{} chars", got.chars().count());
        assert!(got.ends_with('…'));
    }

    #[test]
    fn invalid_utf8_is_still_readable_rather_than_dropped() {
        assert!(complaint(&[0xff, b'h', b'i']).is_some());
    }
}
