//! What the tray knows, and the readings it takes from that.
//!
//! Deliberately free of both `ksni` and `zbus`: everything here is a pure function of state
//! the service published, so the state machine and every sentence the menu shows can be
//! tested without a bus or a panel. [`crate::sni`] renders it, [`crate::watch`] fills it in.

use std::collections::BTreeMap;

use rvt_core::capabilities::Tier;
use rvt_core::ipc::{MountView, TransferView};
use rvt_core::transfer::TransferState;
use tokio::sync::mpsc::UnboundedSender;

use crate::link::LinkError;

/// Something the user asked for.
///
/// A menu callback runs on `ksni`'s task and has to return at once, so it sends one of these
/// rather than making the call itself — the menu is never waiting on a D-Bus round trip
/// (#52).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Bring a configured mount up.
    Mount(String),
    /// Take one down. Never forced from the menu: forcing severs a write in flight, and
    /// `rclone-vfsmount-tray unmount --force` is where that is chosen deliberately.
    Unmount(String),
    /// Hand a mount point to the desktop's file manager.
    Open(String),
    /// Start or stop the service's user unit.
    StartService,
    StopService,
    /// Re-read everything from the service.
    Refresh,
    /// Close the icon. Mounts are unaffected — see DESIGN.md, "The lifetime rule".
    Quit,
}

/// Whether the tray can currently see the service, and what it knows if so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Link {
    /// Before the first attempt has resolved. Says nothing yet.
    Connecting,
    /// No usable link, and why.
    Down(Down),
    /// Connected, and the versions agreed.
    Up(ServiceInfo),
}

/// A link that is not usable, reduced to what the menu shows.
///
/// The reason is folded into one sentence here rather than left as a [`LinkError`] so the
/// menu does not have to match on failure kinds it would render identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Down {
    /// The first line of the menu.
    pub headline: String,
    /// Whether starting the unit is the thing to offer. Only for a service that is simply
    /// not running: starting it fixes nothing when the bus is absent or the versions differ.
    pub offer_start: bool,
}

impl Down {
    pub(crate) fn from_error(e: &LinkError) -> Self {
        let (headline, offer_start) = match e {
            LinkError::NotRunning => ("The service is not running".to_string(), true),
            LinkError::NoSessionBus(_) => ("No session bus to reach".to_string(), false),
            LinkError::Incompatible => (
                "The service speaks a different interface — update both halves".to_string(),
                false,
            ),
            LinkError::TooOld { needed, found } => (
                format!("The service is too old: it offers interface {found}, this needs {needed}"),
                false,
            ),
            // `message` already opens with the clause a headline would otherwise repeat,
            // and for a refusal it is the service's own sentence, which stands alone.
            LinkError::Refused(_) | LinkError::Transport(_) => (e.message(), false),
        };
        Self {
            headline,
            offer_start,
        }
    }
}

/// What the service says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceInfo {
    pub service_version: String,
    pub interface_version: u32,
    pub rclone_version: String,
    pub capability_tier: String,
}

/// The outcome of an action, kept until it stops being true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    /// The mount it was about, if it was about one. Cleared when that mount's state next
    /// changes, because by then the sentence describes a moment that has passed.
    pub mount: Option<String>,
    pub text: String,
}

/// What the icon says.
///
/// Ordered by how loudly: [`Self::Disconnected`] outranks everything because nothing below
/// it can be known without a link, and [`Self::Degraded`] outranks [`Self::Syncing`] because
/// a figure that cannot be vouched for must not be presented as progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayState {
    /// The first attempt has not resolved. Distinct from [`Self::Disconnected`], which is an
    /// answer; this is not having asked yet, and lasts milliseconds.
    Connecting,
    /// The service is not reachable. Says nothing whatever about the mounts (#52).
    Disconnected,
    /// A mount failed, uploads errored, or the cache is full. The user has to act.
    Attention,
    /// Serving, but what is outstanding cannot be established.
    Degraded,
    /// Uploads pending or in flight. Normal operation.
    Syncing,
    /// Nothing is serving.
    Offline,
    /// Everything up, nothing outstanding.
    Idle,
}

