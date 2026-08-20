//! The menu the panel shows.
//!
//! Built fresh from [`TrayModel`] every time `ksni` asks, and never doing any work of its
//! own: a click sends an [`Action`] and returns, so nothing the service is slow to answer
//! can hold the menu open (#52).

use ksni::menu::{StandardItem, SubMenu};
use ksni::MenuItem;
use rvt_core::capabilities::Tier;
use rvt_core::ipc::MountView;
use rvt_core::transfer::TransferState;

use crate::model::{
    human_bytes, pending_phrase, wrap, Action, Down, Link, Notice, ServiceInfo, TrayModel,
};

/// Files listed per mount before the rest are counted instead.
///
/// A write-back queue runs to thousands of entries during a large copy, and a menu with one
/// item per file is one the panel cannot draw and nobody can read (#27).
const FILE_CAP: usize = 10;

/// Columns a row is wrapped to, and the rows one piece of text may take.
///
/// A DBusMenu item is drawn on one line and does not wrap, and much of what reaches a label
/// here is a whole paragraph: rclone's stderr as a mount's failure reason, or the service's
/// busy refusal, which names the command that finds the process holding the mount. Both run
/// to hundreds of characters, so every label goes through [`text`], which breaks it up.
const ROW_WIDTH: usize = 64;
const ROW_LINES: usize = 8;

/// Rows kept for each block of text that varies in length.
///
/// A block that grew and shrank with what it had to say would change the menu's shape, and
/// `ksni` answers a shape change by invalidating every menu id. Each is padded to its size
/// instead, so the text arriving or going is a property change the panel absorbs.
/// A failure reason is the unit's recent journal, so systemd's own narration is mixed into
/// it; the line that says what went wrong is near the top, and eight rows is what it takes to
/// reach the end of it. A refusal from the service runs to about the same, and its last
/// sentence — the command that names the process holding a mount — is the actionable one.
const REASON_ROWS: usize = 8;
const DEGRADED_ROWS: usize = 3;
const NOTICE_ROWS: usize = 8;

/// What the menu says Quit does, spelled out where the reflex click is: quitting the tray
/// leaves every mount serving. See DESIGN.md, "The lifetime rule".
const QUIT_LABEL: &str = "Quit (mounts stay up)";

pub(crate) fn build(m: &TrayModel) -> Vec<MenuItem<TrayModel>> {
    match m.link() {
        Link::Connecting => {
            // The same denial the disconnected menu carries: this shows the rows cleared,
            // and can stand for as long as a new service instance takes to answer.
            let mut items = text("Connecting to the service…");
            items.extend(text(
                "Mounts already up are unaffected — this is only the tray's link.",
            ));
            items.push(MenuItem::Separator);
            items.push(quit());
            items
        }
        Link::Down(down) => unreachable_service(down, m.notice()),
        Link::Up(info) => connected(m, info),
    }
}

/// The menu for a service the tray cannot see.
///
/// The second line is not decoration. A tray that has lost its service knows nothing about
/// the mounts, and the failure this guards against is the user reading an empty tray as an
/// empty mount table and going looking for their files (#52).
fn unreachable_service(down: &Down, notice: Option<&Notice>) -> Vec<MenuItem<TrayModel>> {
    let mut items = Vec::new();
    // A "Start service" that failed reports here, where the button that failed is.
    items.extend(notice_lines(notice));
    items.extend(text(&down.headline));
    items.extend(text(
        "Mounts already up are unaffected — this is only the tray's link.",
    ));
    items.push(MenuItem::Separator);
    if down.offer_start {
        items.push(action("Start service", Action::StartService));
    }
    items.push(action("Retry now", Action::Refresh));
    items.push(MenuItem::Separator);
    items.push(quit());
    items
}

fn connected(m: &TrayModel, info: &ServiceInfo) -> Vec<MenuItem<TrayModel>> {
    let mut items = notice_lines(m.notice());

    let summary = m.summary();
    // Four rows, always, some of them hidden. This block changes with all but every poll
    // while an upload runs — a rate that comes and goes, a total that empties — and a row
    // appearing or disappearing is a layout change, which throws away every menu id and makes
    // the panel refetch. Kept a fixed shape, those are property changes instead.
    items.push(one_row(summary.mounted_line()));
    items.push(row_or_gap(summary.pending_line()));
    items.push(row_or_gap(summary.rate_line()));
    items.push(row_or_gap(summary.dirty_on_disk_only.then(|| {
        "Some figures are files on disk, not live upload progress.".to_string()
    })));

    let (managed, foreign): (Vec<_>, Vec<_>) = m.mounts().partition(|v| v.managed);

    if !managed.is_empty() {
        items.push(MenuItem::Separator);
        items.extend(managed.iter().map(|v| mount_item(m, v)));
    }
    if !foreign.is_empty() {
        items.push(MenuItem::Separator);
        items.extend(text("Started outside this service"));
        items.extend(foreign.iter().map(|v| mount_item(m, v)));
    }

    if !managed.is_empty() {
        items.push(MenuItem::Separator);
        items.push(bulk(
            "Mount all",
            true,
            managed.iter().any(|v| settled(v) && !v.live),
        ));
        items.push(bulk(
            "Unmount all",
            false,
            managed.iter().any(|v| settled(v) && v.live),
        ));
    }

    items.push(MenuItem::Separator);
    items.push(action("Refresh", Action::Refresh));
    items.push(about(info));
    items.push(stop_service());
    items.push(quit());
    items
}

