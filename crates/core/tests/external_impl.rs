//! Prove that `MountSupervisor` can actually be implemented from another crate.
//!
//! This file exists because it once could not be. `DiscoveredMount` and `Pending`
//! are `#[non_exhaustive]`, which forbids struct literals outside the defining
//! crate — so `reconcile` returned a type the implementer had no way to construct,
//! and `unmount` could not build the error it is required to return. The trait whose
//! entire purpose is to be implemented by `crates/service` was unimplementable, and
//! every unit test passed, because unit tests live *inside* `rvt-core` and are exempt
//! from that rule.
//!
//! Integration tests are compiled as a separate crate, so this file is subject to the
//! same rules as a real consumer. It is a compile-time assertion first and a
//! behavioural test second: if `rvt-core` ever again exposes a type that an
//! implementer cannot build, this stops compiling.
//!
//! Keep it written the way `crates/service` will write it — constructors, not
//! literals, and no `pub(crate)` shortcuts.

use rvt_core::models::Pending;
use rvt_core::{BoxFuture, DiscoveredMount, MountState, MountSupervisor, SupervisorError};

/// A stand-in for the systemd supervisor that #17 will add.
struct FakeSupervisor {
    mounts: Vec<(String, MountState)>,
}

impl MountSupervisor for FakeSupervisor {
    fn mount<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), SupervisorError>> {
        Box::pin(async move {
            if self.mounts.iter().any(|(n, _)| n == name) {
                Ok(())
            } else {
                Err(SupervisorError::UnknownMount(name.to_string()))
            }
        })
    }

    fn unmount<'a>(
        &'a self,
        _name: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), SupervisorError>> {
        Box::pin(async move {
            if force {
                return Ok(());
            }
            // The refusal every real implementation must be able to produce.
            Err(SupervisorError::PendingUploads(Pending::new(3, 1_024, 1)))
        })
    }

    fn state<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<MountState, SupervisorError>> {
        Box::pin(async move {
            self.mounts
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| s.clone())
                .ok_or_else(|| SupervisorError::UnknownMount(name.to_string()))
        })
    }

    fn reconcile(&self) -> BoxFuture<'_, Result<Vec<DiscoveredMount>, SupervisorError>> {
        Box::pin(async move {
            Ok(self
                .mounts
                .iter()
                .map(|(n, s)| DiscoveredMount::new(n.clone(), s.clone()))
                .collect())
        })
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    // A minimal executor, so this test needs no async runtime dependency. The futures
    // here never actually pend, so a no-op waker is sufficient.
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut f = Box::pin(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn fake() -> FakeSupervisor {
    FakeSupervisor {
        mounts: vec![
            ("photos".into(), MountState::Mounted),
            ("backup".into(), MountState::Unmounted),
            ("someone-elses".into(), MountState::Foreign),
        ],
    }
}

#[test]
fn the_trait_is_implementable_from_another_crate() {
    // Reaching this line at all is the assertion: the file compiled.
    let s = fake();
    assert!(block_on(s.mount("photos")).is_ok());
    assert!(matches!(
        block_on(s.mount("nope")),
        Err(SupervisorError::UnknownMount(_))
    ));
}

#[test]
fn the_trait_is_dyn_compatible_from_another_crate() {
    // The in-crate test asserts this too, but a consumer is what actually matters:
    // the service picks its supervisor at runtime.
    let erased: Box<dyn MountSupervisor> = Box::new(fake());
    assert_eq!(
        block_on(erased.state("photos")).unwrap(),
        MountState::Mounted
    );
}

#[test]
fn an_implementer_can_build_every_type_it_must_return() {
    let s = fake();

    // DiscoveredMount, via the constructor.
    let found = block_on(s.reconcile()).unwrap();
    assert_eq!(found.len(), 3);
    assert!(found.iter().any(|m| m.state == MountState::Foreign));
    // Configured-but-down mounts are reported, not omitted — see reconcile's docs.
    assert!(found
        .iter()
        .any(|m| m.name == "backup" && m.state == MountState::Unmounted));

    // Pending, via the constructor, inside the error unmount must be able to return.
    match block_on(s.unmount("photos", false)) {
        Err(SupervisorError::PendingUploads(p)) => {
            assert_eq!(p.files, 3);
            assert_eq!(p.known_bytes, 1_024);
            assert!(!p.is_exact(), "one file had no size");
            // The message must not present a floor as a total.
            let msg = SupervisorError::PendingUploads(p).to_string();
            assert!(msg.contains("at least"), "{msg}");
        }
        other => panic!("expected a pending-uploads refusal, got {other:?}"),
    }

    assert!(
        block_on(s.unmount("photos", true)).is_ok(),
        "force overrides"
    );
}

/// Every `MountState` and `SupervisorError` an implementer must be able to *return*,
/// constructed from outside the crate.
///
/// This is the compile-time half of this file's job, and it is enumerated one variant
/// at a time on purpose. The first version of this file covered two of seven error
/// variants and three of six states — marking `SupervisorError::Supervision`
/// `#[non_exhaustive]` (exactly the change that broke `DiscoveredMount` and `Pending`,
/// and exactly what a future reviewer will suggest) left every test passing. A guard
/// that covers a third of its surface is the failure this file exists to prevent.
#[test]
fn an_implementer_can_construct_every_state_and_error() {
    let states = [
        MountState::Unmounted,
        MountState::Mounting,
        MountState::Mounted,
        MountState::Unmounting,
        MountState::Failed {
            reason: "rclone exited 1: mount point busy".into(),
        },
        MountState::Foreign,
    ];
    assert_eq!(
        states.iter().filter(|s| s.is_live()).count(),
        2,
        "only Mounted and Foreign are serving"
    );
    assert_eq!(
        states.iter().filter(|s| s.is_managed()).count(),
        5,
        "only Foreign is unmanaged"
    );

    let errors = [
        SupervisorError::UnknownMount("photos".into()),
        SupervisorError::BadMountPoint {
            path: "/mnt/photos".into(),
            reason: "not a directory".into(),
            source: None,
        },
        SupervisorError::RcloneFailed {
            reason: "exit status 1".into(),
            source: Some(Box::new(std::io::Error::other("spawn failed"))),
        },
        SupervisorError::PendingUploads(Pending::new(3, 1_024, 1)),
        SupervisorError::Busy {
            path: "/mnt/photos".into(),
        },
        SupervisorError::Supervision {
            context: "StartTransientUnit".into(),
            source: Some(Box::new(std::io::Error::other("no session bus"))),
        },
        SupervisorError::NotManaged("someone-elses".into()),
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty(), "{e:?} rendered as nothing");
    }

    // The cause chain must survive to a log, which is the point of carrying a source.
    let with_source = &errors[2];
    assert!(
        std::error::Error::source(with_source).is_some(),
        "RcloneFailed must expose its underlying cause"
    );
}