impl TrayState {
    /// A freedesktop icon-naming-spec name, so the panel resolves it in any theme without
    /// this project installing artwork first. Bespoke symbolic icons are #29.
    pub(crate) fn icon_name(self) -> &'static str {
        match self {
            TrayState::Connecting => "image-loading",
            TrayState::Disconnected => "network-error",
            TrayState::Attention => "dialog-warning",
            // The state is not wrong, it is unreadable — which is what a question mark says.
            TrayState::Degraded => "dialog-question",
            TrayState::Syncing => "sync-synchronizing",
            TrayState::Offline => "network-offline",
            TrayState::Idle => "folder-remote",
        }
    }

    /// Whether the panel should emphasise the icon.
    ///
    /// Only for what the user has to act on. Uploads in progress are normal operation, and
    /// an icon that shouts through every large copy is one people learn to ignore (#25).
    pub(crate) fn needs_attention(self) -> bool {
        matches!(self, TrayState::Attention)
    }

    /// The first line of the tooltip.
    pub(crate) fn label(self) -> &'static str {
        match self {
            TrayState::Connecting => "Connecting…",
            TrayState::Disconnected => "Service unreachable",
            TrayState::Attention => "Needs attention",
            TrayState::Degraded => "State partly unknown",
            TrayState::Syncing => "Uploading",
            TrayState::Offline => "Nothing mounted",
            TrayState::Idle => "Up to date",
        }
    }
}

/// Everything the tray holds.
pub(crate) struct TrayModel {
    link: Link,
    /// Keyed by name so a signal upserts one row, and so the order the menu shows does not
    /// depend on the order the service happened to list them in.
    mounts: BTreeMap<String, MountView>,
    /// Held as [`TransferState`] rather than the wire type, for the predicates on it that
    /// decide what may honestly be shown.
    transfers: BTreeMap<String, TransferState>,
    notice: Option<Notice>,
    actions: UnboundedSender<Action>,
}

impl TrayModel {
    pub(crate) fn new(actions: UnboundedSender<Action>) -> Self {
        Self {
            link: Link::Connecting,
            mounts: BTreeMap::new(),
            transfers: BTreeMap::new(),
            notice: None,
            actions,
        }
    }

    /// Queue an action. A closed channel means the tray is shutting down, which is not
    /// something a menu click can do anything about.
    pub(crate) fn act(&self, action: Action) {
        if self.actions.send(action.clone()).is_err() {
            tracing::debug!(?action, "dropped: the tray is shutting down");
        }
    }

    pub(crate) fn link(&self) -> &Link {
        &self.link
    }

    pub(crate) fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub(crate) fn set_notice(&mut self, mount: Option<String>, text: impl Into<String>) {
        self.notice = Some(Notice {
            mount,
            text: text.into(),
        });
    }

    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// Mark the link down and forget everything it told us.
    ///
    /// Dropping the rows is the point: a stale list would be rendered as current, and #52's
    /// one rule is that the tray must never imply it knows a mount's state when it does not.
    /// What replaces it is a menu that says the service is unreachable — never "no mounts".
    pub(crate) fn go_down(&mut self, e: &LinkError) {
        self.link = Link::Down(Down::from_error(e));
        self.mounts.clear();
        self.transfers.clear();
        self.notice = None;
    }

    /// Adopt a fresh snapshot. Replaces rather than merges: state is not carried across a
    /// service lifetime (#52).
    pub(crate) fn go_up(&mut self, info: ServiceInfo, mounts: Vec<MountView>) {
        self.link = Link::Up(info);
        self.mounts = mounts.into_iter().map(|m| (m.name.clone(), m)).collect();
        self.transfers.clear();
        self.notice = None;
    }

    /// Re-read everything from the *same* service. Unlike [`Self::go_up`] this keeps the
    /// notice: it describes something that happened during this service's lifetime, and the
    /// user asking for a refresh is not asking to be told less.
    pub(crate) fn resync(&mut self, info: ServiceInfo, mounts: Vec<MountView>) {
        self.link = Link::Up(info);
        self.mounts = mounts.into_iter().map(|m| (m.name.clone(), m)).collect();
        self.transfers.clear();
    }

    pub(crate) fn upsert_mount(&mut self, view: MountView) {
        // The notice described this mount as it was before this change.
        if self.notice.as_ref().and_then(|n| n.mount.as_deref()) == Some(view.name.as_str()) {
            self.notice = None;
        }
        self.mounts.insert(view.name.clone(), view);
    }

    pub(crate) fn remove_mount(&mut self, name: &str) {
        self.mounts.remove(name);
        self.transfers.remove(name);
    }

    pub(crate) fn upsert_transfer(&mut self, view: &TransferView) {
        self.transfers
            .insert(view.mount.clone(), TransferState::from(view));
    }

    pub(crate) fn mounts(&self) -> impl Iterator<Item = &MountView> {
        self.mounts.values()
    }

    pub(crate) fn transfer(&self, name: &str) -> Option<&TransferState> {
        self.transfers.get(name)
    }

    /// What the icon should say.
    pub(crate) fn state(&self) -> TrayState {
        match self.link {
            Link::Connecting => return TrayState::Connecting,
            Link::Down(_) => return TrayState::Disconnected,
            Link::Up(_) => {}
        }
        let s = self.summary();
        if s.failed > 0 || s.errored > 0 || s.out_of_space {
            return TrayState::Attention;
        }
        if s.unobservable > 0 {
            return TrayState::Degraded;
        }
        if s.files > 0 {
            return TrayState::Syncing;
        }
        if s.live == 0 {
            return TrayState::Offline;
        }
        TrayState::Idle
    }

