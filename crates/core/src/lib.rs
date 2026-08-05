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

pub use models::Pending;
pub use supervisor::{
    BoxFuture, Cause, DiscoveredMount, MountState, MountSupervisor, SupervisorError,
};

/// Resolve a log filter directive from an explicit flag, `RUST_LOG`, or the default.
///
/// The flag is validated as a bare level and rejected otherwise, which is not
/// pedantry: `tracing_subscriber`'s `EnvFilter` grammar treats an unrecognised bare
/// word as a *target* filter, so `--log-level verbose` parses successfully, matches
/// no target, and silences the process completely — no output, no warning, exit code
/// zero. A typo must not turn a service into a silent one.
///
/// `RUST_LOG` keeps the full directive grammar, which is what people expect of it,
/// and the explicit flag takes precedence over it per CLI convention.
///
/// Returns the directive string to hand to `EnvFilter`, or the invalid level.
pub fn resolve_log_filter(flag: Option<&str>, env: Option<String>) -> Result<String, String> {
    match flag {
        Some(level) => {
            let normalised = level.trim();
            if !matches!(
                normalised.to_ascii_lowercase().as_str(),
                "off" | "error" | "warn" | "info" | "debug" | "trace"
            ) {
                return Err(level.to_string());
            }
            // Return the TRIMMED value. Validating a trimmed string and then handing
            // back the original would accept `" info "`, which `EnvFilter` reads as a
            // target filter matching nothing — reintroducing, in the very function
            // written to prevent it, the silent-logging bug described above.
            Ok(normalised.to_string())
        }
        None => Ok(env.unwrap_or_else(|| "info".to_string())),
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::resolve_log_filter;

    #[test]
    fn explicit_level_beats_the_environment() {
        let got = resolve_log_filter(Some("debug"), Some("trace".into())).unwrap();
        assert_eq!(got, "debug", "an explicit flag must win over RUST_LOG");
    }

    #[test]
    fn env_is_used_when_no_flag_and_keeps_directive_grammar() {
        let got = resolve_log_filter(None, Some("rvt_core=debug,zbus=warn".into())).unwrap();
        assert_eq!(got, "rvt_core=debug,zbus=warn");
        assert_eq!(resolve_log_filter(None, None).unwrap(), "info");
    }

    #[test]
    fn a_typo_is_rejected_rather_than_silencing_everything() {
        // These all parse as EnvFilter *targets* matching nothing, which is why they
        // must be caught here: passing them through disables logging silently.
        for bad in ["verbose", "inf", "not-a-level", "trace,foo=debug", ""] {
            assert!(
                resolve_log_filter(Some(bad), None).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_normalised_away() {
        // Not merely accepted — the returned directive must be the trimmed form.
        // `" info "` reaches EnvFilter as a target filter matching nothing, so
        // returning it verbatim would silence the process exactly as a typo would.
        assert_eq!(resolve_log_filter(Some(" info "), None).unwrap(), "info");
        assert_eq!(
            resolve_log_filter(Some("\tdebug\n"), None).unwrap(),
            "debug"
        );
    }

    #[test]
    fn levels_are_case_insensitive() {
        for good in ["off", "ERROR", "Warn", "info", "Debug", "TRACE"] {
            assert!(resolve_log_filter(Some(good), None).is_ok(), "{good:?}");
        }
    }
}
