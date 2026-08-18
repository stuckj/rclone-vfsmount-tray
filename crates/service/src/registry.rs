//! What the service believes about its mounts, and what a client has not been told yet.
//!
//! Everything the D-Bus surface answers with comes from here rather than from a fresh
//! sweep, so a client's menu never waits on `/proc` and systemd. The watcher (see
//! [`crate::watch`]) is what keeps it current.

use rvt_core::ipc::{MountView, TransferView};
use rvt_core::transfer::TransferState;
use rvt_core::{MountState, Tier};
use std::collections::BTreeMap;

/// Something that has happened since a client was last told.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A mount appeared, or its state changed.
    Mount(MountView),
    /// A row went away: a foreign mount unmounted, or an orphan was stopped.
    Removed(String),
    /// A mount's outstanding work changed.
    Transfer(TransferView),
    /// The best tier any mount has resolved changed, which happens the first time one
    /// answers over rc. A property rather than a signal, but it has to be published all
    /// the same: a client that reads it once and caches it — which is what a D-Bus proxy
    /// does by default — otherwise keeps `"unknown"` for the life of the connection.
    CapabilityTier,
}

/// The mounts, and the last reading taken for each.
#[derive(Debug, Default)]
pub struct Registry {
    /// Ordered so [`Self::mounts`] answers in a stable order; a client renders a list.
    mounts: BTreeMap<String, MountView>,
    transfers: BTreeMap<String, TransferView>,
    /// Richest tier any mount has resolved, which is a property of the rclone binary all
    /// of them share. `None` until one has connected.
    tier: Option<Tier>,
}

impl Registry {
    /// Take in a whole sweep, and report what changed.
    ///
    /// The sweep is the entire truth about which mounts exist, so a name missing from it
    /// is a removal. A reading for a mount that is no longer serving is dropped with it:
    /// the last figures before an unmount describe a mount that is no longer there, and
    /// serving them on would be the confident-but-wrong answer the tier rules exist to
    /// prevent. Dropping one is published, not merely done — a client keeps a model from
    /// these changes, so a reading it is never told to forget is one it goes on showing.
    pub fn observe_mounts(&mut self, found: Vec<MountView>) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut seen = BTreeMap::new();

        for view in found {
            let changed = self.mounts.get(&view.name) != Some(&view);
            if !pollable(&view) && self.transfers.remove(&view.name).is_some() {
                changes.push(Change::Transfer(nothing_to_say(&view)));
            }
            if changed {
                changes.push(Change::Mount(view.clone()));
            }
            seen.insert(view.name.clone(), view);
        }

        for name in self.mounts.keys() {
            if !seen.contains_key(name) {
                self.transfers.remove(name);
                changes.push(Change::Removed(name.clone()));
            }
        }

        self.mounts = seen;
        changes
    }

    /// Record a fresh reading for one mount, reporting whether it said anything new.
    pub fn observe_transfer(&mut self, view: TransferView) -> Option<Change> {
        if self.transfers.get(&view.mount) == Some(&view) {
            return None;
        }
        self.transfers.insert(view.mount.clone(), view.clone());
        Some(Change::Transfer(view))
    }

    /// Every mount, in name order.
    pub fn mounts(&self) -> Vec<MountView> {
        self.mounts.values().cloned().collect()
    }

    /// One mount by name.
    pub fn mount(&self, name: &str) -> Option<&MountView> {
        self.mounts.get(name)
    }

    /// Record what a mount's rc connection turned out to support, reporting whether that
    /// changed the answer.
    ///
    /// Kept at the richest seen: `Tier` orders by detail, so `min` is the best of them.
    pub fn note_tier(&mut self, tier: Tier) -> Option<Change> {
        let best = Some(self.tier.map_or(tier, |seen| seen.min(tier)));
        if best == self.tier {
            return None;
        }
        self.tier = best;
        Some(Change::CapabilityTier)
    }

    /// The richest tier any mount has resolved, or `None` before one has.
    pub fn tier(&self) -> Option<Tier> {
        self.tier
    }

    /// The last reading for a mount, if one has been taken since it came up.
    pub fn transfer(&self, name: &str) -> Option<&TransferView> {
        self.transfers.get(name)
    }

    /// The mounts worth polling: serving, and started by us.
    pub fn pollable(&self) -> Vec<String> {
        self.mounts
            .values()
            .filter(|v| pollable(v))
            .map(|v| v.name.clone())
            .collect()
    }
}