/// One mount: what it is, what can be done to it, and what it still has to upload.
fn mount_item(m: &TrayModel, v: &MountView) -> MenuItem<TrayModel> {
    let mut sub = vec![one_row(
        v.mount_point
            .as_deref()
            .unwrap_or("no mount point recorded"),
    )];
    sub.push(row_or_gap(v.remote.clone()));
    // #26: a failed mount says why here rather than only that it failed. rclone's stderr
    // reaches this, so it is several rows more often than one.
    sub.extend(block(
        v.reason.as_ref().map(|r| format!("Failed: {r}")),
        REASON_ROWS,
    ));

    sub.push(MenuItem::Separator);
    match v.mount_point.as_deref() {
        Some(p) if v.live => sub.push(action("Open", Action::Open(p.to_string()))),
        _ => sub.push(disabled_action("Open")),
    }
    if v.managed {
        // Two items, exactly one of them usable, rather than one that changes its verb.
        sub.push(verb(v.name.clone(), Verb::Mount, settled(v) && !v.live));
        sub.push(verb(v.name.clone(), Verb::Unmount, settled(v) && v.live));
    } else {
        // The service refuses to act on a mount it did not start, so offering the action here
        // would only produce a refusal the user cannot do anything about. Two rows either
        // way, so the group a mount is in does not change the menu's shape.
        sub.push(one_row("Not managed by this service"));
        sub.push(hidden());
    }

    sub.push(MenuItem::Separator);
    sub.extend(transfer_lines(m.transfer(&v.name)));

    submenu(format!("{} — {}", v.name, v.state), sub)
}

/// What one mount still has to upload, rendered only as far as its tier allows.
///
/// Always the same number of rows: one headline, [`FILE_CAP`] file slots, the overflow line,
/// three flags, and the reason it cannot see more. Anything with nothing to say is a hidden
/// row holding its place.
fn transfer_lines(t: Option<&TransferState>) -> Vec<MenuItem<TrayModel>> {
    let mut rows = vec![one_row(match t {
        None => "No upload information yet".to_string(),
        Some(t) => pending_headline(t),
    })];

    for slot in 0..FILE_CAP {
        rows.push(row_or_gap(t.and_then(|t| {
            t.files.get(slot).map(|f| file_line(f, t.has_progress))
        })));
    }
    rows.push(row_or_gap(t.and_then(|t| {
        t.files
            .len()
            .checked_sub(FILE_CAP)
            .filter(|rest| *rest > 0)
            .map(|rest| format!("and {rest} more…"))
    })));
    rows.push(row_or_gap(t.and_then(|t| {
        t.errored_files
            .filter(|n| *n > 0)
            .map(|n| format!("{n} file(s) failed to upload"))
    })));
    rows.push(row_or_gap(
        t.filter(|t| t.out_of_space == Some(true))
            .map(|_| "The cache is out of space".to_string()),
    ));
    rows.push(row_or_gap(t.filter(|t| t.fidelity == Some(Tier::T4)).map(
        |_| "Showing files on disk, not live upload progress.".to_string(),
    )));
    rows.extend(block(
        t.and_then(|t| t.degraded_reason.clone()),
        DEGRADED_ROWS,
    ));
    rows
}

/// What is outstanding for one mount, in one line.
fn pending_headline(t: &TransferState) -> String {
    let phrase = pending_phrase(
        t.pending.files,
        t.pending.known_bytes,
        t.pending.unknown_size_files,
    );
    match (t.outstanding_known, t.pending.files) {
        (true, 0) => "All synced".to_string(),
        (true, _) => phrase,
        // Whatever was seen is real; what is missing is any assurance it is all of it. A bare
        // "unknown" throws away entries the rows below go on to list.
        (false, 0) => "Pending: unknown".to_string(),
        (false, _) => format!("{phrase}, and possibly more"),
    }
}

fn file_line(f: &rvt_core::transfer::TransferFile, has_progress: bool) -> String {
    let mut line = f.name.clone();
    if let Some(size) = f.size {
        line.push_str(&format!(" — {}", human_bytes(size)));
    }
    // `in_flight` is `None` at the tiers that cannot tell a queued file from a sending one,
    // and an absent flag must not be rendered as "queued".
    if f.in_flight == Some(true) {
        line.push_str(" (uploading)");
    }
    // Per-file progress exists only where the queue was joined to rclone's accounting;
    // `has_progress` is what says the join found anything.
    if has_progress {
        if let (Some(sent), Some(size)) = (f.bytes_sent, f.size) {
            if size > 0 {
                line.push_str(&format!(" — {} sent", human_bytes(sent)));
            }
        }
    }
    line
}

fn about(info: &ServiceInfo) -> MenuItem<TrayModel> {
    let mut items = text(format!("Tray {}", env!("CARGO_PKG_VERSION")));
    items.extend(text(format!("Service {}", info.service_version)));
    items.extend(text(format!(
        "Interface {} (this client speaks {})",
        info.interface_version,
        rvt_core::ipc::INTERFACE_VERSION
    )));
    items.extend(text(format!("rclone {}", info.rclone_version)));
    items.extend(text(format!("Capability tier {}", info.capability_tier)));
    submenu("About", items)
}

/// Stopping the service is a submenu rather than an item, so it cannot be hit by reflex on
/// the way to Quit (#26).
fn stop_service() -> MenuItem<TrayModel> {
    let mut items =
        text("Mounts stay up unless the service is configured to unmount when it stops.");
    items.extend(text("The tray will show the service as unreachable."));
    items.push(MenuItem::Separator);
    items.push(action("Yes, stop the service", Action::StopService));
    submenu("Stop service", items)
}

/// Which of the two things a menu item asks for. Fixed when the item is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Mount,
    Unmount,
}

