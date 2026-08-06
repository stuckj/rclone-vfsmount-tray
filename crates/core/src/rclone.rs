//! Finding rclone and asking it about itself, without needing a running daemon.
//!
//! Everything here shells out. The rc API (#12) needs a mounted VFS to talk to; these
//! calls work before anything is mounted, which is what makes them the bootstrap path —
//! and what makes the on-disk cache tier usable with no rc at all.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sanity floor, not a compatibility claim.
///
/// Feature availability is detected with `rc/list` (#13), not inferred from a version —
/// that is the point of #13. This exists only to reject an rclone so old that nothing
/// downstream could work. Behaviour has been verified against **1.75.0** (#9).
pub const MINIMUM_VERSION: Version = Version {
    major: 1,
    minor: 60,
    patch: 0,
};

/// Locations to try when `rclone` is not on `PATH`.
const FALLBACK_PATHS: &[&str] = &[
    "/usr/bin/rclone",
    "/usr/local/bin/rclone",
    "/opt/homebrew/bin/rclone",
    "/snap/bin/rclone",
    "/var/lib/flatpak/exports/bin/rclone",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl std::str::FromStr for Version {
    type Err = ();

    /// Parses `v1.75.0`, `1.75.0`, `1.75`, and `1.75.0-beta.1234.abcdef`.
    fn from_str(s: &str) -> Result<Self, ()> {
        let s = s.trim().trim_start_matches('v');
        let s = s.split(['-', '+']).next().unwrap_or(s);
        let mut it = s.split('.');
        let major = it.next().ok_or(())?.parse().map_err(|_| ())?;
        let minor = it.next().unwrap_or("0").parse().map_err(|_| ())?;
        let patch = it.next().unwrap_or("0").parse().map_err(|_| ())?;
        Ok(Version {
            major,
            minor,
            patch,
        })
    }
}

/// Why rclone cannot be used.
///
/// Every variant is something the tray must be able to render — "rclone not usable" is a
/// state a user has to be told about and can act on, not an internal failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RcloneError {
    #[error("rclone not found (looked in: {searched})")]
    NotFound { searched: String },

    #[error("rclone at {path} could not be run: {source}")]
    NotExecutable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse a version from rclone's output: {output:?}")]
    UnparsableVersion { output: String },

    #[error("rclone {found} is too old; {minimum} or newer is required")]
    TooOld { found: Version, minimum: Version },

    #[error("`rclone {args}` failed ({status}): {stderr}")]
    CommandFailed {
        args: String,
        status: String,
        stderr: String,
    },
}

/// Where rclone keeps its own files, from `rclone config paths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
}

/// A usable rclone binary.
///
/// Constructing one proves the binary exists, runs, and is new enough; nothing further
/// down has to re-check that.
#[derive(Debug, Clone)]
pub struct Rclone {
    path: PathBuf,
    version: Version,
}

impl Rclone {
    /// Find rclone and verify it: `override_path`, then `PATH`, then a short list of
    /// well-known install locations.
    pub fn discover(override_path: Option<&Path>) -> Result<Self, RcloneError> {
        let mut searched: Vec<String> = Vec::new();

        if let Some(p) = override_path {
            searched.push(p.display().to_string());
            return Self::probe(p, &searched);
        }

        // `PATH` first: a user who installed a newer rclone expects it to win.
        if let Some(found) = Self::which("rclone") {
            searched.push(found.display().to_string());
            return Self::probe(&found, &searched);
        }
        searched.push("$PATH".to_string());

        for cand in FALLBACK_PATHS {
            let p = Path::new(cand);
            searched.push(cand.to_string());
            if p.is_file() {
                return Self::probe(p, &searched);
            }
        }

        Err(RcloneError::NotFound {
            searched: searched.join(", "),
        })
    }