/// An empty reading, for a mount there is nothing to read from.
///
/// Not a zero. [`TransferState::unmonitored`] leaves `outstanding_known` false and carries
/// the reason, so a client cannot render this as "nothing left to upload, safe to unmount".
pub fn nothing_to_say(mount: &MountView) -> TransferView {
    TransferView::from(&TransferState::unmonitored(
        &mount.name,
        why_not_polled(mount),
    ))
}

/// An empty reading for a configured mount no sweep has reached yet.
///
/// Only [`Registry`] knows what is serving, and until the first sweep succeeds it knows
/// nothing — which is not the same as a mount that is down, and must not be answered as
/// though the figures were merely zero.
pub fn not_swept_yet(name: &str) -> TransferView {
    TransferView::from(&TransferState::unmonitored(
        name,
        "no sweep has reported on this mount yet",
    ))
}

/// Why a mount has no reading, in words a client can show.
///
/// Every one of these is an ordinary state rather than a failure, which is why it is a
/// reason attached to an empty answer and not an error.
///
/// Read back through [`rvt_core::ipc::state_from_name`] rather than matched as strings, so
/// the vocabulary lives in one place and a renamed state cannot quietly fall through.
fn why_not_polled(mount: &MountView) -> String {
    match rvt_core::ipc::state_from_name(&mount.state, mount.reason.as_deref()) {
        Some(MountState::Foreign) => {
            "started outside this service, so its rc socket is unknown".into()
        }
        Some(MountState::Orphaned) => "no configuration describes this mount any more".into(),
        Some(MountState::Mounted) => "not polled yet".into(),
        _ => "the mount is not serving".into(),
    }
}

