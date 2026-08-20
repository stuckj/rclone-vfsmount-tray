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

/// Columns a notice is wrapped to, and the rows it may take.
///
/// The service's refusals are whole paragraphs — the busy-mount one names the command that
/// finds the process holding it — and a menu row does not wrap.
const NOTICE_WIDTH: usize = 64;
const NOTICE_LINES: usize = 5;

/// What the menu says Quit does, spelled out where the reflex click is: quitting the tray
/// leaves every mount serving. See DESIGN.md, "The lifetime rule".
const QUIT_LABEL: &str = "Quit (mounts stay up)";

pub(crate) fn build(m: &TrayModel) -> Vec<MenuItem<TrayModel>> {
    match m.link() {
        Link::Connecting => vec![
            text("Connecting to the service…"),
            MenuItem::Separator,
            quit(),
        ],
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
    items.push(text(&down.headline));
    items.push(text(
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
    items.push(text(summary.mounted_line()));
    items.extend(summary.pending_line().map(text));
    items.extend(summary.rate_line().map(text));
    if summary.dirty_on_disk_only {
        items.push(text(
            "Some figures are unuploaded files on disk, not live upload progress.",
        ));
    }

    let (managed, foreign): (Vec<_>, Vec<_>) = m.mounts().partition(|v| v.managed);

    if !managed.is_empty() {
        items.push(MenuItem::Separator);
        items.extend(managed.iter().map(|v| mount_item(m, v)));
    }
    if !foreign.is_empty() {
        items.push(MenuItem::Separator);
        items.push(text("Started outside this service"));
        items.extend(foreign.iter().map(|v| mount_item(m, v)));
    }

    if !managed.is_empty() {
        items.push(MenuItem::Separator);
    }
    if managed.iter().any(|v| !v.live) {
        items.push(bulk("Mount all", true));
    }
    if managed.iter().any(|v| v.live) {
        items.push(bulk("Unmount all", false));
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
    let mut sub = vec![text(
        v.mount_point
            .as_deref()
            .unwrap_or("no mount point recorded"),
    )];
    if let Some(remote) = &v.remote {
        sub.push(text(remote));
    }
    // #26: a failed mount says why here rather than only that it failed.
    if let Some(reason) = &v.reason {
        sub.push(text(format!("Failed: {reason}")));
    }

    sub.push(MenuItem::Separator);
    match v.mount_point.as_deref() {
        Some(p) if v.live => sub.push(action("Open", Action::Open(p.to_string()))),
        _ => sub.push(disabled_action("Open")),
    }
    if v.managed {
        let name = v.name.clone();
        sub.push(if v.live {
            action("Unmount", Action::Unmount(name))
        } else {
            action("Mount", Action::Mount(name))
        });
    } else {
        // The service refuses to act on a mount it did not start, so offering the action
        // here would only produce a refusal the user cannot do anything about.
        sub.push(text("Not managed by this service"));
    }

    sub.push(MenuItem::Separator);
    sub.extend(transfer_lines(m.transfer(&v.name)));

    submenu(format!("{} — {}", v.name, v.state), sub)
}

/// What one mount still has to upload, rendered only as far as its tier allows.
fn transfer_lines(t: Option<&TransferState>) -> Vec<MenuItem<TrayModel>> {
    let Some(t) = t else {
        return vec![text("No upload information yet")];
    };

    let mut lines = Vec::new();
    if !t.outstanding_known {
        lines.push(text("Pending: unknown"));
    } else if t.pending.files == 0 {
        lines.push(text("All synced"));
    } else {
        lines.push(text(pending_phrase(
            t.pending.files,
            t.pending.known_bytes,
            t.pending.unknown_size_files,
        )));
    }

    for f in t.files.iter().take(FILE_CAP) {
        let mut line = f.name.clone();
        if let Some(size) = f.size {
            line.push_str(&format!(" — {}", human_bytes(size)));
        }
        // `in_flight` is `None` at the tiers that cannot tell a queued file from a sending
        // one, and an absent flag must not be rendered as "queued".
        if f.in_flight == Some(true) {
            line.push_str(" (uploading)");
        }
        // Per-file progress exists only where the queue was joined to rclone's accounting;
        // `has_progress` is what says the join found anything.
        if t.has_progress {
            if let (Some(sent), Some(size)) = (f.bytes_sent, f.size) {
                if size > 0 {
                    line.push_str(&format!(" — {} sent", human_bytes(sent)));
                }
            }
        }
        lines.push(text(line));
    }
    if t.files.len() > FILE_CAP {
        lines.push(text(format!("and {} more…", t.files.len() - FILE_CAP)));
    }

    if t.errored_files.unwrap_or(0) > 0 {
        lines.push(text(format!(
            "{} file(s) failed to upload",
            t.errored_files.unwrap_or(0)
        )));
    }
    if t.out_of_space == Some(true) {
        lines.push(text("The cache is out of space"));
    }
    if t.fidelity == Some(Tier::T4) {
        lines.push(text(
            "Showing unuploaded files on disk, not live upload progress.",
        ));
    }
    if let Some(why) = &t.degraded_reason {
        lines.push(text(why));
    }
    lines
}

fn about(info: &ServiceInfo) -> MenuItem<TrayModel> {
    submenu(
        "About",
        vec![
            text(format!("Tray {}", env!("CARGO_PKG_VERSION"))),
            text(format!("Service {}", info.service_version)),
            text(format!(
                "Interface {} (this client speaks {})",
                info.interface_version,
                rvt_core::ipc::INTERFACE_VERSION
            )),
            text(format!("rclone {}", info.rclone_version)),
            text(format!("Capability tier {}", info.capability_tier)),
        ],
    )
}

/// Stopping the service is a submenu rather than an item, so it cannot be hit by reflex on
/// the way to Quit (#26).
fn stop_service() -> MenuItem<TrayModel> {
    submenu(
        "Stop service",
        vec![
            text("Mounts stay up unless the service is configured"),
            text("to unmount when it stops."),
            text("The tray will show the service as unreachable."),
            MenuItem::Separator,
            action("Yes, stop the service", Action::StopService),
        ],
    )
}

/// Mount or unmount every managed mount that is not already in that state, decided when the
/// item is clicked rather than when the menu was drawn.
fn bulk(label: &str, mount: bool) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(label),
        activate: Box::new(move |m: &mut TrayModel| {
            let names: Vec<String> = m
                .mounts()
                .filter(|v| v.managed && v.live != mount)
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
    let Some(n) = notice else { return Vec::new() };
    let mut items: Vec<MenuItem<TrayModel>> = wrap(&n.text, NOTICE_WIDTH, NOTICE_LINES)
        .into_iter()
        .map(text)
        .collect();
    if !items.is_empty() {
        items.push(MenuItem::Separator);
    }
    items
}

/// A line to read rather than click.
fn text(label: impl Into<String>) -> MenuItem<TrayModel> {
    StandardItem {
        label: escape(&label.into()),
        enabled: false,
        ..Default::default()
    }
    .into()
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

fn submenu(label: impl Into<String>, items: Vec<MenuItem<TrayModel>>) -> MenuItem<TrayModel> {
    SubMenu {
        label: escape(&label.into()),
        submenu: items,
        ..Default::default()
    }
    .into()
}

/// Protect a label from being read as a mnemonic.
///
/// DBusMenu strips a single underscore and turns a doubled one back into a single, so a
/// mount called `my_files` reaches the panel as "myfiles" unless it is doubled here. Applied
/// in the four constructors above rather than at each call site: most of what reaches a label
/// is a mount name, a path, or a sentence from rclone, and the ones that are not are our own
/// prose, which has no underscores to double.
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
    pub(super) fn labels(items: &[MenuItem<TrayModel>]) -> Vec<String> {
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

    pub(super) fn says(items: &[MenuItem<TrayModel>], needle: &str) -> bool {
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
        m.set_notice(None, "could not run systemctl: No such file or directory");
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
        );
        let items = build(&m);
        assert!(says(&items, "could not be unmounted"));
        assert!(
            labels(&items).iter().all(|l| l.len() <= NOTICE_WIDTH + 2),
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
    fn a_bulk_item_appears_only_when_there_is_something_for_it_to_do() {
        let (all_up, _a) = connected(vec![mount("photos", "mounted")], vec![]);
        let l = labels(&build(&all_up));
        assert!(l.contains(&"Unmount all".to_string()));
        assert!(!l.contains(&"Mount all".to_string()));
    }
}

#[cfg(test)]
mod round_one {
    use super::tests::{labels, says};
    use super::*;
    use crate::fixtures::{connected, idle_transfer, mount, pending};
    use crate::link::LinkError;

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
        m.set_notice(Some("my_photos".into()), "my_photos: still_in_use");

        for label in labels(&build(&m)) {
            assert!(escaped(&label), "a lone underscore survives in {label:?}");
        }

        // And the same on the other menu, whose headline can carry an error from zbus.
        m.go_down(&LinkError::Transport(zbus::Error::Failure(
            "peer_disconnected".into(),
        )));
        m.set_notice(None, "could not run systemctl: no_such_unit");
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
        let (m, _rx) = connected(vec![mount("elsewhere", "foreign")], vec![]);
        let items = build(&m);
        assert!(says(&items, "unknown"), "{:?}", labels(&items));
        assert!(!says(&items, "All synced"), "{:?}", labels(&items));
    }
}
