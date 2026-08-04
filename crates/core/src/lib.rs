//! Core logic for `rclone-vfsmount-tray`.
//!
//! This crate holds everything that is neither a tray icon nor a window: the typed
//! models for rclone's remote-control (rc) API, the VFS cache scanner, and the
//! configuration model.
//!
//! It is deliberately pure Rust — no system C libraries — so that CI can lint and
//! test it on a bare runner.

pub mod models;
pub mod supervisor;

pub use supervisor::{MountState, MountSupervisor, SupervisorError};