    fn which(bin: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|c| c.is_file())
    }

    fn probe(path: &Path, searched: &[String]) -> Result<Self, RcloneError> {
        let out = Command::new(path)
            .arg("version")
            .output()
            .map_err(|source| RcloneError::NotExecutable {
                path: path.to_path_buf(),
                source,
            })?;
        if !out.status.success() {
            return Err(RcloneError::NotFound {
                searched: searched.join(", "),
            });
        }
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let version =
            parse_version_output(&stdout).ok_or_else(|| RcloneError::UnparsableVersion {
                output: stdout.lines().next().unwrap_or_default().to_string(),
            })?;
        if version < MINIMUM_VERSION {
            return Err(RcloneError::TooOld {
                found: version,
                minimum: MINIMUM_VERSION,
            });
        }
        Ok(Rclone {
            path: path.to_path_buf(),
            version,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> Version {
        self.version
    }

    /// `rclone config paths` — the config file and cache directory.
    ///
    /// The cache directory is what makes the on-disk tier work with no rc endpoint. Where
    /// rc *is* reachable, prefer `vfs/stats`'s `path`/`pathMeta`, which are exact for a
    /// given VFS rather than the root this composes from.
    pub fn config_paths(&self) -> Result<ConfigPaths, RcloneError> {
        let stdout = self.run(["config", "paths"])?;
        let mut config_file = None;
        let mut cache_dir = None;
        for line in stdout.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = PathBuf::from(value.trim());
            match key.trim().to_ascii_lowercase().as_str() {
                "config file" => config_file = Some(value),
                "cache dir" => cache_dir = Some(value),
                _ => {}
            }
        }
        match (config_file, cache_dir) {
            (Some(config_file), Some(cache_dir)) => Ok(ConfigPaths {
                config_file,
                cache_dir,
            }),
            _ => Err(RcloneError::CommandFailed {
                args: "config paths".into(),
                status: "unexpected output".into(),
                stderr: stdout.lines().take(4).collect::<Vec<_>>().join("; "),
            }),
        }
    }

    /// `rclone listremotes` — configured remote names, without trailing colons.
    ///
    /// The non-rc path to the same answer as `config/listremotes`, so the mount editor
    /// can offer a remote picker before anything is mounted.
    pub fn list_remotes(&self) -> Result<Vec<String>, RcloneError> {
        Ok(self
            .run(["listremotes"])?
            .lines()
            .map(|l| l.trim().trim_end_matches(':').to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    fn run<I, S>(&self, args: I) -> Result<String, RcloneError>
    where
        I: IntoIterator<Item = S> + Clone,
        S: AsRef<OsStr>,
    {
        let rendered = args
            .clone()
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let out = Command::new(&self.path)
            .args(args)
            .output()
            .map_err(|source| RcloneError::NotExecutable {
                path: self.path.clone(),
                source,
            })?;
        if !out.status.success() {
            return Err(RcloneError::CommandFailed {
                args: rendered,
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Pull the version out of `rclone version`'s first line (`rclone v1.75.0`).
fn parse_version_output(stdout: &str) -> Option<Version> {
    let first = stdout.lines().next()?;
    first.split_whitespace().find_map(|w| w.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(
            "v1.75.0".parse(),
            Ok(Version {
                major: 1,
                minor: 75,
                patch: 0
            })
        );
        assert_eq!(
            "1.75".parse(),
            Ok(Version {
                major: 1,
                minor: 75,
                patch: 0
            })
        );
        // rclone's beta builds carry a suffix.
        assert_eq!(
            "v1.76.0-beta.8443.abc123".parse(),
            Ok(Version {
                major: 1,
                minor: 76,
                patch: 0
            })
        );
        assert!("".parse::<Version>().is_err());
        assert!("not-a-version".parse::<Version>().is_err());

        let old: Version = "1.59.9".parse().unwrap();
        let new: Version = "1.75.0".parse().unwrap();
        assert!(old < MINIMUM_VERSION && new > MINIMUM_VERSION);
    }

    #[test]
    fn version_is_taken_from_the_first_line() {
        let real = "rclone v1.75.0\n- os/version: ubuntu 26.04 (64 bit)\n- go/version: go1.26.5\n";
        assert_eq!(
            parse_version_output(real),
            Some(Version {
                major: 1,
                minor: 75,
                patch: 0
            })
        );
        // go1.26.5 on a later line must not be mistaken for rclone's version.
        assert_ne!(
            parse_version_output(real),
            Some(Version {
                major: 1,
                minor: 26,
                patch: 5
            })
        );
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn error_messages_name_what_to_do_about_it() {
        let e = RcloneError::TooOld {
            found: "1.50.0".parse().unwrap(),
            minimum: MINIMUM_VERSION,
        };
        let msg = e.to_string();
        assert!(
            msg.contains("1.50.0") && msg.contains(&MINIMUM_VERSION.to_string()),
            "{msg}"
        );

        let e = RcloneError::NotFound {
            searched: "$PATH, /usr/bin/rclone".into(),
        };
        assert!(
            e.to_string().contains("/usr/bin/rclone"),
            "should list where it looked"
        );
    }
}