    /// Everything the menu and tooltip summarise, gathered in one pass.
    ///
    /// Outstanding work is counted over **live** mounts only. A configured mount that is
    /// deliberately down reports nothing observable, and counting that as "unknown" would
    /// leave the tray permanently unable to say anything about the mounts that are up.
    pub(crate) fn summary(&self) -> Summary {
        let mut s = Summary {
            total: self.mounts.len(),
            ..Summary::default()
        };
        let mut rate = None::<u64>;
        // An honest byte total needs every contributing mount to have one; one that cannot
        // give one poisons the aggregate rather than being silently left out of it.
        let mut remaining = Some(0u64);

        for m in self.mounts.values() {
            if m.state == "failed" {
                s.failed += 1;
            }
            if !m.live {
                continue;
            }
            s.live += 1;
            let Some(t) = self.transfers.get(&m.name) else {
                // A mount that is serving and has told us nothing. Permanent for a mount
                // this service did not start — it refuses to read one, and never polls it —
                // and momentary for one whose state arrived before its first reading.
                // Passing over it would leave the total describing the other mounts and the
                // headline calling the lot of them synced.
                s.unobservable += 1;
                continue;
            };
            if t.degraded_reason.is_some() || !t.outstanding_known {
                s.unobservable += 1;
            }
            // Saturating throughout: these come off the bus, and a total that overflows is
            // a panic in a debug build and a wrapped figure in a release one.
            s.files = s.files.saturating_add(t.pending.files);
            s.bytes = s.bytes.saturating_add(t.pending.known_bytes);
            s.unknown_size_files = s
                .unknown_size_files
                .saturating_add(t.pending.unknown_size_files);
            s.errored = s.errored.saturating_add(t.errored_files.unwrap_or(0));
            s.out_of_space |= t.out_of_space.unwrap_or(false);
            if let Some(r) = t.rate_bytes_per_sec {
                rate = Some(rate.unwrap_or(0).saturating_add(r));
            }
            if t.pending.files > 0 {
                // `remaining_bytes` sums only the files that contributed a size, so a queue
                // holding one file of unknown size is as unusable for an estimate as a tier
                // with no byte total at all — it would time the others and ignore that one.
                let vouched =
                    t.has_byte_total() && t.outstanding_known && t.pending.unknown_size_files == 0;
                remaining = match (remaining, vouched) {
                    (Some(acc), true) => Some(acc.saturating_add(t.remaining_bytes())),
                    _ => None,
                };
            }
            if t.fidelity == Some(Tier::T4) {
                s.dirty_on_disk_only = true;
            }
        }
        s.rate = rate;
        // A mount nothing could be read from is not a mount with nothing to send. Poisoning
        // here as well as per-mount above catches the two that never reach that branch: one
        // with no reading at all, and a blind one whose count happens to be zero.
        s.remaining = remaining.filter(|_| s.unobservable == 0);
        s
    }
}

/// The aggregate across live mounts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    /// Rows the service listed, however they got there.
    pub total: usize,
    /// Of those, the ones serving.
    pub live: usize,
    /// Mounts that failed and gave up.
    pub failed: usize,
    /// Live mounts whose outstanding work cannot be vouched for.
    pub unobservable: usize,
    pub files: u64,
    pub bytes: u64,
    pub unknown_size_files: u64,
    pub errored: u64,
    pub out_of_space: bool,
    /// Summed across the mounts that report one.
    pub rate: Option<u64>,
    /// Bytes still to send, or `None` when any pending mount cannot give an honest total.
    pub remaining: Option<u64>,
    /// Whether any figure above came from a cache walk rather than from rclone's queue.
    pub dirty_on_disk_only: bool,
}

impl Summary {
    /// How many mounts are serving, which is the line everything else qualifies.
    pub(crate) fn mounted_line(&self) -> String {
        if self.total == 0 {
            "No mounts configured".to_string()
        } else {
            format!("{} of {} mounted", self.live, self.total)
        }
    }

    /// The one line that answers "is my stuff uploaded?".
    ///
    /// `None` when nothing is serving: outstanding work is only counted for live mounts, so
    /// there is no reading to report rather than a reading of zero.
    pub(crate) fn pending_line(&self) -> Option<String> {
        if self.live == 0 {
            return None;
        }
        if self.files == 0 {
            return Some(if self.unobservable > 0 {
                // Nothing was counted, and nothing could have been: saying "all synced" here
                // would be a claim the reading does not support.
                format!(
                    "Pending: unknown on {} of {} mounts",
                    self.unobservable, self.live
                )
            } else {
                "All synced".to_string()
            });
        }
        let mut line = pending_phrase(self.files, self.bytes, self.unknown_size_files);
        if self.unobservable > 0 {
            line.push_str(&format!(", plus {} mount(s) unreadable", self.unobservable));
        }
        Some(line)
    }

