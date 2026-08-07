//! Which rc commands a particular rclone actually has, and what may honestly be shown
//! as a result.
//!
//! Detected with `rc/list`, which enumerates the commands a build registers, rather than
//! inferred from a version number. A version is a guess about a build; `rc/list` is the
//! build's own answer, and it reflects how rclone was compiled and flagged.
//!
//! Resolution is per-connection, not global: one mount may be reachable over rc while
//! another was started by someone else and can only be scanned on disk.

use crate::models::RcList;
use crate::rc::{RcClient, RcError};
use std::collections::BTreeSet;

/// How much can be said about pending uploads, given what this rclone will tell us.
///
/// Ordering is by how much detail the tier supports, not by preference: `T4` is a
/// first-class tier, and the only one that works when rclone is unreachable or has
/// crashed, because the rc endpoints only know a running process's in-memory queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// `core/stats` — per-file progress and real ETAs.
    T1,
    /// `vfs/queue` — per-file sizes and an in-flight flag, aggregate rate, no per-file
    /// percentages. The minimum bar for reporting pending uploads.
    T2,
    /// `vfs/stats` — counts only. Does not meet the bar alone, but hands over the cache
    /// paths that T4 needs.
    T3,
    /// On-disk cache scan. No rc required.
    T4,
}

impl Tier {
    /// Whether this tier can report what is outstanding well enough to answer "is it safe
    /// to unmount".
    ///
    /// Only `vfs/queue` and the disk scan can. `core/stats` shows transfers that have
    /// *started*, and it lags `vfs/queue` by `--vfs-write-back` — a file written seconds
    /// ago is dirty on disk and absent from `transferring[]`, so a total taken from it
    /// reads zero while gigabytes are still queued. That is wrong in the direction that
    /// makes an unmount look safe when it is not. T3 gives counts without bytes.
    ///
    /// A build that resolves to T1 or T3 is not stuck: `vfs/queue` may still be present,
    /// and the disk scan always is. See [`Capabilities::rc_can_answer_outstanding`].
    pub fn meets_the_bar(self) -> bool {
        matches!(self, Tier::T2 | Tier::T4)
    }

    /// Whether per-file percentages are honest at this tier.
    ///
    /// Only T1 carries per-file byte progress. Showing a percentage anywhere else means
    /// inventing one, and a progress bar that actually means "we have no idea" is worse
    /// than no bar.
    pub fn has_per_file_progress(self) -> bool {
        self == Tier::T1
    }

    /// Whether this tier can mark a *particular file* as uploading rather than queued.
    ///
    /// T4 cannot: `Dirty` stays true until the upload completes, so queued and uploading
    /// are indistinguishable on disk. Neither can T3 — `vfs/stats` reports
    /// `uploadsInProgress`/`uploadsQueued` as aggregate counts and never names a file, so
    /// there is nothing there to mark.
    pub fn has_in_flight_flag(self) -> bool {
        matches!(self, Tier::T1 | Tier::T2)
    }
}

/// The rc commands one rclone process registers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    commands: BTreeSet<String>,
    /// Set when the commands are empty because rclone could not be asked, rather than
    /// because it genuinely registers none.
    degraded: Option<String>,
}