/// One item that only ever asks for one thing.
///
/// `ksni` replaces every stored callback each time the tray is updated and dispatches a click
/// to whichever callback now sits at the clicked index, while the panel is still drawing the
/// labels it last fetched. A single item that swapped between "Mount" and "Unmount" would
/// therefore run the opposite of what was clicked whenever the mount changed state between
/// the draw and the click. Two items keep each index's meaning fixed, and keep the row the
/// same shape as the mount goes up and down, so the panel's ids stay valid across it.
fn verb(name: String, verb: Verb, enabled: bool) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(match verb {
            Verb::Mount => "Mount",
            Verb::Unmount => "Unmount",
        }),
        enabled,
        activate: Box::new(move |m: &mut TrayModel| {
            // The model can move between a callback being stored and being called: the tray
            // is edited under one lock and redrawn under another. A row that has gone, or a
            // mount already in the state being asked for, means there is nothing to do.
            let Some(state) = m
                .mounts()
                .find(|v| v.name == name)
                .map(|v| (v.live, settled(v)))
            else {
                return;
            };
            // Disabling an item is a request to the panel, not a guarantee: a click sent
            // against an earlier layout still arrives, so the callback re-checks.
            let (live, settled) = state;
            if !settled {
                return;
            }
            match verb {
                Verb::Mount if !live => m.act(Action::Mount(name.clone())),
                Verb::Unmount if live => m.act(Action::Unmount(name.clone())),
                _ => {}
            }
        }),
        ..Default::default()
    }
    .into()
}

/// Whether a mount is somewhere it can be asked to leave.
///
/// `mounting` and `unmounting` are both `live == false`, and acting on either means asking
/// for something already under way: a second `mount` that queues behind the first, or — after
/// "Unmount all" — a "Mount all" that brings straight back up what was just taken down.
///
/// A state name this build does not know counts as settled, where the rest of the tray treats
/// an unknown name as no answer. That is deliberate: `Live` and `Managed` travel beside the
/// name so a client one release behind can still act (`rvt_core::ipc`), and refusing here
/// would leave such a mount with no working item at all. The cost of being wrong is a request
/// for something already under way, which the service serialises and answers.
fn settled(v: &MountView) -> bool {
    !matches!(v.state.as_str(), "mounting" | "unmounting")
}

/// Mount or unmount every managed mount that is not already in that state, decided when the
/// item is clicked rather than when the menu was drawn.
fn bulk(label: &str, mount: bool, enabled: bool) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(label),
        enabled,
        activate: Box::new(move |m: &mut TrayModel| {
            let names: Vec<String> = m
                .mounts()
                .filter(|v| v.managed && settled(v) && v.live != mount)
                .map(|v| v.name.clone())
                .collect();
            for name in names {
                m.act(if mount {
                    Action::Mount(name)
                } else {
                    Action::Unmount(name)
                });
            }
        }),
        ..Default::default()
    }
    .into()
}

fn quit() -> MenuItem<TrayModel> {
    action(QUIT_LABEL, Action::Quit)
}

/// The last thing that went wrong, broken across rows and followed by a separator.
fn notice_lines(notice: Option<&Notice>) -> Vec<MenuItem<TrayModel>> {
    let mut items = block(notice.map(|n| n.text.clone()), NOTICE_ROWS);
    // A separator only when there is something above it to separate. `ksni` diffs an item's
    // type along with its other properties, so swapping the two is a property change and the
    // menu keeps its shape either way.
    items.push(if notice.is_some() {
        MenuItem::Separator
    } else {
        hidden()
    });
    items
}

/// Text to read rather than click, as however many rows it takes to be readable.
///
/// Returns rows rather than one item so that no caller can hand a paragraph to a menu that
/// draws it on a single line. Our own prose is short and comes back as one.
fn text(label: impl Into<String>) -> Vec<MenuItem<TrayModel>> {
    wrap(&label.into(), ROW_WIDTH, ROW_LINES)
        .into_iter()
        .map(|line| {
            StandardItem {
                label: escape(&line),
                enabled: false,
                ..Default::default()
            }
            .into()
        })
        .collect()
}

fn action(label: impl Into<String>, a: Action) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(&label.into()),
        activate: Box::new(move |m: &mut TrayModel| m.act(a.clone())),
        ..Default::default()
    }
    .into()
}

/// An action that is not available now, kept in place so the menu does not change shape
/// under the cursor as mounts come and go.
fn disabled_action(label: &str) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(label),
        enabled: false,
        ..Default::default()
    }
    .into()
}

/// One row exactly, whatever it holds. For a list whose length has to stay fixed.
fn one_row(label: impl Into<String>) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(&clip(&label.into())),
        enabled: false,
        ..Default::default()
    }
    .into()
}

/// Exactly `rows` rows: what there is to say, wrapped, and hidden rows for the rest.
fn block(label: Option<String>, rows: usize) -> Vec<MenuItem<TrayModel>> {
    let mut out: Vec<MenuItem<TrayModel>> = label
        .map(|l| wrap(&l, ROW_WIDTH, rows).into_iter().map(one_row).collect())
        .unwrap_or_default();
    out.resize_with(rows, hidden);
    out
}

/// One row if there is something to say, and a placeholder if not.
fn row_or_gap(label: Option<String>) -> MenuItem<TrayModel> {
    match label {
        Some(l) => one_row(l),
        None => hidden(),
    }
}

/// A row that holds a place in the layout without being drawn.
fn hidden() -> MenuItem<TrayModel> {
    StandardItem {
        visible: false,
        ..Default::default()
    }
    .into()
}

/// A submenu's label is one row that cannot be broken up, so it is cut instead.
fn submenu(label: impl Into<String>, items: Vec<MenuItem<TrayModel>>) -> MenuItem<TrayModel> {
    SubMenu {
        label: escape(&clip(&label.into())),
        submenu: items,
        ..Default::default()
    }
    .into()
}