    /// Throughput and, only where it can be derived honestly, how long is left.
    ///
    /// `None` rather than a guess: an ETA needs a byte total every contributing mount can
    /// vouch for, and a rate. Neither is available at every tier.
    pub(crate) fn rate_line(&self) -> Option<String> {
        let rate = self.rate.filter(|r| *r > 0)?;
        let mut line = format!("{}/s", human_bytes(rate));
        if let Some(left) = self.remaining.filter(|_| self.files > 0) {
            line.push_str(&format!(" · {} left", eta_phrase(left / rate)));
        }
        Some(line)
    }
}

/// What is outstanding, in one phrase, wherever it is shown.
///
/// The qualifier is part of it rather than something a caller adds: a count printed without
/// it reads as a total, and the summary and the per-mount line have to agree.
pub(crate) fn pending_phrase(files: u64, bytes: u64, unknown_size_files: u64) -> String {
    let mut line = format!("{} pending", files_and_bytes(files, bytes));
    if unknown_size_files > 0 {
        line.push_str(&format!(" ({unknown_size_files} of unknown size)"));
    }
    line
}

/// "3 files, 1.2 GiB", or just the count when no total is known.
fn files_and_bytes(files: u64, bytes: u64) -> String {
    let plural = if files == 1 { "file" } else { "files" };
    if bytes == 0 {
        format!("{files} {plural}")
    } else {
        format!("{files} {plural}, {}", human_bytes(bytes))
    }
}

/// Binary units, the ones rclone counts in. One decimal, and no trailing `.0`.
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    let rendered = format!("{value:.1}");
    format!(
        "{} {unit}",
        rendered.strip_suffix(".0").unwrap_or(&rendered)
    )
}

