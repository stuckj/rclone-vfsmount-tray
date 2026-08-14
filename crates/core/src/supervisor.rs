//! Starting and stopping rclone mounts.
//!
//! **A mount's lifetime is not tied to the process that started it.** Restarting or
//! killing the service must leave mounts up; nothing a client does may unmount anything.
//! So: no unmounting from `Drop`, no putting rclone in the service's own cgroup, and
//! [`MountSupervisor::unmount`] is the only path to an unmount. See DESIGN.md.

use crate::models::Pending;
use std::future::Future;
use std::pin::Pin;

/// Render a pending-upload summary that never presents a floor as a total.
fn pending_summary(p: &Pending) -> String {
    if p.is_exact() {
        format!(
            "{} file(s) totalling {} bytes are still waiting to upload",
            p.files, p.known_bytes
        )
    } else {
        format!(
            "{} file(s) are still waiting to upload — at least {} bytes, \
             including {} of unknown size",
            p.files, p.known_bytes, p.unknown_size_files
        )
    }
}

/// A boxed future, so the trait below stays usable as `dyn MountSupervisor`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the supervisor believes the state of a mount to be.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountState {
    /// Not mounted, and nothing is trying to.
    Unmounted,
    /// A mount attempt is in progress.
    Mounting,
    /// Mounted and serving.
    Mounted,
    /// Being torn down.
    Unmounting,
    /// The mount attempt failed. Carries the reason — usually the tail of rclone's
    /// stderr, because a bare "mount failed" is not actionable.
    Failed { reason: String },
    /// Mounted, but we did not start it — an external systemd unit, a shell script,
    /// a manual invocation. Adopted for display and monitoring only; the supervisor
    /// must not restart or reconfigure it.
    Foreign,
    /// Serving, started by us, and named by no entry in the config — the mount was
    /// renamed or deleted while its unit kept running.
    ///
    /// Stoppable, because the unit is ours. Not startable: nothing left describes what
    /// it should be, so taking it down is all that can be done with one.
    Orphaned,
}

impl MountState {
    /// Whether the mount point is currently serving, however it got there.
    ///
    /// Matched exhaustively so a future variant cannot silently default to "not live".
    pub fn is_live(&self) -> bool {
        match self {
            MountState::Mounted | MountState::Foreign | MountState::Orphaned => true,
            MountState::Unmounted
            | MountState::Mounting
            | MountState::Unmounting
            | MountState::Failed { .. } => false,
        }
    }

    /// Whether this supervisor owns the mount and may act on it.
    ///
    /// Matched exhaustively so a future not-managed variant cannot default to managed.
    pub fn is_managed(&self) -> bool {
        match self {
            MountState::Unmounted
            | MountState::Mounting
            | MountState::Mounted
            | MountState::Unmounting
            | MountState::Failed { .. }
            | MountState::Orphaned => true,
            MountState::Foreign => false,
        }
    }
}

/// A boxed underlying cause, preserved so `source()` chains survive to the log.
pub type Cause = Box<dyn std::error::Error + Send + Sync>;

/// Why a supervisor operation failed.
///
/// `#[non_exhaustive]` because this will grow — adding a variant should not be a
/// breaking change for anything matching on it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// Nothing answers to that name: no config entry, and no [`MountState::Orphaned`]
    /// unit either.
    #[error("no mount configured named {0:?}")]
    UnknownMount(String),

    /// The mount point is unusable — missing, not a directory, not writable.
    #[error("mount point {path:?} is unusable: {reason}")]
    BadMountPoint {
        path: String,
        reason: String,
        #[source]
        source: Option<Cause>,
    },

    /// rclone could not be started, or exited during startup. `reason` should carry
    /// the tail of its stderr — a bare "mount failed" is not actionable.
    #[error("rclone failed to start: {reason}")]
    RcloneFailed {
        reason: String,
        #[source]
        source: Option<Cause>,
    },

    /// The unmount was refused because the write-back cache still holds unuploaded
    /// data. Callers may retry with force, having told the user what that costs.
    ///
    /// Carries [`Pending`] rather than a byte count: unsized files contribute nothing, so
    /// one number would render three of them as "totalling 0 bytes".
    #[error("{}", pending_summary(.0))]
    PendingUploads(Pending),

    /// The mount point could not be released — usually a process still using it: an open
    /// file, a read-only descriptor or a working directory inside it all count. The only
    /// signal an in-progress write gives.
    ///
    /// Rendered verbatim, so `detail` is the whole message.
    #[error("{detail}")]
    Busy { detail: String },

    /// Talking to the init system failed.
    #[error("init system error: {context}")]
    Supervision {
        context: String,
        #[source]
        source: Option<Cause>,
    },

    /// We do not manage this mount, so we will not act on it.
    #[error("mount {0:?} was not started by us")]
    NotManaged(String),
}

/// One mount as found by [`MountSupervisor::reconcile`].
///
/// Built via [`DiscoveredMount::new`] — `#[non_exhaustive]` rejects struct literals
/// outside this crate, and this trait exists to be implemented outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiscoveredMount {
    /// Configured name, or a derived one for mounts we did not start.
    pub name: String,
    pub state: MountState,
}

impl DiscoveredMount {
    pub fn new(name: impl Into<String>, state: MountState) -> Self {
        Self {
            name: name.into(),
            state,
        }
    }
}