/// Whether this mount has an rc socket of ours to ask.
///
/// `Mounted` alone, not `live && managed`. An orphan is both, and does still answer on
/// the socket it was started with, but no config entry describes it any more, so there is
/// nothing to report it against. A foreign mount's socket is unknown by definition (#70).
fn pollable(view: &MountView) -> bool {
    view.state == rvt_core::ipc::state_name(&rvt_core::MountState::Mounted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built through the real conversion, so `live` and `managed` are whatever the state
    /// actually implies. Writing them by hand made every fixture claim a foreign mount was
    /// not live, which is the one thing it is.
    fn view(name: &str, state: &str) -> MountView {
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

    fn transfer(mount: &str, files: u64) -> TransferView {
        TransferView {
            mount: mount.into(),
            fidelity: Some("T2".into()),
            outstanding_known: true,
            has_progress: false,
            pending_files: files,
            pending_known_bytes: files * 1024,
            pending_unknown_size_files: 0,
            uploading: None,
            errored_files: None,
            out_of_space: None,
            rate_bytes_per_sec: None,
            files: Vec::new(),
            degraded_reason: None,
        }
    }

    #[test]
    fn a_first_sweep_is_all_new() {
        let mut r = Registry::default();
        let changes = r.observe_mounts(vec![view("photos", "mounted"), view("docs", "unmounted")]);
        assert_eq!(changes.len(), 2);
        assert_eq!(r.mounts().len(), 2);
    }

    #[test]
    fn an_unchanged_sweep_says_nothing() {
        let mut r = Registry::default();
        r.observe_mounts(vec![view("photos", "mounted")]);
        assert!(r.observe_mounts(vec![view("photos", "mounted")]).is_empty());
    }

    #[test]
    fn a_state_transition_is_reported_once() {
        let mut r = Registry::default();
        r.observe_mounts(vec![view("photos", "unmounted")]);

        let changes = r.observe_mounts(vec![view("photos", "mounted")]);
        assert_eq!(changes, vec![Change::Mount(view("photos", "mounted"))]);
        assert!(r.observe_mounts(vec![view("photos", "mounted")]).is_empty());
    }

    #[test]
    fn a_row_that_vanishes_is_reported_as_removed() {
        // Configured mounts never vanish — a foreign mount somebody else unmounted does,
        // and a client left holding the row would show a mount that is not there.
        let mut r = Registry::default();
        r.observe_mounts(vec![
            view("photos", "mounted"),
            view("/mnt/theirs", "foreign"),
        ]);

        let changes = r.observe_mounts(vec![view("photos", "mounted")]);
        assert_eq!(changes, vec![Change::Removed("/mnt/theirs".into())]);
        assert!(r.mount("/mnt/theirs").is_none());
    }

    #[test]
    fn the_tier_reported_is_the_richest_any_mount_reached() {
        let mut r = Registry::default();
        assert_eq!(
            r.tier(),
            None,
            "no tier may be named before one is resolved"
        );

        assert_eq!(r.note_tier(Tier::T3), Some(Change::CapabilityTier));
        assert_eq!(r.note_tier(Tier::T2), Some(Change::CapabilityTier));
        assert_eq!(
            r.note_tier(Tier::T4),
            None,
            "a poorer tier does not change the answer, so nothing is published"
        );
        assert_eq!(r.note_tier(Tier::T2), None, "nor does repeating the best");
        assert_eq!(r.tier(), Some(Tier::T2));
    }

    #[test]
    fn dropping_a_reading_is_published_rather_than_done_quietly() {
        // A client builds its model from these changes, so a reading it is never told to
        // forget is one it goes on showing: "3 files, 1.2 GB still to upload" against a
        // mount that is down.
        let mut r = Registry::default();
        r.observe_mounts(vec![view("photos", "mounted")]);
        r.observe_transfer(transfer("photos", 3));

        let changes = r.observe_mounts(vec![view("photos", "unmounted")]);
        let emptied = changes
            .iter()
            .find_map(|c| match c {
                Change::Transfer(v) => Some(v),
                _ => None,
            })
            .expect("the dropped reading has to be announced");

        assert_eq!(emptied.pending_files, 0);
        assert!(
            !emptied.outstanding_known,
            "an empty reading that claims to be exact reads as safe to unmount"
        );
        assert!(emptied.degraded_reason.is_some());
    }

    #[test]
    fn a_reading_does_not_outlive_the_mount_it_describes() {
        let mut r = Registry::default();
        r.observe_mounts(vec![view("photos", "mounted")]);
        r.observe_transfer(transfer("photos", 3));
        assert!(r.transfer("photos").is_some());

        r.observe_mounts(vec![view("photos", "unmounted")]);
        assert_eq!(
            r.transfer("photos"),
            None,
            "figures from before an unmount describe a mount that is no longer serving"
        );
    }

    #[test]
    fn an_unchanged_reading_is_not_re_announced() {
        let mut r = Registry::default();
        r.observe_mounts(vec![view("photos", "mounted")]);

        assert!(r.observe_transfer(transfer("photos", 3)).is_some());
        assert!(r.observe_transfer(transfer("photos", 3)).is_none());
        assert!(r.observe_transfer(transfer("photos", 2)).is_some());
    }

    #[test]
    fn only_mounts_with_a_socket_of_ours_are_polled() {
        let mut r = Registry::default();
        r.observe_mounts(vec![
            view("photos", "mounted"),
            view("docs", "unmounted"),
            view("stale", "orphaned"),
            view("/mnt/theirs", "foreign"),
            view("broken", "failed"),
        ]);
        assert_eq!(r.pollable(), vec!["photos".to_string()]);
    }
}