/// `s` in pieces of at most `width` characters, or whole when it already fits.
fn split_to_width(s: &str, width: usize) -> Vec<String> {
    if s.chars().count() <= width {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut piece = String::new();
    for c in s.chars() {
        if piece.chars().count() == width {
            out.push(std::mem::take(&mut piece));
        }
        piece.push(c);
    }
    if !piece.is_empty() {
        out.push(piece);
    }
    out
}

/// How long is left, at the resolution someone waiting actually cares about.
///
/// The hedging is part of the phrase rather than added by the caller: the estimate comes
/// from differencing a queue total across polls, which swings while files are still being
/// added, and a bare "12s" claims a precision it does not have.
pub(crate) fn eta_phrase(secs: u64) -> String {
    match secs {
        0..=4 => "a few seconds".to_string(),
        5..=59 => format!("about {secs}s"),
        60..=3599 => format!("about {}m", secs / 60),
        3600..=86_399 => {
            let (h, m) = (secs / 3600, (secs % 3600) / 60);
            if m == 0 {
                format!("about {h}h")
            } else {
                format!("about {h}h {m}m")
            }
        }
        _ => "over a day".to_string(),
    }
}

/// Break a sentence across menu rows.
///
/// A panel draws one item per line and does not wrap, so an unmount refusal — which carries
/// the command that names the process holding the mount — arrives as a row wider than the
/// screen unless it is broken up here.
pub(crate) fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    // Characters, not bytes. A mount point with an accented letter in it would otherwise
    // wrap short of the width, and the service's own sentences carry em dashes.
    let width = width.max(1);
    let fits = |line: &str, word: &str| line.chars().count() + 1 + word.chars().count() <= width;
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        // A word wider than the row still has to be broken. The sentence this exists for is
        // the service's busy refusal, which carries the mount point twice — and a path is
        // the one part of it with no spaces to break at.
        for piece in split_to_width(word, width) {
            match lines.last_mut() {
                Some(line) if fits(line, &piece) => {
                    line.push(' ');
                    line.push_str(&piece);
                }
                _ => lines.push(piece),
            }
        }
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        // The truncation has to be visible: a sentence that stops mid-clause reads as the
        // whole message, and the part cut off is usually the part that says what to do.
        if let Some(last) = lines.last_mut() {
            last.push_str(" …");
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{blank, connected, idle_transfer, mount, pending, service};
    use rvt_core::ipc::TransferFileView;

    #[test]
    fn every_state_shows_a_different_icon() {
        // A state that shares an icon with another is a state the user cannot tell apart.
        let states = [
            TrayState::Connecting,
            TrayState::Disconnected,
            TrayState::Attention,
            TrayState::Degraded,
            TrayState::Syncing,
            TrayState::Offline,
            TrayState::Idle,
        ];
        let names: std::collections::BTreeSet<_> = states.iter().map(|s| s.icon_name()).collect();
        assert_eq!(names.len(), states.len(), "two states share an icon");
        assert!(states.iter().all(|s| !s.label().is_empty()));
    }

    #[test]
    fn only_the_state_that_needs_acting_on_asks_for_attention() {
        // #25: uploads in progress are normal operation. An icon that shouts through every
        // large copy is one people learn to ignore.
        assert!(TrayState::Attention.needs_attention());
        for quiet in [
            TrayState::Disconnected,
            TrayState::Degraded,
            TrayState::Syncing,
            TrayState::Offline,
            TrayState::Idle,
        ] {
            assert!(!quiet.needs_attention(), "{quiet:?} asks for attention");
        }
    }

    #[test]
    fn the_state_is_never_up_without_a_link_and_says_which_kind_of_not() {
        // Not having asked yet is not the same as having asked and been refused: a tray that
        // says "Service unreachable" for the millisecond before it has looked is wrong every
        // time it starts.
        let (mut m, _rx) = blank();
        assert_eq!(m.state(), TrayState::Connecting, "before the first attempt");
        m.go_up(service(), vec![mount("photos", "mounted")]);
        m.upsert_transfer(&idle_transfer("photos"));
        assert_eq!(m.state(), TrayState::Idle);
        m.go_down(&LinkError::NotRunning);
        assert_eq!(m.state(), TrayState::Disconnected);
    }

    #[test]
    fn losing_the_link_forgets_every_mount() {
        // The #52 invariant, at the layer that holds the rows: a stale list outliving the
        // service would be rendered as current.
        let (mut m, _rx) = connected(
            vec![mount("photos", "mounted")],
            vec![idle_transfer("photos")],
        );
        assert_eq!(m.mounts().count(), 1);
        m.go_down(&LinkError::NotRunning);
        assert_eq!(m.mounts().count(), 0);
        assert!(m.transfer("photos").is_none());
    }

    #[test]
    fn reattaching_replaces_the_list_rather_than_merging_into_it() {
        // State is not carried across a service lifetime. A mount dropped from the config
        // while the service was down must not survive the reconnect.
        let (mut m, _rx) = connected(
            vec![mount("photos", "mounted"), mount("docs", "mounted")],
            vec![idle_transfer("photos")],
        );
        m.go_up(service(), vec![mount("photos", "mounted")]);
        assert_eq!(
            m.mounts().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["photos"]
        );
        assert!(
            m.transfer("photos").is_none(),
            "readings from the previous service lifetime are not carried over"
        );
    }

    #[test]
    fn a_failed_mount_wants_attention() {
        let mut failed = mount("photos", "failed");
        failed.reason = Some("rclone exited".into());
        let (m, _rx) = connected(vec![failed], vec![]);
        assert_eq!(m.state(), TrayState::Attention);
    }

    #[test]
    fn errored_uploads_and_a_full_cache_each_want_attention() {
        for spoil in [
            |t: &mut TransferView| t.errored_files = Some(2),
            |t: &mut TransferView| t.out_of_space = Some(true),
        ] {
            let mut t = idle_transfer("photos");
            spoil(&mut t);
            let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
            assert_eq!(m.state(), TrayState::Attention);
        }
    }

    #[test]
    fn a_live_mount_that_cannot_be_read_is_degraded() {
        for blind in [
            |t: &mut TransferView| t.degraded_reason = Some("rclone unreachable".into()),
            |t: &mut TransferView| t.outstanding_known = false,
        ] {
            let mut t = idle_transfer("photos");
            blind(&mut t);
            let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
            assert_eq!(m.state(), TrayState::Degraded);
        }
    }

    #[test]
    fn a_mount_that_is_simply_down_does_not_degrade_the_tray() {
        // The service reports an unmounted mount as unobservable, because it is. Reading
        // that as a degradation would pin the icon to "unknown" for anyone who keeps a
        // configured mount switched off.
        let mut t = idle_transfer("photos");
        t.outstanding_known = false;
        t.degraded_reason = Some("the mount is not serving".into());
        let (m, _rx) = connected(
            vec![mount("photos", "unmounted"), mount("docs", "mounted")],
            vec![t, idle_transfer("docs")],
        );
        assert_eq!(m.state(), TrayState::Idle);
    }

    #[test]
    fn a_reading_that_cannot_be_vouched_for_outranks_one_that_can() {
        // Degraded before Syncing: a figure nothing stands behind must not be presented as
        // progress.
        let mut blind = idle_transfer("photos");
        blind.degraded_reason = Some("rclone unreachable".into());
        let mut busy = idle_transfer("docs");
        pending(&mut busy, 3, 1024, 0);

        let (m, _rx) = connected(
            vec![mount("photos", "mounted"), mount("docs", "mounted")],
            vec![blind, busy],
        );
        assert_eq!(m.state(), TrayState::Degraded);
    }

    #[test]
    fn nothing_serving_reads_as_offline_rather_than_idle() {
        let (m, _rx) = connected(vec![mount("photos", "unmounted")], vec![]);
        assert_eq!(m.state(), TrayState::Offline);
    }

    #[test]
    fn work_outstanding_reads_as_syncing() {
        let mut t = idle_transfer("photos");
        pending(&mut t, 3, 1_288_490_188, 0);
        let (m, _rx) = connected(vec![mount("photos", "mounted")], vec![t]);
        assert_eq!(m.state(), TrayState::Syncing);
        assert_eq!(
            m.summary().pending_line().as_deref(),
            Some("3 files, 1.2 GiB pending")
        );
    }

    #[test]
    fn only_live_mounts_contribute_to_the_summary() {
        let mut down = idle_transfer("photos");
        pending(&mut down, 9, 9_000_000, 0);
        let mut up = idle_transfer("docs");
        pending(&mut up, 1, 1024, 0);

        let s = connected(
            vec![mount("photos", "unmounted"), mount("docs", "mounted")],
            vec![down, up],
        )
        .0
        .summary();
        assert_eq!((s.live, s.total), (1, 2));
        assert_eq!(s.files, 1, "the unmounted mount's queue is not aggregated");
    }

    #[test]
    fn nothing_mounted_gives_no_pending_line_at_all() {
        // Not "0 files pending": the tray counts nothing for a mount that is not serving, so
        // there is no reading to report.
        let s = connected(vec![mount("photos", "unmounted")], vec![])
            .0
            .summary();
        assert_eq!(s.pending_line(), None);
        assert_eq!(s.mounted_line(), "0 of 1 mounted");
    }

    #[test]
    fn no_mounts_at_all_says_so() {
        assert_eq!(
            connected(vec![], vec![]).0.summary().mounted_line(),
            "No mounts configured"
        );
    }

    #[test]
    fn an_eta_needs_a_byte_total_every_pending_mount_can_vouch_for() {
        // T3 is `vfs/stats` alone: counts with no sizes. Differencing a total that is always
        // zero would produce a confident, wrong "2 seconds left".
        let mut countable = idle_transfer("photos");
        pending(&mut countable, 2, 20_000_000, 0);
        countable.rate_bytes_per_sec = Some(10_000_000);

        let mut sizeless = idle_transfer("docs");
        sizeless.fidelity = Some("T3".into());
        pending(&mut sizeless, 2, 0, 2);
        sizeless.rate_bytes_per_sec = Some(10_000_000);

        let honest = connected(vec![mount("photos", "mounted")], vec![countable.clone()])
            .0
            .summary();
        assert_eq!(honest.remaining, Some(20_000_000));
        let line = honest.rate_line().expect("a rate was reported");
        assert!(line.contains("left"), "{line}");

        let spoiled = connected(
            vec![mount("photos", "mounted"), mount("docs", "mounted")],
            vec![countable, sizeless],
        )
        .0
        .summary();
        assert_eq!(spoiled.remaining, None);
        let line = spoiled.rate_line().expect("a rate was still reported");
        assert!(
            !line.contains("left"),
            "one mount that cannot give a total must suppress the estimate: {line}"
        );
    }

    #[test]
    fn bytes_already_sent_come_off_the_estimate() {
        let mut t = idle_transfer("photos");
        pending(&mut t, 1, 10_000_000, 0);
        t.has_progress = true;
        t.files = vec![TransferFileView {
            name: "clip.mp4".into(),
            size: Some(10_000_000),
            in_flight: Some(true),
            tries: Some(1),
            bytes_sent: Some(6_000_000),
        }];
        t.rate_bytes_per_sec = Some(1_048_576);

        let s = connected(vec![mount("photos", "mounted")], vec![t])
            .0
            .summary();
        assert_eq!(s.remaining, Some(4_000_000));
        assert_eq!(
            s.rate_line().as_deref(),
            Some("1 MiB/s · a few seconds left")
        );
    }

    #[test]
    fn no_rate_means_no_rate_line() {
        let mut t = idle_transfer("photos");
        pending(&mut t, 1, 1024, 0);
        let s = connected(vec![mount("photos", "mounted")], vec![t])
            .0
            .summary();
        assert_eq!(s.rate_line(), None);
    }

    #[test]
    fn files_of_unknown_size_are_counted_but_not_weighed() {
        let mut t = idle_transfer("photos");
        pending(&mut t, 3, 1024, 2);
        let line = connected(vec![mount("photos", "mounted")], vec![t])
            .0
            .summary()
            .pending_line()
            .expect("something is pending");
        assert_eq!(line, "3 files, 1 KiB pending (2 of unknown size)");
    }

    #[test]
    fn a_notice_lasts_until_the_mount_it_names_moves_on() {
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        m.set_notice(Some("photos".into()), "photos: still in use");
        assert!(m.notice().is_some());

        m.upsert_mount(mount("docs", "mounted"));
        assert!(
            m.notice().is_some(),
            "another mount's news is not this one's"
        );

        m.upsert_mount(mount("photos", "unmounted"));
        assert!(m.notice().is_none());
    }

    #[test]
    fn a_removed_mount_takes_its_reading_with_it() {
        let (mut m, _rx) = connected(
            vec![mount("photos", "mounted")],
            vec![idle_transfer("photos")],
        );
        m.remove_mount("photos");
        assert_eq!(m.mounts().count(), 0);
        assert!(m.transfer("photos").is_none());
    }

    #[test]
    fn only_a_stopped_service_is_worth_offering_to_start() {
        assert!(Down::from_error(&LinkError::NotRunning).offer_start);
        for pointless in [
            LinkError::Incompatible,
            LinkError::TooOld {
                needed: 2,
                found: 1,
            },
        ] {
            let d = Down::from_error(&pointless);
            assert!(!d.offer_start, "{}", d.headline);
            assert!(!d.headline.is_empty());
        }
    }

    #[test]
    fn bytes_read_the_way_rclone_counts_them() {
        for (n, want) in [
            (0u64, "0 B"),
            (999, "999 B"),
            (1024, "1 KiB"),
            (1536, "1.5 KiB"),
            (1_048_576, "1 MiB"),
            (1_288_490_188, "1.2 GiB"),
            (1_099_511_627_776, "1 TiB"),
            (u64::MAX, "16 EiB"),
        ] {
            assert_eq!(human_bytes(n), want, "{n}");
        }
    }

    #[test]
    fn an_estimate_is_hedged_and_stops_at_a_useful_resolution() {
        for (secs, want) in [
            (0u64, "a few seconds"),
            (4, "a few seconds"),
            (5, "about 5s"),
            (59, "about 59s"),
            (60, "about 1m"),
            (3599, "about 59m"),
            (3600, "about 1h"),
            (5400, "about 1h 30m"),
            (86_400, "over a day"),
        ] {
            assert_eq!(eta_phrase(secs), want, "{secs}");
        }
    }

    #[test]
    fn a_single_file_is_not_pluralised_and_an_unknown_total_is_not_shown() {
        assert_eq!(files_and_bytes(1, 0), "1 file");
        assert_eq!(files_and_bytes(2, 0), "2 files");
        assert_eq!(files_and_bytes(1, 2048), "1 file, 2 KiB");
    }

    #[test]
    fn a_long_refusal_is_broken_into_rows_and_says_when_it_was_cut() {
        let short = wrap("still in use", 64, 5);
        assert_eq!(short, ["still in use"]);

        let text = "the mount point could not be released because a process is still \
                    using it; run fuser to find out which one, then close it and try again";
        let lines = wrap(text, 32, 3);
        assert!(lines.iter().all(|l| l.len() <= 34), "{lines:?}");
        assert_eq!(lines.len(), 3);
        assert!(
            lines.last().unwrap().ends_with('…'),
            "a sentence cut short must say so: {lines:?}"
        );

        assert!(wrap("", 32, 5).is_empty());
    }

    #[test]
    fn an_action_reaches_the_task_that_carries_it_out() {
        let (m, mut rx) = blank();
        m.act(Action::Mount("photos".into()));
        assert_eq!(rx.try_recv(), Ok(Action::Mount("photos".into())));
    }
}

#[cfg(test)]
mod round_one {
    use super::*;
    use crate::fixtures::{connected, idle_transfer, mount, pending, service};

    #[test]
    fn a_serving_mount_that_has_said_nothing_is_not_called_synced() {
        // The durable case is a mount this service did not start: it refuses to read one and
        // never polls it, so no reading ever arrives and the row is live and blank forever.
        // Counting it as clean would answer "is my work uploaded?" with a yes nothing backs.
        let (m, _rx) = connected(vec![mount("elsewhere", "foreign")], vec![]);
        assert_eq!(m.state(), TrayState::Degraded);
        let line = m.summary().pending_line().expect("something is mounted");
        assert!(line.contains("unknown"), "{line}");
        assert_ne!(line, "All synced");
    }

    #[test]
    fn one_blank_mount_does_not_hide_behind_another_that_answered() {
        let mut busy = idle_transfer("photos");
        pending(&mut busy, 2, 2048, 0);
        let (m, _rx) = connected(
            vec![mount("photos", "mounted"), mount("elsewhere", "foreign")],
            vec![busy],
        );
        let s = m.summary();
        assert_eq!(s.unobservable, 1);
        assert!(
            s.pending_line().unwrap().contains("1 mount(s) unreadable"),
            "{:?}",
            s.pending_line()
        );
    }

    #[test]
    fn an_estimate_needs_every_pending_file_to_have_a_size() {
        // `remaining_bytes` sums only files that reported one, so timing the queue by it
        // while a file has no size counts down to zero with that file still to send. A cache
        // walk produces exactly this: a dirty descriptor whose data file cannot be measured
        // leaves the walk complete and the size absent.
        let mut t = idle_transfer("photos");
        pending(&mut t, 5, 100 * 1024 * 1024, 1);
        t.rate_bytes_per_sec = Some(10 * 1024 * 1024);

        let s = connected(vec![mount("photos", "mounted")], vec![t])
            .0
            .summary();
        assert_eq!(s.remaining, None);
        let line = s.rate_line().expect("the rate itself is still known");
        assert!(!line.contains("left"), "{line}");
    }

    #[test]
    fn a_refresh_keeps_the_notice_and_a_new_service_does_not() {
        let (mut m, _rx) = connected(vec![mount("photos", "mounted")], vec![]);
        m.set_notice(Some("photos".into()), "photos: still in use");

        m.resync(service(), vec![mount("photos", "mounted")]);
        assert!(
            m.notice().is_some(),
            "re-reading the same service does not make the refusal untrue"
        );

        m.go_up(service(), vec![mount("photos", "mounted")]);
        assert!(
            m.notice().is_none(),
            "a new service lifetime knows nothing of it"
        );
    }

    #[test]
    fn a_wrapped_line_is_measured_in_characters_not_bytes() {
        // Every sentence the tray shows may carry an em dash, and the service's failure text
        // does. Counting its three bytes as three columns wraps the row short.
        let text = "él él él él";
        assert_eq!(wrap(text, 11, 5), ["él él él él"]);
    }

    #[test]
    fn an_unreachable_service_says_so_once() {
        let d = Down::from_error(&LinkError::Transport(zbus::Error::Failure(
            "connection reset".into(),
        )));
        assert_eq!(
            d.headline.matches("could not be reached").count(),
            1,
            "{}",
            d.headline
        );
        assert!(d.headline.contains("connection reset"));
    }
}

#[cfg(test)]
mod round_two {
    use super::*;
    use crate::fixtures::{connected, idle_transfer, mount, pending};

    #[test]
    fn no_estimate_is_offered_while_any_mount_cannot_be_read() {
        // A mount nothing could be read from is not a mount with nothing left to send, so
        // timing the ones that answered says the whole job finishes when they do. The icon
        // already says "State partly unknown" here; a countdown beside it contradicts it.
        let mut busy = idle_transfer("photos");
        pending(&mut busy, 1, 100 * 1024 * 1024, 0);
        busy.rate_bytes_per_sec = Some(4 * 1024 * 1024);

        let (m, _rx) = connected(
            vec![mount("photos", "mounted"), mount("elsewhere", "foreign")],
            vec![busy],
        );
        let s = m.summary();
        assert_eq!(m.state(), TrayState::Degraded);
        assert_eq!(s.remaining, None);
        let line = s.rate_line().expect("the rate is still known");
        assert!(!line.contains("left"), "{line}");
    }

    #[test]
    fn a_blind_mount_with_nothing_counted_still_stops_the_estimate() {
        // The other way in: this one answered, said zero, and said it could not vouch for
        // the zero — so it never reaches the per-mount check, which only runs on a mount
        // with something pending.
        let mut busy = idle_transfer("photos");
        pending(&mut busy, 1, 100 * 1024 * 1024, 0);
        busy.rate_bytes_per_sec = Some(4 * 1024 * 1024);
        let mut blind = idle_transfer("docs");
        blind.outstanding_known = false;

        let s = connected(
            vec![mount("photos", "mounted"), mount("docs", "mounted")],
            vec![busy, blind],
        )
        .0
        .summary();
        assert_eq!(s.remaining, None);
    }

    #[test]
    fn a_word_with_no_spaces_is_broken_rather_than_left_to_overhang() {
        // The sentence this exists for embeds a mount point, and a path is the one part of
        // it with nowhere to break.
        let path = "/home/someone/mnt/a-very-long-directory-name/photos";
        let lines = wrap(&format!("busy: {path}"), 20, 8);
        assert!(lines.iter().all(|l| l.chars().count() <= 20), "{lines:?}");
        assert!(lines.len() > 1);
        assert_eq!(
            lines.join("").replace(' ', ""),
            format!("busy:{path}").replace(' ', ""),
            "breaking a word must not lose any of it"
        );
    }

    #[test]
    fn a_width_of_zero_does_not_spin() {
        // Nothing passes this today; the loop that breaks a long word would not terminate.
        assert!(!wrap("something", 0, 4).is_empty());
    }
}