/// Starts and stops rclone mounts.
///
/// Futures are boxed so the trait stays dyn-compatible: the implementation is chosen at
/// runtime and tests substitute a double. These operations fire at human frequency, so
/// the allocation is irrelevant.
///
/// Implementations must uphold the lifetime rule in the module docs.
pub trait MountSupervisor: Send + Sync {
    /// Bring up a mount. Resolves once the mount point is actually serving, not once
    /// rclone has been spawned — the two are several seconds apart and the
    /// difference is user-visible.
    fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>>;

    /// Tear down a mount.
    ///
    /// [`SupervisorError::Busy`] — the kernel would not release the point, which is what
    /// an in-progress write looks like from outside: rclone queues a file only when it is
    /// *closed*, so its rc API never reports one. **Forcing past it loses data.** Ask the
    /// kernel *before* signalling rclone, or the refusal never happens (#73).
    ///
    /// [`SupervisorError::PendingUploads`] — data still to send, but on disk and resumed
    /// on remount, so a warning rather than a veto (#19). Nothing here implements it yet.
    ///
    /// `force` is always an explicit caller decision, never inferred.
    ///
    /// The name may be a [`MountState::Orphaned`] one, since [`Self::reconcile`] reports
    /// those and taking one down is the only thing left to do with it.
    fn unmount<'a>(
        &'a self,
        name: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), SupervisorError>>;

    /// Current state of one mount, configured or [`MountState::Orphaned`].
    fn state<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<MountState, SupervisorError>>;

    /// Reconcile against reality on startup — the service may have restarted while mounts
    /// stayed up.
    ///
    /// Returns every configured mount, plus a row for every live mount no unit of ours is
    /// serving where the config puts it: [`MountState::Foreign`] for the ones we did not
    /// start, [`MountState::Orphaned`] for our own units left running under a name the
    /// config has since renamed or dropped. A rename therefore reports the path twice —
    /// once for the config entry that wants it, once for the unit that still holds it.
    /// A configured mount that is down is reported [`MountState::Unmounted`], not
    /// omitted.
    ///
    /// **One name, one row.** A unit left behind by a changed `mount_point` is still named
    /// by its own entry, so it is reported once under that name as
    /// [`MountState::Failed`], and the path it is holding gets no row of its own (#90).
    /// A client offering "stop the leftover unit" therefore cannot key that on
    /// [`MountState::Orphaned`] alone — a `Failed` row can need the same gesture, and its
    /// reason names the unit and the path.
    fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_mounts_are_live_but_not_managed() {
        assert!(MountState::Foreign.is_live());
        assert!(!MountState::Foreign.is_managed());
        assert!(MountState::Mounted.is_live());
        assert!(MountState::Mounted.is_managed());
    }

    #[test]
    fn an_orphaned_unit_is_live_and_ours_to_act_on() {
        // The whole point of telling it from `Foreign`: it can be stopped by stopping
        // its unit, where a foreign mount can only be fusermounted.
        assert!(MountState::Orphaned.is_live());
        assert!(MountState::Orphaned.is_managed());
    }

    #[test]
    fn failed_and_transitional_states_are_not_live() {
        for s in [
            MountState::Unmounted,
            MountState::Mounting,
            MountState::Unmounting,
            MountState::Failed {
                reason: "boom".into(),
            },
        ] {
            assert!(!s.is_live(), "{s:?} must not report as live");
        }
    }

    #[test]
    fn pending_upload_error_states_the_cost() {
        let e = SupervisorError::PendingUploads(Pending {
            files: 3,
            known_bytes: 1_288_490_188,
            unknown_size_files: 0,
        });
        let msg = e.to_string();
        assert!(msg.contains('3') && msg.contains("1288490188"), "{msg}");
    }

    #[test]
    fn pending_upload_error_never_presents_a_floor_as_a_total() {
        // Three unsized files must not read as "totalling 0 bytes" — that is what a
        // user sees immediately before deciding whether to force an unmount.
        let e = SupervisorError::PendingUploads(Pending {
            files: 3,
            known_bytes: 0,
            unknown_size_files: 3,
        });
        let msg = e.to_string();
        assert!(!msg.contains("totalling"), "{msg}");
        assert!(msg.contains("at least"), "{msg}");
        assert!(msg.contains("unknown"), "{msg}");
    }

    #[test]
    fn errors_preserve_their_cause_chain() {
        use std::error::Error;
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no");
        let e = SupervisorError::Supervision {
            context: "starting unit".into(),
            source: Some(Box::new(io)),
        };
        assert!(
            e.source().is_some(),
            "a flattened string leaves nothing to debug from a log"
        );
        assert!(e.to_string().contains("starting unit"));
    }

    /// The trait must stay usable as `dyn`, so the implementation can be chosen at
    /// runtime and so tests can substitute a double. This is a compile-time
    /// assertion; if it stops building, the trait has become dyn-incompatible.
    #[test]
    fn supervisor_trait_is_dyn_compatible() {
        struct Never;
        impl MountSupervisor for Never {
            fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
                Box::pin(async move { Err(SupervisorError::UnknownMount(name.into())) })
            }
            fn unmount<'a>(
                &'a self,
                name: &'a str,
                _force: bool,
            ) -> BoxFuture<'a, Result<(), SupervisorError>> {
                Box::pin(async move { Err(SupervisorError::UnknownMount(name.into())) })
            }
            fn state<'a>(
                &'a self,
                name: &'a str,
            ) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
                Box::pin(async move { Err(SupervisorError::UnknownMount(name.into())) })
            }
            fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
        }
        let _erased: Box<dyn MountSupervisor> = Box::new(Never);
    }
}