/// Cut a label that has to fit one row, marking that it was cut.
fn clip(s: &str) -> String {
    match s.char_indices().nth(ROW_WIDTH) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

/// Protect a label from being read as a mnemonic.
///
/// DBusMenu strips a single underscore and turns a doubled one back into a single, so a
/// mount called `my_files` reaches the panel as "myfiles" unless it is doubled here. Applied
/// wherever a label is built rather than at each call site: most of what reaches one is a
/// mount name, a path, or a sentence from rclone, and the rest is our own prose, which has no
/// underscores to double.
fn escape(s: &str) -> String {
    s.replace('_', "__")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{blank, connected, idle_transfer, mount, pending};
    use crate::link::LinkError;
    use rvt_core::ipc::TransferFileView;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Every label in the tree, submenus included. Separators have none and are skipped.
    fn labels(items: &[MenuItem<TrayModel>]) -> Vec<String> {
        let mut out = Vec::new();
        walk(items, &mut |item| match item {
            MenuItem::Standard(s) => out.push(s.label.clone()),
            MenuItem::SubMenu(s) => out.push(s.label.clone()),
            _ => {}
        });
        out
    }

    fn walk(items: &[MenuItem<TrayModel>], f: &mut impl FnMut(&MenuItem<TrayModel>)) {
        for item in items {
            f(item);
            if let MenuItem::SubMenu(s) = item {
                walk(&s.submenu, f);
            }
        }
    }

    /// The one item with this label. Panics rather than returning an option: a test asking
    /// about an item that is not there has already failed.
    fn item(items: &[MenuItem<TrayModel>], label: &str) -> StandardItem<TrayModel> {
        let mut found = None;
        walk(items, &mut |i| {
            if let MenuItem::Standard(s) = i {
                if s.label == label {
                    found = Some(StandardItem {
                        label: s.label.clone(),
                        enabled: s.enabled,
                        activate: Box::new(|_| {}),
                        ..Default::default()
                    });
                }
            }
        });
        found.unwrap_or_else(|| panic!("no item labelled {label:?} in {:?}", labels(items)))
    }

    /// Click the item with this label, and return what the tray was asked to do.
    fn click(m: &mut TrayModel, rx: &mut UnboundedReceiver<Action>, label: &str) -> Vec<Action> {
        let items = build(m);
        let mut fired = false;
        walk(&items, &mut |i| {
            if let MenuItem::Standard(s) = i {
                if s.label == label && !fired {
                    fired = true;
                    // The callback only ever queues, so a click cannot touch the model.
                    (s.activate)(m);
                }
            }
        });
        assert!(fired, "no item labelled {label:?} in {:?}", labels(&items));
        let mut actions = Vec::new();
        while let Ok(a) = rx.try_recv() {
            actions.push(a);
        }
        actions
    }

    fn says(items: &[MenuItem<TrayModel>], needle: &str) -> bool {
        labels(items).iter().any(|l| l.contains(needle))
    }

    #[test]
    fn a_stopped_service_says_so_and_never_claims_there_are_no_mounts() {
        // #52's one rule. The tray knows nothing about the mounts here, and an empty list —
        // or a count of zero — would be a claim it cannot make.
        let (mut m, _rx) = blank();
        m.go_down(&LinkError::NotRunning);
        let items = build(&m);
        let l = labels(&items);

        assert!(says(&items, "not running"), "{l:?}");
        assert!(says(&items, "unaffected"), "{l:?}");
        assert!(l.contains(&"Start service".to_string()), "{l:?}");
        assert!(
            !l.iter()
                .any(|s| s.contains(" mounted") || s.contains("synced")),
            "the menu must not describe mount state it cannot see: {l:?}"
        );
    }

    #[test]
    fn a_version_mismatch_is_named_and_offers_nothing_that_would_not_help() {
        let (mut m, _rx) = blank();
        m.go_down(&LinkError::TooOld {
            needed: 2,
            found: 1,
        });
        let items = build(&m);
        assert!(says(&items, "too old"), "{:?}", labels(&items));
        assert!(
            !labels(&items).contains(&"Start service".to_string()),
            "starting a service that is already running fixes nothing"
        );
    }

    #[test]
    fn a_failed_start_reports_where_the_button_that_failed_is() {
        let (mut m, _rx) = blank();
        m.go_down(&LinkError::NotRunning);
        m.set_notice(
            None,
            "could not run systemctl: No such file or directory",
            None,
        );
        assert!(says(&build(&m), "could not run systemctl"));
    }

    #[test]
    fn a_refusal_is_broken_across_rows_rather_than_shown_as_one_long_one() {
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        m.set_notice(
            Some("photos".into()),
            "photos: /mnt/photos could not be unmounted: Device or resource busy. \
             Usually a process is still using the mount — a file open under it, or a \
             shell whose working directory is inside it.",
            Some(false),
        );
        let items = build(&m);
        assert!(says(&items, "could not be unmounted"));
        assert!(
            labels(&items)
                .iter()
                .all(|l| l.chars().count() <= ROW_WIDTH + 2),
            "a row wider than the panel: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn an_underscore_in_a_name_reaches_the_panel_intact() {
        // DBusMenu eats a single underscore as a mnemonic marker, so `my_files` would be
        // drawn as "myfiles" unless it is doubled on the way out.
        let (m, _rx) = connected(vec![mount("my_files", "mounted")], vec![]);
        assert!(says(&build(&m), "my__files"), "{:?}", labels(&build(&m)));
    }

    #[test]
    fn open_is_offered_only_for_a_mount_that_is_serving() {
        let (up, _a) = connected(vec![mount("photos", "mounted")], vec![]);
        assert!(item(&build(&up), "Open").enabled);

        let (down, _b) = connected(vec![mount("photos", "unmounted")], vec![]);
        assert!(!item(&build(&down), "Open").enabled);
    }

    #[test]
    fn a_mount_this_service_did_not_start_is_set_apart_and_offers_no_actions() {
        let (m, _rx) = connected(
            vec![mount("photos", "mounted"), mount("elsewhere", "foreign")],
            vec![],
        );
        let items = build(&m);
        assert!(says(&items, "Started outside this service"));
        assert!(says(&items, "Not managed by this service"));
        // One Unmount, for the one mount the service owns.
        assert_eq!(labels(&items).iter().filter(|l| *l == "Unmount").count(), 1);
    }

    #[test]
    fn a_failed_mount_says_why_rather_than_only_that_it_failed() {
        let mut failed = mount("photos", "failed");
        failed.reason = Some("rclone exited before the mount appeared".into());
        let (m, _rx) = connected(vec![failed], vec![]);
        assert!(says(&build(&m), "rclone exited before the mount appeared"));
    }

    #[test]
    fn a_long_queue_is_capped_and_says_how_many_it_left_out() {
        // #27: a write-back queue runs to thousands during a large copy, and one menu item
        // per file is a menu the panel cannot draw.
        let mut t = idle_transfer("photos");
        pending(&mut t, 25, 25_000, 0);
        t.files = (0..25)
            .map(|i| TransferFileView {
                name: format!("clip{i}.mp4"),
                size: Some(1000),
                in_flight: Some(i == 0),
                tries: Some(1),
                bytes_sent: None,
            })
            .collect();

        let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
        let items = build(&m);
        let listed = labels(&items)
            .iter()
            .filter(|l| l.starts_with("clip"))
            .count();
        assert_eq!(listed, FILE_CAP);
        assert!(says(&items, &format!("and {} more", 25 - FILE_CAP)));
        assert!(says(&items, "clip0.mp4 — 1000 B (uploading)"));
    }

    #[test]
    fn a_file_the_tier_cannot_place_is_not_claimed_to_be_uploading() {
        // A cache walk cannot tell a queued file from one being sent, and says so with an
        // absent flag. Rendering that as "queued" or "uploading" would be an invention.
        let mut t = idle_transfer("photos");
        t.fidelity = Some("T4".into());
        pending(&mut t, 1, 1024, 0);
        t.files = vec![TransferFileView {
            name: "clip.mp4".into(),
            size: Some(1024),
            in_flight: None,
            tries: None,
            bytes_sent: None,
        }];

        let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
        let items = build(&m);
        assert!(says(&items, "clip.mp4 — 1 KiB"));
        assert!(!says(&items, "uploading"));
        assert!(!says(&items, "%"), "no percentage at a tier without one");
        assert!(says(&items, "not live upload progress"));
    }

    #[test]
    fn clicking_mount_asks_the_service_for_that_one_mount() {
        let (mut m, mut rx) = connected(vec![mount("photos", "unmounted")], vec![]);
        assert_eq!(
            click(&mut m, &mut rx, "Mount"),
            [Action::Mount("photos".into())]
        );
    }

    #[test]
    fn quit_asks_only_the_tray_to_stop_and_says_so_on_the_item() {
        // DESIGN.md, "The lifetime rule": the item people hit by reflex must not be the one
        // that takes their filesystems away, and must not read as though it might.
        let (mut m, mut rx) = connected(vec![mount("photos", "mounted")], vec![]);
        assert!(QUIT_LABEL.contains("mounts stay up"));
        assert_eq!(click(&mut m, &mut rx, QUIT_LABEL), [Action::Quit]);
    }

    #[test]
    fn stopping_the_service_takes_a_second_click_and_says_what_happens_to_mounts() {
        let (mut m, mut rx) = connected(vec![mount("photos", "mounted")], vec![]);
        let items = build(&m);
        // The top-level item is a submenu, so it cannot be triggered on the way past.
        assert!(items
            .iter()
            .any(|i| matches!(i, MenuItem::SubMenu(s) if s.label == "Stop service")));
        assert!(says(&items, "Mounts stay up"));
        assert_eq!(
            click(&mut m, &mut rx, "Yes, stop the service"),
            [Action::StopService]
        );
    }

    #[test]
    fn mount_all_acts_on_what_is_down_when_it_is_clicked_not_when_it_was_drawn() {
        // A menu can sit open while the service brings a mount up underneath it.
        let (mut m, mut rx) = connected(
            vec![mount("photos", "unmounted"), mount("docs", "unmounted")],
            vec![],
        );
        m.upsert_mount(mount("docs", "mounted"));
        assert_eq!(
            click(&mut m, &mut rx, "Mount all"),
            [Action::Mount("photos".into())]
        );
    }

    #[test]
    fn a_bulk_item_is_usable_only_when_there_is_something_for_it_to_do() {
        // Present either way: an item coming and going is a layout change, and the panel
        // throws away every menu id when the layout changes.
        let (all_up, _a) = connected(vec![mount("photos", "mounted")], vec![]);
        let items = build(&all_up);
        assert!(item(&items, "Unmount all").enabled);
        assert!(!item(&items, "Mount all").enabled);
    }

    #[test]
    fn the_per_mount_line_carries_the_same_qualifier_as_the_summary() {
        // Two renderings of one figure. Without the qualifier the submenu's copy reads as a
        // total, and it sits a click away from the one that says otherwise.
        let mut t = idle_transfer("photos");
        pending(&mut t, 5, 100 * 1024 * 1024, 1);
        let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);

        let lines = labels(&build(&m));
        let pending_lines: Vec<&String> = lines.iter().filter(|l| l.contains("pending")).collect();
        assert_eq!(pending_lines.len(), 2, "{lines:?}");
        assert!(
            pending_lines
                .iter()
                .all(|l| l.contains("1 of unknown size")),
            "{pending_lines:?}"
        );
    }

    /// Whether every underscore in a label survives DBusMenu's mnemonic stripping, which
    /// keeps one of each pair and drops a lone one.
    fn escaped(label: &str) -> bool {
        label
            .split(|c| c != '_')
            .filter(|run| !run.is_empty())
            .all(|run| run.len() % 2 == 0)
    }

    #[test]
    fn every_label_reaches_the_panel_with_its_underscores() {
        let mut failed = mount("my_photos", "failed");
        failed.reason = Some("rclone_mount exited with code 1".into());
        failed.mount_point = Some("/mnt/my_photos".into());
        failed.remote = Some("my_remote:some_path".into());

        let mut t = idle_transfer("my_photos");
        pending(&mut t, 1, 1024, 0);
        t.degraded_reason = Some("no write_back queue on this mount".into());
        t.files = vec![rvt_core::ipc::TransferFileView {
            name: "holiday_2026.mp4".into(),
            size: Some(1024),
            in_flight: Some(true),
            tries: Some(1),
            bytes_sent: None,
        }];

        let (mut m, _rx) = connected(vec![failed], vec![t]);
        m.set_notice(
            Some("my_photos".into()),
            "my_photos: still_in_use",
            Some(false),
        );

        for label in labels(&build(&m)) {
            assert!(escaped(&label), "a lone underscore survives in {label:?}");
        }

        // And the same on the other menu, whose headline can carry an error from zbus.
        m.go_down(&LinkError::Transport(zbus::Error::Failure(
            "peer_disconnected".into(),
        )));
        m.set_notice(None, "could not run systemctl: no_such_unit", None);
        for label in labels(&build(&m)) {
            assert!(escaped(&label), "a lone underscore survives in {label:?}");
        }
    }

    #[test]
    fn the_guard_on_escaping_can_tell_the_difference() {
        // The control: an unescaped name has to fail the check the test above applies.
        assert!(escaped("my__files"));
        assert!(!escaped("my_files"));
        assert!(!escaped("a___b"));
        assert!(escaped("no underscores here"));
    }

    #[test]
    fn a_serving_mount_with_no_reading_says_so_in_the_summary_too() {
        let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        let items = build(&m);
        assert!(says(&items, "unknown"), "{:?}", labels(&items));
        assert!(!says(&items, "All synced"), "{:?}", labels(&items));
    }

    /// A paragraph of the shape the service actually produces: rclone's stderr, or its busy
    /// refusal, which carries the mount point twice and so has a long unbreakable run in it.
    fn a_paragraph() -> String {
        format!(
            "/home/someone/mnt/photos could not be unmounted: fusermount3: failed to unmount \
             /home/someone/mnt/photos: Device or resource busy. Usually a process is still \
             using the mount — a file open under it, or a shell whose working directory is \
             inside it. `fuser -m /home/someone/mnt/photos` names them. {}",
            "supplementary-detail-with-no-spaces-at-all-".repeat(12)
        )
    }

    /// Every label the panel would draw, whatever the tray is showing.
    fn every_label(m: &mut TrayModel) -> Vec<String> {
        let mut all = labels(&build(m));
        m.go_down(&LinkError::NotRunning);
        all.extend(labels(&build(m)));
        all
    }

    /// A label as drawn: DBusMenu turns each doubled underscore back into one. The escaped
    /// string is longer than the row it produces, so the width has to be measured here and
    /// not on what crosses the wire.
    fn drawn(label: &str) -> String {
        label.replace("__", "_")
    }

    #[test]
    fn no_row_is_wider_than_the_panel_can_draw() {
        // A DBusMenu item is one line and does not wrap. A failure reason went out as a
        // single 1728-character label before this held.
        // A long name as well as a long reason: a submenu's label is one row that cannot be
        // broken up, so it has to be cut instead. Underscores throughout, because escaping
        // happens after the measurement and a fixture without them cannot show that.
        let name = "a_mount_with_a_very_long_name".repeat(4);
        let mut failed = mount(&name, "failed");
        failed.reason = Some(a_paragraph().replace('-', "_"));
        failed.mount_point = Some(format!("/mnt/{}", "deep_dir/".repeat(40)));

        let mut t = idle_transfer(&name);
        t.degraded_reason = Some(a_paragraph().replace('-', "_"));

        let (mut m, _rx) = connected(vec![failed], vec![t]);
        m.set_notice(Some(name), a_paragraph().replace('-', "_"), Some(false));

        let all = every_label(&mut m);
        assert!(all.len() > 10, "{all:?}");
        for label in &all {
            let row = drawn(label);
            assert!(
                row.chars().count() <= ROW_WIDTH + 2,
                "{} columns: {row:?}",
                row.chars().count()
            );
        }
    }

    #[test]
    fn a_reason_too_long_to_show_says_it_was_cut() {
        let mut failed = mount("photos", "failed");
        failed.reason = Some(a_paragraph());
        let (m, _rx) = connected(vec![failed], vec![]);
        let all = labels(&build(&m));
        assert!(
            all.iter().any(|l| l.ends_with('…')),
            "a reason shown in part must say so: {all:?}"
        );
    }

    /// Click an item in a menu drawn earlier, which is what a panel holding a click does.
    fn click_stale(
        items: &[MenuItem<TrayModel>],
        label: &str,
        m: &mut TrayModel,
        rx: &mut UnboundedReceiver<Action>,
    ) -> Vec<Action> {
        let mut fired = false;
        walk(items, &mut |i| {
            if let MenuItem::Standard(s) = i {
                if s.label == label && !fired {
                    fired = true;
                    (s.activate)(m);
                }
            }
        });
        assert!(fired, "no item labelled {label:?}");
        let mut out = Vec::new();
        while let Ok(a) = rx.try_recv() {
            out.push(a);
        }
        out
    }

    #[test]
    fn a_click_on_mount_never_takes_a_mount_down() {
        // The panel can hold a click for an item drawn before the mount last changed state,
        // and `ksni` keeps an item's id while the menu's shape is unchanged — which the
        // unmounted and mounted-but-unreadable states of one mount can share. Reading the
        // click as a toggle would unmount a mount that had just come back.
        let (mut m, mut rx) = connected(vec![mount("photos", "unmounted")], vec![]);
        let drawn = build(&m);
        assert!(labels(&drawn).contains(&"Mount".to_string()));

        m.upsert_mount(mount("photos", "mounted"));
        assert_eq!(
            click_stale(&drawn, "Mount", &mut m, &mut rx),
            [],
            "a request the mount already satisfies does nothing"
        );
    }

    #[test]
    fn a_click_on_mount_still_mounts_when_the_mount_is_down() {
        let (mut m, mut rx) = connected(vec![mount("photos", "unmounted")], vec![]);
        assert_eq!(
            click(&mut m, &mut rx, "Mount"),
            [Action::Mount("photos".into())]
        );
    }

    #[test]
    fn a_click_on_unmount_still_unmounts_when_the_mount_is_up() {
        let (mut m, mut rx) = connected(vec![mount("photos", "mounted")], vec![]);
        assert_eq!(
            click(&mut m, &mut rx, "Unmount"),
            [Action::Unmount("photos".into())]
        );
    }

    #[test]
    fn a_click_for_a_mount_that_has_gone_does_nothing() {
        let (mut m, mut rx) = connected(vec![mount("photos", "mounted")], vec![]);
        let drawn = build(&m);
        m.remove_mount("photos");
        assert_eq!(click_stale(&drawn, "Unmount", &mut m, &mut rx), []);
    }

    #[test]
    fn a_count_that_cannot_be_vouched_for_is_shown_as_a_floor_not_dropped() {
        // "Pending: unknown" above a list of the files it says nothing about reads as though
        // the list were something else.
        let mut t = idle_transfer("photos");
        t.outstanding_known = false;
        pending(&mut t, 2, 2048, 0);
        let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
        let all = labels(&build(&m));
        assert!(
            all.iter().any(|l| l.contains("and possibly more")),
            "{all:?}"
        );
    }

    /// Every state name the service's own vocabulary contains.
    const EVERY_STATE: [&str; 7] = [
        "unmounted",
        "mounting",
        "mounted",
        "unmounting",
        "failed",
        "foreign",
        "orphaned",
    ];

    #[test]
    fn a_mount_on_its_way_somewhere_is_not_asked_to_go_there_again() {
        // Both transitional states are `live == false`, so a toggle derived from that alone
        // offers "Mount" to a mount that is already mounting, and — worse — offers "Mount
        // all" to a set of mounts that are in the middle of coming down.
        for busy in ["mounting", "unmounting"] {
            let (mut m, mut rx) = connected(vec![mount("photos", busy)], vec![]);
            let items = build(&m);
            for offered in ["Mount", "Unmount", "Mount all", "Unmount all"] {
                assert!(!item(&items, offered).enabled, "{busy}: {offered}");
                // Disabling is a request to the panel, not a guarantee, so the callbacks
                // have to refuse as well.
                assert_eq!(click(&mut m, &mut rx, offered), [], "{busy}: {offered}");
            }
        }
    }

    #[test]
    fn a_click_does_what_its_own_label_says_and_not_the_other_thing() {
        // `ksni` replaces every callback on each update and dispatches by index, so the two
        // items have to mean one thing each. A single item that swapped verbs would run the
        // opposite of what the panel drew whenever the mount moved in between.
        let (mut m, mut rx) = connected(vec![mount("photos", "unmounted")], vec![]);
        assert_eq!(click(&mut m, &mut rx, "Unmount"), [], "already down");
        assert_eq!(
            click(&mut m, &mut rx, "Mount"),
            [Action::Mount("photos".into())]
        );

        m.upsert_mount(mount("photos", "mounted"));
        assert_eq!(click(&mut m, &mut rx, "Mount"), [], "already up");
        assert_eq!(
            click(&mut m, &mut rx, "Unmount"),
            [Action::Unmount("photos".into())]
        );
    }

    #[test]
    fn a_mounts_row_keeps_its_shape_as_the_mount_comes_and_goes() {
        // `ksni` invalidates every menu id when a node's child count changes, so a row that
        // reshaped as its mount toggled would throw away the click the panel already sent.
        let counts: Vec<usize> = ["unmounted", "mounting", "mounted", "unmounting"]
            .iter()
            .map(|state| {
                let (m, _rx) =
                    connected(vec![mount("photos", state)], vec![idle_transfer("photos")]);
                submenu_rows(&build(&m), "photos")
            })
            .collect();
        assert!(
            counts.windows(2).all(|w| w[0] == w[1]),
            "the row changes shape as the mount moves: {counts:?}"
        );
    }

    #[test]
    fn the_file_list_keeps_its_shape_as_the_queue_drains() {
        let counts: Vec<usize> = [0usize, 1, FILE_CAP, FILE_CAP + 5]
            .iter()
            .map(|n| {
                let mut t = idle_transfer("photos");
                pending(&mut t, *n as u64, 1024 * *n as u64, 0);
                t.files = (0..*n)
                    .map(|i| rvt_core::ipc::TransferFileView {
                        name: format!("clip{i}.mp4"),
                        size: Some(1024),
                        in_flight: Some(false),
                        tries: Some(1),
                        bytes_sent: None,
                    })
                    .collect();
                let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
                submenu_rows(&build(&m), "photos")
            })
            .collect();
        assert!(
            counts.windows(2).all(|w| w[0] == w[1]),
            "the menu reshapes as the queue drains: {counts:?}"
        );
    }

    #[test]
    fn every_state_the_service_can_publish_gets_a_menu() {
        for state in EVERY_STATE {
            let (m, _rx) = connected(vec![mount("photos", state)], vec![idle_transfer("photos")]);
            let l = labels(&build(&m));
            assert!(
                l.iter().any(|row| row.contains(state)),
                "{state} is not named anywhere: {l:?}"
            );
        }
    }

    /// Items in one mount's submenu, hidden ones included: what `ksni` counts when deciding
    /// whether the layout changed, and so whether every menu id is thrown away.
    fn submenu_rows(items: &[MenuItem<TrayModel>], name: &str) -> usize {
        let mut n = None;
        walk(items, &mut |i| {
            if let MenuItem::SubMenu(s) = i {
                if s.label.starts_with(name) {
                    n = Some(s.submenu.len());
                }
            }
        });
        n.unwrap_or_else(|| panic!("no submenu for {name:?}"))
    }

    #[test]
    fn the_whole_menu_keeps_its_shape_while_an_upload_runs() {
        // The summary block used to gain and lose rows with every poll: a rate that comes and
        // goes, a total that empties. Each of those threw away every menu id.
        let shapes: Vec<usize> = [
            (0u64, None),
            (3, Some(4 * 1024 * 1024)),
            (3, None),
            (1, Some(1024)),
            (0, None),
        ]
        .iter()
        .map(|(files, rate)| {
            let mut t = idle_transfer("photos");
            pending(&mut t, *files, 1024 * *files, 0);
            t.rate_bytes_per_sec = *rate;
            t.files = (0..*files)
                .map(|i| rvt_core::ipc::TransferFileView {
                    name: format!("clip{i}.mp4"),
                    size: Some(1024),
                    in_flight: Some(i == 0),
                    tries: Some(1),
                    bytes_sent: None,
                })
                .collect();
            let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
            let mut n = 0;
            walk(&build(&m), &mut |_| n += 1);
            n
        })
        .collect();
        assert!(
            shapes.windows(2).all(|w| w[0] == w[1]),
            "the menu reshapes as an upload progresses: {shapes:?}"
        );
    }

    /// Every value the menu branches on, crossed. Building this by hand is what let two
    /// defects through before: the assertions were sound and the inputs never reached them.
    fn every_shape() -> Vec<(
        String,
        TrayModel,
        tokio::sync::mpsc::UnboundedReceiver<Action>,
    )> {
        let mut out = Vec::new();
        for state in EVERY_STATE {
            for files in [0usize, 1, FILE_CAP, FILE_CAP + 3] {
                for (tier, known, progress) in [
                    ("T2", true, false),
                    ("T2", true, true),
                    ("T3", true, false),
                    ("T4", false, false),
                ] {
                    for (errored, full, why) in [
                        (0u64, false, None),
                        (
                            3,
                            true,
                            Some("rclone_unreachable: no rc socket at /run/x_y".to_string()),
                        ),
                    ] {
                        let mut r = idle_transfer("my_pics");
                        r.fidelity = Some(tier.into());
                        r.outstanding_known = known;
                        r.has_progress = progress;
                        r.errored_files = Some(errored);
                        r.out_of_space = Some(full);
                        r.degraded_reason = why.clone();
                        pending(&mut r, files as u64, 1024 * files as u64, 0);
                        r.files = (0..files)
                            .map(|i| rvt_core::ipc::TransferFileView {
                                name: format!("holiday_{i}_of_{files}.mp4"),
                                size: Some(1024),
                                in_flight: [Some(true), Some(false), None][i % 3],
                                tries: Some(1),
                                bytes_sent: progress.then_some(512),
                            })
                            .collect();
                        let (m, rx) = connected(vec![mount("my_pics", state)], vec![r]);
                        out.push((
                            format!("{state}/{files} files/{tier}/errored={errored}"),
                            m,
                            rx,
                        ));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn one_mount_draws_the_same_number_of_rows_whatever_it_is_doing() {
        // `ksni` throws away every menu id when a node's child count changes, so the only
        // thing that may reshape the menu is a mount appearing or going away.
        let mut seen: Vec<(String, usize)> = Vec::new();
        for (what, m, _rx) in every_shape() {
            seen.push((what, submenu_rows(&build(&m), "my__pics")));
        }
        let first = seen[0].1;
        let odd: Vec<&(String, usize)> = seen.iter().filter(|(_, n)| *n != first).collect();
        assert!(
            odd.is_empty(),
            "{} of {} combinations reshape the row (expected {first}): {:?}",
            odd.len(),
            seen.len(),
            &odd[..odd.len().min(6)]
        );
    }

    #[test]
    fn a_failed_mount_shows_the_line_that_says_what_went_wrong() {
        // Captured from a real failure — a mount point that is not empty, the commonest way a
        // mount fails on a fresh setup. Kept at full length, paths included: shortening them
        // is what makes this fit in four rows, and four rows was the defect.
        let mut failed = mount("photos", "failed");
        failed.reason = Some(
            "Started rvt-mount-bad.service - rclone mount r6src: at \
             /home/claude/.claude/jobs/f70669bc/tmp/r6/mnt.\n\
             ERROR+4: Fatal error: failed to mount FUSE fs: \
             \"/home/claude/.claude/jobs/f70669bc/tmp/r6/mnt\" is not empty, use \
             --allow-non-empty to mount anyway\n\
             rvt-mount-bad.service: Main process exited, code=exited, status=1/FAILURE\n\
             rvt-mount-bad.service: Failed with result 'exit-code'.\n\
             rvt-mount-bad.service: Scheduled restart job, restart counter is at 1."
                .to_string(),
        );
        let (m, _rx) = connected(vec![failed], vec![]);
        let drawn = labels(&build(&m)).join(" ");
        assert!(
            drawn.contains("use --allow-non-empty"),
            "the cause was cut off: {drawn}"
        );
    }

    #[test]
    fn a_busy_unmount_keeps_the_sentence_that_says_what_to_do() {
        // The service's refusal ends with the command that names the process holding the
        // mount; cutting the message short drops exactly that.
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        // The service's own sentence, with a mount point of the length people really have.
        m.set_notice(
            Some("photos".into()),
            "photos: /home/claude/media/backups/photos could not be unmounted: fusermount3: \
             failed to unmount /home/claude/media/backups/photos: Device or resource busy. \
             Usually a process is still using the mount — a file open under it, or a shell \
             whose working directory is inside it. \
             `fuser -m /home/claude/media/backups/photos` names them. Unmounting anyway cuts \
             anything mid-write off, and rclone then uploads the partial file as if it were \
             complete.",
            Some(false),
        );
        let drawn = labels(&build(&m)).join(" ");
        assert!(drawn.contains("names them"), "the advice was cut: {drawn}");
    }

    #[test]
    fn the_connecting_menu_denies_the_no_mounts_reading_too() {
        // It shows the rows cleared, and stands for as long as a new instance takes to
        // answer the handshake — up to the attach timeout.
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        m.go_connecting();
        let items = build(&m);
        assert!(says(&items, "unaffected"), "{:?}", labels(&items));
        assert!(!says(&items, "mounted"), "{:?}", labels(&items));
    }
}
