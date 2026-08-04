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

use std::fmt;

/// What the supervisor believes the state of a mount to be.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub fn is_live(&self) -> bool {
        matches!(self, MountState::Mounted | MountState::Foreign)
    }

    /// Whether this supervisor owns the mount and may act on it.
    pub fn is_managed(&self) -> bool {
        !matches!(self, MountState::Foreign)
    }
}

/// Why a supervisor operation failed.
#[derive(Debug)]
pub enum SupervisorError {
    /// No mount is configured under that name.
    UnknownMount(String),
    /// The mount point is unusable — missing, not a directory, not writable.
    BadMountPoint { path: String, reason: String },
    /// rclone could not be started, or exited during startup. Carries whatever it
    /// said on stderr.
    RcloneFailed { reason: String },
    /// The unmount was refused because the write-back cache still holds unuploaded
    /// data. Callers may retry with force, having told the user what that costs.
    PendingUploads { files: u64, bytes: u64 },
    /// The mount point is busy and could not be released.
    Busy { path: String },
    /// Talking to the init system failed.
    Supervision(String),
    /// We do not manage this mount, so we will not act on it.
    NotManaged(String),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMount(n) => write!(f, "no mount configured named {n:?}"),
            Self::BadMountPoint { path, reason } => {
                write!(f, "mount point {path:?} is unusable: {reason}")
            }
            Self::RcloneFailed { reason } => write!(f, "rclone failed to start: {reason}"),
            Self::PendingUploads { files, bytes } => write!(
                f,
                "{files} file(s) totalling {bytes} bytes are still waiting to upload"
            ),
            Self::Busy { path } => write!(f, "mount point {path:?} is busy"),
            Self::Supervision(m) => write!(f, "init system error: {m}"),
            Self::NotManaged(n) => write!(f, "mount {n:?} was not started by us"),
        }
    }
}

impl std::error::Error for SupervisorError {}

/// Starts and stops rclone mounts.
///
/// Implementations are selected by the service at construction; the trait is not
/// object-safe (the methods are `async fn`), so callers should be generic over it
/// rather than holding a `Box<dyn MountSupervisor>`.
pub trait MountSupervisor {
    /// Bring up a mount. Returns once the mount point is actually serving, not once
    /// rclone has been spawned — the two are several seconds apart and the
    /// difference is user-visible.
    fn mount(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send;

    /// Tear down a mount.
    ///
    /// Must refuse with [`SupervisorError::PendingUploads`] when the write-back
    /// cache still holds unuploaded data, unless `force` is set. `force` is always
    /// an explicit caller decision — never a default, and never inferred.
    fn unmount(
        &self,
        name: &str,
        force: bool,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send;

    /// Current state of one mount.
    fn state(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<MountState, SupervisorError>> + Send;

    /// Reconcile against reality on startup.
    ///
    /// The service may have been restarted while mounts stayed up, so it must
    /// discover and adopt what is already mounted rather than assuming a blank
    /// slate. Returns every mount found live, including [`MountState::Foreign`] ones
    /// that we did not start.
    fn reconcile(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<(String, MountState)>, SupervisorError>> + Send;
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
}