impl Capabilities {
    /// Build from the command paths `rc/list` reported.
    pub fn from_paths<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            commands: paths.into_iter().map(Into::into).collect(),
            degraded: None,
        }
    }

    /// Ask an rclone what it supports.
    ///
    /// An unreachable socket is not an error here: it resolves to no commands, which
    /// resolves to [`Tier::T4`], which is exactly right — the on-disk scan is what works
    /// when rclone is not answering.
    ///
    /// The reason is kept rather than discarded. "rclone is not running" and "rclone is
    /// running and I refuse to talk to its socket" both degrade to T4, but only the second
    /// is a misconfiguration the user can fix, and it is permanent until they do. Without
    /// the reason nothing above this can tell them the difference.
    pub async fn probe(client: &RcClient) -> Result<Self, RcError> {
        match client
            .call::<RcList>("rc/list", serde_json::json!({}))
            .await
        {
            Ok(list) => Ok(Self::from_paths(list.commands.into_iter().map(|c| c.path))),
            Err(e) if e.is_unreachable() => Ok(Self::from_refusal(e.to_string())),
            Err(e) => Err(e),
        }
    }

    /// A capability set that is empty because rclone was refused or unreachable.
    pub fn from_refusal(reason: impl Into<String>) -> Self {
        Self {
            commands: BTreeSet::new(),
            degraded: Some(reason.into()),
        }
    }

    /// Why this rclone could not be asked, when it could not be.
    ///
    /// `None` means the answer is genuine. `Some` means the tier below is a fallback, and
    /// carries the text explaining it — an insecure socket says so here rather than
    /// looking identical to a mount that is simply not up.
    pub fn degraded_reason(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    /// Whether **rc** can say what is outstanding, i.e. whether `vfs/queue` is available.
    ///
    /// Deliberately not "can the question be answered at all": the on-disk scan answers it
    /// with no rc whatsoever, so that version would be true unconditionally and tell a
    /// caller nothing. Phrased this way it is also monotonic — learning more about an
    /// rclone can only ever turn it from false to true. The earlier phrasing made a build
    /// with `vfs/stats` look *less* capable than one with no rc at all, which inverts
    /// DESIGN.md: `vfs/stats` is what hands the cache paths to the scanner.
    ///
    /// A `false` here means fall to the scan, not that the answer is unavailable.
    pub fn rc_can_answer_outstanding(&self) -> bool {
        self.has("vfs/queue")
    }

    pub fn has(&self, command: &str) -> bool {
        self.commands.contains(command)
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// The most detailed tier this rclone supports.
    pub fn tier(&self) -> Tier {
        if self.has("core/stats") {
            Tier::T1
        } else if self.has("vfs/queue") {
            Tier::T2
        } else if self.has("vfs/stats") {
            Tier::T3
        } else {
            Tier::T4
        }
    }

    /// Whether a `core/stats` transfer can be attributed to a specific mount exactly.
    ///
    /// A write-back upload is identified by `group == global_stats` **and** `srcFs`
    /// matching that mount's cache path. `vfs/stats` reports that path exactly for a
    /// given VFS; without it the path has to be composed from `rclone config paths`,
    /// which gives the cache root rather than this mount's directory. The group alone is
    /// not enough — VFS cache *downloads* share it, so filtering on it counts a file
    /// being downloaded as a pending upload.
    pub fn can_attribute_exactly(&self) -> bool {
        self.has("vfs/stats")
    }

    /// Whether an upload can be forced rather than waited out.
    pub fn can_force_upload(&self) -> bool {
        self.has("vfs/queue-set-expiry")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from `rc/list` on rclone v1.75.0 (#9), trimmed to the commands the
    /// capability ladder turns on.
    fn full_house() -> Capabilities {
        Capabilities::from_paths([
            "core/stats",
            "core/version",
            "rc/list",
            "vfs/queue",
            "vfs/queue-set-expiry",
            "vfs/stats",
            "vfs/list",
            "vfs/forget",
            "config/dump",
            "mount/listmounts",
        ])
    }

    #[test]
    fn a_full_build_resolves_to_the_most_detailed_tier() {
        let c = full_house();
        assert_eq!(c.tier(), Tier::T1);
        assert!(c.can_attribute_exactly());
        assert!(c.can_force_upload());
    }

    #[test]
    fn each_missing_command_steps_down_one_rung() {
        let without = |drop: &[&str]| {
            Capabilities::from_paths(
                ["core/stats", "vfs/queue", "vfs/stats"]
                    .into_iter()
                    .filter(|c| !drop.contains(c)),
            )
        };
        assert_eq!(without(&[]).tier(), Tier::T1);
        assert_eq!(without(&["core/stats"]).tier(), Tier::T2);
        assert_eq!(without(&["core/stats", "vfs/queue"]).tier(), Tier::T3);
        assert_eq!(
            without(&["core/stats", "vfs/queue", "vfs/stats"]).tier(),
            Tier::T4
        );
    }

    #[test]
    fn nothing_at_all_is_t4_rather_than_an_error() {
        // An rclone that is not answering is the case T4 exists for.
        let c = Capabilities::default();
        assert!(c.is_empty());
        assert_eq!(c.tier(), Tier::T4);
        assert!(c.tier().meets_the_bar());
    }

    #[test]
    fn only_the_queue_and_the_disk_scan_can_say_what_is_outstanding() {
        // T3 gives counts without bytes. T1 is the subtle one: `core/stats` shows
        // transfers that have started, and lags `vfs/queue` by `--vfs-write-back`, so a
        // total taken from it reads zero while gigabytes sit dirty — safe-looking when it
        // is not.
        assert!(Tier::T2.meets_the_bar());
        assert!(Tier::T4.meets_the_bar());
        assert!(!Tier::T3.meets_the_bar());
        assert!(!Tier::T1.meets_the_bar());
    }

    #[test]
    fn a_t1_build_can_still_answer_when_it_also_has_the_queue() {
        // The tier names the most detailed source, not the only one available.
        let c = Capabilities::from_paths(["core/stats", "vfs/queue"]);
        assert_eq!(c.tier(), Tier::T1);
        assert!(!c.tier().meets_the_bar());
        assert!(c.rc_can_answer_outstanding(), "vfs/queue is right there");

        // No rc at all: nothing to ask, and the disk scan is what answers.
        assert!(!Capabilities::default().rc_can_answer_outstanding());

        let thin = Capabilities::from_paths(["core/stats"]);
        assert!(!thin.rc_can_answer_outstanding());
    }

    #[test]
    fn a_refusal_is_distinguishable_from_an_absent_rclone() {
        // Both degrade to T4, but only one is a misconfiguration the user can fix, and it
        // stays broken until they do.
        let absent = Capabilities::default();
        assert_eq!(absent.tier(), Tier::T4);
        assert!(absent.degraded_reason().is_none());

        let refused = Capabilities::from_refusal("rc socket /run/x.sock is not private");
        assert_eq!(refused.tier(), Tier::T4);
        assert!(
            refused.degraded_reason().unwrap().contains("not private"),
            "the reason must survive so it can be shown"
        );
    }

    #[test]
    fn only_t1_may_show_a_per_file_percentage() {
        assert!(Tier::T1.has_per_file_progress());
        for t in [Tier::T2, Tier::T3, Tier::T4] {
            assert!(
                !t.has_per_file_progress(),
                "{t:?} has no per-file byte progress to derive a percentage from"
            );
        }
    }

    #[test]
    fn asking_rc_about_the_queue_is_monotonic() {
        // Learning more about an rclone must never turn this from true to false.
        assert!(!Capabilities::default().rc_can_answer_outstanding());
        assert!(!Capabilities::from_paths(["vfs/stats"]).rc_can_answer_outstanding());
        assert!(!Capabilities::from_paths(["core/stats", "vfs/stats"]).rc_can_answer_outstanding());
        assert!(Capabilities::from_paths(["vfs/queue"]).rc_can_answer_outstanding());
        assert!(full_house().rc_can_answer_outstanding());
    }

    #[test]
    fn forcing_an_upload_needs_the_endpoint_that_does_it() {
        // Offering the action without the endpoint turns a missing feature into a 404 the
        // user sees as an error.
        assert!(full_house().can_force_upload());
        assert!(
            !Capabilities::from_paths(["vfs/queue"]).can_force_upload(),
            "the queue alone cannot force an upload"
        );
        assert!(!Capabilities::default().can_force_upload());
    }

    #[test]
    fn t4_cannot_tell_queued_from_uploading() {
        // `Dirty` stays true until the upload completes, so the two are the same on disk.
        assert!(!Tier::T4.has_in_flight_flag());
        assert!(Tier::T2.has_in_flight_flag());
        assert!(Tier::T1.has_in_flight_flag());
        // T3 has counts, not files, so it has nothing to mark as in flight.
        assert!(!Tier::T3.has_in_flight_flag());
    }

    #[test]
    fn attribution_needs_vfs_stats_even_when_core_stats_is_present() {
        // Without the exact cache path, a download shows up as a pending upload — wrong
        // in the direction that makes an unmount look unsafe.
        let c = Capabilities::from_paths(["core/stats", "vfs/queue"]);
        assert_eq!(c.tier(), Tier::T1);
        assert!(!c.can_attribute_exactly());
    }

    #[test]
    fn the_captured_rc_list_resolves_to_the_top_tier() {
        // The real payload from a live rclone, not a hand-written stand-in. This is the
        // same type `tests/fixtures.rs` pins against the capture, so a wire-format change
        // fails there rather than silently degrading every mount to T4 at runtime.
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../testdata/rc-list-v1.75.0.json"
        ))
        .expect("testdata/rc-list-v1.75.0.json");
        let list: RcList = serde_json::from_str(&raw).expect("the capture should parse");
        let c = Capabilities::from_paths(list.commands.into_iter().map(|e| e.path));

        assert!(c.has("core/stats"), "the capture registers core/stats");
        assert!(c.has("vfs/queue") && c.has("vfs/stats"));
        assert_eq!(c.tier(), Tier::T1);
        assert!(c.can_attribute_exactly());
        assert!(c.can_force_upload());
    }
}
