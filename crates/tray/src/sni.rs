//! The StatusNotifierItem `ksni` serves on our behalf.
//!
//! Every method here is a read of [`TrayModel`] and nothing else. `ksni` calls them from its
//! own task while holding the model's lock, so anything that awaited would stall the panel.

use ksni::{Category, OfflineReason, Status, ToolTip, Tray};

use crate::menu;
use crate::model::{Action, Link, TrayModel, TrayState};

/// The item's identity on the bus. Panels remember an icon's position by it, so it is fixed
/// rather than derived from the session.
const TRAY_ID: &str = "rclone-vfsmount-tray";

const TRAY_TITLE: &str = "rclone VFS mounts";

impl Tray for TrayModel {
    fn id(&self) -> String {
        TRAY_ID.to_string()
    }

    fn title(&self) -> String {
        TRAY_TITLE.to_string()
    }

    fn category(&self) -> Category {
        Category::SystemServices
    }

    fn icon_name(&self) -> String {
        self.state().icon_name().to_string()
    }

    /// Shown in place of [`Self::icon_name`] while the status is `NeedsAttention`, so it has
    /// to be the attention icon whatever the current state derives to.
    fn attention_icon_name(&self) -> String {
        TrayState::Attention.icon_name().to_string()
    }

    fn status(&self) -> Status {
        if self.state().needs_attention() {
            Status::NeedsAttention
        } else {
            // Never `Passive`: a host is free to hide a passive item, and an icon that
            // vanishes when everything is fine is one the user cannot find when it is not.
            Status::Active
        }
    }

    /// The summary, without opening anything.
    ///
    /// Deliberately built from counts and fixed prose only. A host may render this as markup
    /// (the specification allows a subset of HTML), so a mount name or an rclone message
    /// reaching it would be at the mercy of whatever `&` or `<` it contains. The detail lives
    /// in the menu, where DBusMenu takes plain text.
    fn tool_tip(&self) -> ToolTip {
        let state = self.state();
        let description = match self.link() {
            Link::Connecting => "Connecting…".to_string(),
            Link::Down(_) => {
                "The tray cannot reach the service.\nMounts already up are unaffected.".to_string()
            }
            Link::Up(_) => {
                let s = self.summary();
                let mut lines = vec![s.mounted_line()];
                lines.extend(s.pending_line());
                lines.extend(s.rate_line());
                lines.join("\n")
            }
        };
        ToolTip {
            icon_name: state.icon_name().to_string(),
            icon_pixmap: Vec::new(),
            title: format!("{TRAY_TITLE} — {}", self.headline()),
            description,
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        menu::build(self)
    }

    /// A left click. There is no window to raise — the menu is the whole interface — so this
    /// re-reads state from the service, which is the only thing a click could usefully mean.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.act(Action::Refresh);
    }

    fn watcher_online(&self) {
        tracing::info!("a StatusNotifierWatcher appeared; the icon is registered");
    }

    /// Keep running with no panel to draw us.
    ///
    /// Returning `false` would end the tray service. A desktop that restarts its shell, or
    /// one that starts the session's programs before the panel, would then leave the user
    /// with no icon and no way to get one back short of restarting the process (#25).
    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        tracing::warn!(
            ?reason,
            "no StatusNotifierWatcher on this session: the icon is not being shown. \
             Waiting for one to appear."
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{blank, connected, idle_transfer, mount, pending};
    use crate::link::LinkError;

    #[test]
    fn the_icon_follows_the_state_and_the_attention_icon_does_not() {
        let (up, _a) = connected(
            vec![mount("photos", "mounted")],
            vec![idle_transfer("photos")],
        );
        assert_eq!(up.icon_name(), TrayState::Idle.icon_name());
        // Shown in place of the normal icon while the status is NeedsAttention, so it has to
        // be the attention icon even while the tray is idle.
        assert_eq!(up.attention_icon_name(), TrayState::Attention.icon_name());

        let (mut down, _b) = blank();
        down.go_down(&LinkError::NotRunning);
        assert_eq!(down.icon_name(), TrayState::Disconnected.icon_name());
    }

    #[test]
    fn the_panel_is_only_asked_to_emphasise_what_needs_acting_on() {
        let (idle, _a) = connected(
            vec![mount("photos", "mounted")],
            vec![idle_transfer("photos")],
        );
        assert!(matches!(idle.status(), Status::Active));

        let mut busy = idle_transfer("photos");
        pending(&mut busy, 3, 3000, 0);
        let (syncing, _b) = connected(vec![mount("photos", "mounted")], vec![busy]);
        assert!(
            matches!(syncing.status(), Status::Active),
            "uploads in progress are normal operation"
        );

        let mut failed = mount("photos", "failed");
        failed.reason = Some("rclone exited".into());
        let (bad, _c) = connected(vec![failed], vec![]);
        assert!(matches!(bad.status(), Status::NeedsAttention));
    }

    #[test]
    fn the_item_is_never_passive() {
        // A host may hide a passive item, and an icon that disappears while everything is
        // fine is one the user cannot find when it is not.
        let (mut m, _rx) = connected(vec![mount("photos", "unmounted")], vec![]);
        assert!(!matches!(m.status(), Status::Passive));
        m.go_down(&LinkError::NotRunning);
        assert!(!matches!(m.status(), Status::Passive));
    }

    #[test]
    fn a_disconnected_tooltip_describes_the_link_and_not_the_mounts() {
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        m.go_down(&LinkError::NotRunning);
        let tip = m.tool_tip();
        assert!(
            tip.description.contains("cannot reach the service"),
            "{tip:?}"
        );
        assert!(tip.description.contains("unaffected"), "{tip:?}");
        assert!(
            !tip.description.contains("mounted"),
            "a count here would be a claim the tray cannot make: {tip:?}"
        );
    }

    #[test]
    fn a_connected_tooltip_carries_the_summary() {
        let mut busy = idle_transfer("photos");
        pending(&mut busy, 3, 1_288_490_188, 0);
        busy.rate_bytes_per_sec = Some(4 * 1024 * 1024);
        let (m, _rx) = connected(
            vec![mount("photos", "mounted"), mount("docs", "unmounted")],
            vec![busy],
        );
        let tip = m.tool_tip();
        assert_eq!(
            tip.description,
            "1 of 2 mounted\n3 files, 1.2 GiB pending\n4 MiB/s · about 5m left"
        );
        assert_eq!(tip.icon_name, TrayState::Syncing.icon_name());
        assert!(tip.title.contains(TRAY_TITLE));
    }

    #[test]
    fn the_item_keeps_one_identity_whatever_it_is_showing() {
        // Panels remember an icon's position by its id.
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        let id = m.id();
        m.go_down(&LinkError::NotRunning);
        assert_eq!(m.id(), id);
        assert_eq!(id, TRAY_ID);
        assert!(!m.title().is_empty());
    }

    #[test]
    fn a_missing_panel_does_not_end_the_tray() {
        // Returning false here would stop the tray service. A shell restart, or a login that
        // starts programs before the panel, must leave the icon waiting for one.
        let (m, _rx) = blank();
        assert!(m.watcher_offline(OfflineReason::No));
    }
}
