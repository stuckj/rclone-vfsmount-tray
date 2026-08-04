//! The abstraction over "how does an rclone mount get started and stopped".
//!
//! The concrete implementation is decided in `DESIGN.md` — one rclone process per
//! mount, each run as a **transient systemd user unit** that the service starts over
//! systemd's D-Bus API. The trait exists so that decision stays reversible, and so
//! that the rest of the service is written against an interface rather than against
//! `systemd-run`.
//!
//! # The invariant every implementation must uphold
//!
//! **A mount's lifetime is not tied to the lifetime of the process that started it.**
//!
//! Stopping, restarting or killing the service must leave mounts exactly as they
//! were, because a package upgrade restarts the service and nobody expects
//! `apt upgrade` to unmount their filesystems. Clients (the tray, the GTK windows)
//! are further removed still: nothing they do, including exiting, may unmount
//! anything.
//!
//! Concretely, an implementation must not unmount from a `Drop` impl, must not put
//! rclone in the service's own process group or cgroup, and must treat
//! [`MountSupervisor::unmount`] as the *only* path to an unmount.

use std::future::Future;
use std::pin::Pin;

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
}

impl MountState {
    /// Whether the mount point is currently serving, however it got there.
    ///
    /// Matched exhaustively for the same reason as [`Self::is_managed`]: a future
    /// variant defaulting to "not live" would hide a real mount from the
    /// pending-uploads check.
    pub fn is_live(&self) -> bool {
        match self {
            MountState::Mounted | MountState::Foreign => true,
            MountState::Unmounted
            | MountState::Mounting
            | MountState::Unmounting
            | MountState::Failed { .. } => false,
        }
    }

    /// Whether this supervisor owns the mount and may act on it.
    ///
    /// Matched exhaustively on purpose: a `!matches!(self, Foreign)` shorthand would
    /// silently report any future not-managed variant as managed, which is the
    /// direction that gets someone's filesystem unmounted by a tool that never owned
    /// it.
    pub fn is_managed(&self) -> bool {
        match self {
            MountState::Unmounted
            | MountState::Mounting
            | MountState::Mounted
            | MountState::Unmounting
            | MountState::Failed { .. } => true,
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
    /// No mount is configured under that name.
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
    #[error("{files} file(s) totalling {bytes} bytes are still waiting to upload")]
    PendingUploads { files: u64, bytes: u64 },

    /// The mount point is busy and could not be released.
    #[error("mount point {path:?} is busy")]
    Busy { path: String },

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
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiscoveredMount {
    /// Configured name, or a derived one for mounts we did not start.
    pub name: String,
    pub state: MountState,
}

/// Starts and stops rclone mounts.
///
/// # Why the futures are boxed
///
/// The methods return [`BoxFuture`] rather than `impl Future`. Returning
/// `impl Future` would make this trait dyn-incompatible, and that costs more than
/// the allocation saves:
///
/// - The implementation is chosen at *runtime* — systemd where it is available,
///   something else where it is not. That needs `Box<dyn MountSupervisor>`.
/// - Without type erasure, every consumer becomes generic over `S: MountSupervisor`,
///   including the zbus interface objects registered on the object server, with no
///   escape hatch when that gets unwieldy.
/// - Test doubles and the lifetime tests want erasure too.
///
/// These operations mount filesystems and fire at human frequency. One boxed future
/// per call is not a cost worth optimising, and RPITIT would be optimising the one
/// axis that does not matter here.
///
/// # The invariant
///
/// Implementations must uphold the lifetime rule in the module documentation: a
/// mount's lifetime is not tied to the process that started it, and nothing a client
/// does may unmount anything.
pub trait MountSupervisor: Send + Sync {
    /// Bring up a mount. Resolves once the mount point is actually serving, not once
    /// rclone has been spawned — the two are several seconds apart and the
    /// difference is user-visible.
    fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>>;

    /// Tear down a mount.
    ///
    /// Must refuse with [`SupervisorError::PendingUploads`] when the write-back
    /// cache still holds unuploaded data, unless `force` is set. `force` is always
    /// an explicit caller decision — never a default, and never inferred.
    fn unmount<'a>(
        &'a self,
        name: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), SupervisorError>>;

    /// Current state of one mount.
    fn state<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<MountState, SupervisorError>>;

    /// Reconcile against reality on startup.
    ///
    /// The service may have been restarted while mounts stayed up, so it must
    /// discover and adopt what is already mounted rather than assuming a blank
    /// slate. Returns every mount found live, including [`MountState::Foreign`] ones
    /// that we did not start.
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
        let e = SupervisorError::PendingUploads {
            files: 3,
            bytes: 1_288_490_188,
        };
        let msg = e.to_string();
        assert!(msg.contains('3') && msg.contains("1288490188"), "{msg}");
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
