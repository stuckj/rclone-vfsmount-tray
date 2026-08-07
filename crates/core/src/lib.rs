//! Core logic for `rclone-vfsmount-tray`: [`config`] (the applet's own settings),
//! [`models`] (rc API and cache metadata), [`rclone`] (finding and querying the binary),
//! [`supervisor`] (mount lifecycle), and [`resolve_log_filter`].
//!
//! Pure Rust — no system C libraries — so CI can lint and test it on a bare runner.
//! The cache scanner (#22) lands here too.

pub mod config;
pub mod models;
pub mod mountinfo;
pub mod rclone;
pub mod supervisor;

pub use config::{CacheMode, Config, ConfigError, Mount};
pub use models::Pending;
pub use rclone::{Rclone, RcloneError};
pub use supervisor::{
    BoxFuture, Cause, DiscoveredMount, MountState, MountSupervisor, SupervisorError,
};

/// Resolve a log filter: explicit flag, else `RUST_LOG`, else `info`.
///
/// The flag must be a bare level. `EnvFilter` reads an unknown word as a *target*
/// matching nothing, which silences the process — so validate, and return what was
/// validated. Returns the offending value on rejection.
pub fn resolve_log_filter(flag: Option<&str>, env: Option<String>) -> Result<String, String> {
    // `EnvFilter` does not trim, and it does not reject. A padded value parses as a
    // *target* literally named `" info "`, matches nothing, and silences the process
    // — no output, no warning, exit 0. A blank one parses to no directive at all and
    // falls back to EnvFilter's own ERROR default, losing every info and warn line.
    //
    // This function has already shipped that bug twice: once on the flag door, then
    // again on the environment door, both times by trimming to *decide* and returning
    // the original. So each door now trims once, up front, and returns what it
    // validated. Do not reintroduce a `.trim()` that feeds only a condition.
    if let Some(raw) = flag {
        let level = raw.trim();
        if !matches!(
            level.to_ascii_lowercase().as_str(),
            "off" | "error" | "warn" | "info" | "debug" | "trace"
        ) {
            // Report the value as given: echoing a trimmed version back at someone
            // who typed trailing whitespace hides the actual mistake.
            return Err(raw.to_string());
        }
        return Ok(level.to_string());
    }

    Ok(env
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "info".to_string()))
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
    fn a_set_but_empty_rust_log_is_treated_as_unset() {
        // Passing "" through leaves EnvFilter with no valid directive, so it falls
        // back to its own ERROR default and the service loses every info and warn
        // line. Not silence, but not what the operator asked for either.
        assert_eq!(
            resolve_log_filter(None, Some(String::new())).unwrap(),
            "info"
        );
        assert_eq!(
            resolve_log_filter(None, Some("   ".into())).unwrap(),
            "info"
        );
    }

    #[test]
    fn a_padded_rust_log_is_trimmed_not_merely_accepted() {
        // The regression this function shipped twice: trimming to decide, returning
        // the original. `EnvFilter` reads `" info "` as a target named `" info "`,
        // matches nothing, and produces no output at all.
        assert_eq!(
            resolve_log_filter(None, Some(" info ".into())).unwrap(),
            "info"
        );
        assert_eq!(
            resolve_log_filter(None, Some(" rvt_core=debug,zbus=warn\n".into())).unwrap(),
            "rvt_core=debug,zbus=warn"
        );
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
