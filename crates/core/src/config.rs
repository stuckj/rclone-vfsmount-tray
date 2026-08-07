//! The applet's own configuration: which mounts it manages, and how it behaves.
//!
//! Distinct from rclone's config, which holds the remotes. This file references those by
//! name; it never duplicates them and never contains credentials.
//!
//! Until the GTK editor lands (#42) this file *is* the configuration UI, so it is meant
//! to be hand-edited. `config.example.toml` is the annotated reference.
//!
//! The service is the only writer — clients go through D-Bus (#40). Two processes doing
//! read-modify-write on one TOML file loses edits.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Schema version of the on-disk file.
///
/// Present from the first release so that a later change has somewhere to branch on. A
/// file from the future is refused rather than guessed at — an older build silently
/// dropping fields it does not understand is how configuration gets eaten.
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub global: Global,
    #[serde(default, rename = "mount")]
    pub mounts: Vec<Mount>,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for Config {
    /// `#[serde(default = ...)]` only applies when deserialising, so a derived `Default`
    /// would give `version: 0` — and that is the value a fresh install saves.
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            global: Global::default(),
            mounts: Vec::new(),
        }
    }
}

/// Reads `version` and nothing else.
///
/// The real parse uses `deny_unknown_fields`, so a v2 file that adds a field dies there —
/// reporting "not valid TOML", which is both unhelpful and untrue — before the version
/// check could produce an actionable error. The version has to be read by something that
/// tolerates fields this build has never heard of.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default = "default_version")]
    version: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Global {
    /// Use this rclone instead of searching `PATH`.
    pub rclone_path: Option<PathBuf>,
    /// Override rclone's cache directory. Normally taken from `rclone config paths`.
    pub cache_dir: Option<PathBuf>,
    /// Unmount everything when the service stops.
    ///
    /// **Off by default, deliberately.** A package upgrade restarts the service, and
    /// `apt upgrade` must not unmount anything (#54).
    pub unmount_on_service_stop: bool,
    pub poll: Poll,
    pub notifications: Notifications,
}

/// How often to ask rclone for state, in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Poll {
    /// While something is uploading or downloading.
    pub active_secs: u64,
    /// While nothing is happening. Higher, so an idle mount costs nothing.
    pub idle_secs: u64,
}

impl Default for Poll {
    fn default() -> Self {
        Self {
            active_secs: 1,
            idle_secs: 15,
        }
    }
}

/// Which events are worth interrupting someone for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Notifications {
    /// Everything queued for a mount has finished uploading — the "safe to unplug" signal.
    pub uploads_complete: bool,
    /// An upload failed or ran out of retries.
    pub upload_errors: bool,
    /// A mount failed, or died and could not be restarted.
    pub mount_failures: bool,
    /// The cache is full. Silently breaks uploads, so on by default.
    pub cache_full: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            uploads_complete: true,
            upload_errors: true,
            mount_failures: true,
            cache_full: true,
        }
    }
}

/// rclone's `--vfs-cache-mode`.
///
/// This is the most consequential per-mount setting: `off` and `minimal` bypass the
/// write-back cache for write-only opens, so those writes stream straight through and no
/// tier can attribute them to a mount. See DESIGN.md.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    Off,
    Minimal,
    #[default]
    Writes,
    Full,
}

impl CacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::Off => "off",
            CacheMode::Minimal => "minimal",
            CacheMode::Writes => "writes",
            CacheMode::Full => "full",
        }
    }

    /// Whether this mount has a write-back cache at all — whether `vfs/queue`,
    /// `vfs/stats` and the cache scanner can report anything for it.
    ///
    /// rclone builds the cache, and its queue, for any mode above `off`
    /// (`vfs.SetCacheMode`: `cacheMode > CacheModeOff`).
    pub fn has_writeback(self) -> bool {
        !matches!(self, CacheMode::Off)
    }

    /// Whether *every* write reaches that queue.
    ///
    /// Under `minimal` only write-only opens of uncached files stream past it, so `false`
    /// means "may also have writes we cannot see", not "has no queue".
    pub fn all_writes_queued(self) -> bool {
        matches!(self, CacheMode::Writes | CacheMode::Full)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Stable identifier, used in the tray, on D-Bus and in the systemd unit name.
    pub name: String,
    /// An rclone remote name, without the trailing colon.
    pub remote: String,
    /// Optional path within the remote. Empty means its root.
    #[serde(default)]
    pub path: String,
    pub mount_point: PathBuf,

    #[serde(default)]
    pub cache_mode: CacheMode,
    /// `--vfs-cache-max-size`, in rclone's own syntax (`10G`). Unset means unlimited.
    #[serde(default)]
    pub cache_max_size: Option<String>,
    /// `--vfs-cache-max-age` (`1h`, `24h`).
    #[serde(default)]
    pub cache_max_age: Option<String>,

    /// Mount when the service starts.
    #[serde(default = "yes")]
    pub auto_mount: bool,
    #[serde(default)]
    pub read_only: bool,
    /// `--allow-other`. Needs `user_allow_other` in `/etc/fuse.conf`.
    #[serde(default)]
    pub allow_other: bool,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    /// Octal, as a string, so `0022` survives a round trip.
    #[serde(default)]
    pub umask: Option<String>,

    /// Escape hatch, passed to rclone verbatim after everything above.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Mount {
    /// The `remote:path` string rclone expects.
    pub fn fs_spec(&self) -> String {
        if self.path.is_empty() {
            format!("{}:", self.remote)
        } else {
            format!("{}:{}", self.remote, self.path.trim_start_matches('/'))
        }
    }

    /// The systemd unit name for this mount.
    ///
    /// [`Config::validate`] already restricts names to the characters a unit name
    /// accepts, so for any config that passed validation this substitution changes
    /// nothing. It is here for `Mount` values built directly, which skip that path.
    pub fn unit_name(&self) -> String {
        let slug: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{UNIT_PREFIX}{slug}.service")
    }

    /// The rclone argv for this mount, excluding the binary itself.
    ///
    /// Order matters only for `extra_args`, which comes last so a user can override any
    /// flag composed here — rclone takes the last occurrence of a repeated flag.
    pub fn mount_args(&self, rc_socket: &Path) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "mount".into(),
            self.fs_spec(),
            self.mount_point.to_string_lossy().into_owned(),
        ];

        a.push("--vfs-cache-mode".into());
        a.push(self.cache_mode.as_str().into());
        if let Some(size) = &self.cache_max_size {
            a.push("--vfs-cache-max-size".into());
            a.push(size.clone());
        }
        if let Some(age) = &self.cache_max_age {
            a.push("--vfs-cache-max-age".into());
            a.push(age.clone());
        }

        if self.read_only {
            a.push("--read-only".into());
        }
        if self.allow_other {
            a.push("--allow-other".into());
        }
        if let Some(uid) = self.uid {
            a.push("--uid".into());
            a.push(uid.to_string());
        }
        if let Some(gid) = self.gid {
            a.push("--gid".into());
            a.push(gid.to_string());
        }
        if let Some(umask) = &self.umask {
            a.push("--umask".into());
            a.push(umask.clone());
        }

        // The rc socket is what every tier above T4 talks to, so it is not optional and
        // not configurable — a mount without it can only ever be scanned on disk.
        //
        // `--rc-no-auth` is safe *only* because the socket is unreachable by anyone but
        // its owner: rc access is equivalent to shell access as this user. rclone creates
        // the socket 0775 regardless of what we ask for, so the unit sets `UMask=0077` to
        // bring it down to 0700. Changing that umask reopens the hole.
        a.push("--rc".into());
        a.push("--rc-addr".into());
        a.push(format!("unix://{}", rc_socket.display()));
        a.push("--rc-no-auth".into());

        a.extend(self.extra_args.iter().cloned());
        a
    }
}

/// Prefix for every unit this service starts. Also how [`crate::supervisor`] tells its
/// own units apart from any other rclone mount on the system.
pub const UNIT_PREFIX: &str = "rvt-mount-";

/// Why a config file cannot be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("config file {path} could not be read: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config file {path} could not be written: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config file {path} is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config file {path} is version {found}, but this build understands up to {supported} — upgrade rclone-vfsmount-tray")]
    FromTheFuture {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("{0}")]
    Invalid(String),

    #[error("could not determine a config directory: neither $XDG_CONFIG_HOME nor $HOME is set")]
    NoConfigDir,
}

impl Config {
    /// `$XDG_CONFIG_HOME/rclone-vfsmount-tray/config.toml`, else `~/.config/...`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        // Both are treated as unset when empty. `PathBuf::from("").join(".config")` is
        // the *relative* path `.config`, which for a background service resolves against
        // whatever directory it happens to be in — the same trap as an empty `PATH`
        // element meaning the CWD.
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .ok_or(ConfigError::NoConfigDir)?,
            )
            .join(".config"),
        };
        Ok(base.join("rclone-vfsmount-tray").join("config.toml"))
    }

    /// Load and validate. A missing file is the default config, not an error — a fresh
    /// install has no mounts yet and should still start.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        let probe: VersionProbe = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if probe.version > CURRENT_VERSION {
            return Err(ConfigError::FromTheFuture {
                path: path.to_path_buf(),
                found: probe.version,
                supported: CURRENT_VERSION,
            });
        }
        let cfg: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Write atomically: temp file in the same directory, then rename.
    ///
    /// A partial write here is a config file the service cannot parse on next start,
    /// which on a headless box means no mounts and no obvious reason why.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
            path: dir.to_path_buf(),
            source,
        })?;

        let body = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Invalid(format!("could not serialise config: {e}")))?;

        // Unique per call, not per process: two concurrent saves sharing one temp path
        // can interleave truncate and write, and rename a spliced file into place.
        let tmp = path.with_extension(format!(
            "toml.tmp.{}.{}",
            std::process::id(),
            SAVE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        let write_tmp = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            // rename() is atomic against a reader, not against a crash: without this the
            // rename can land while the contents are still only in page cache.
            f.sync_all()
        };
        write_tmp().map_err(|source| {
            let _ = std::fs::remove_file(&tmp);
            ConfigError::Write {
                path: tmp.clone(),
                source,
            }
        })?;

        std::fs::rename(&tmp, path).map_err(|source| {
            let _ = std::fs::remove_file(&tmp);
            ConfigError::Write {
                path: path.to_path_buf(),
                source,
            }
        })?;

        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    pub fn mount(&self, name: &str) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.name == name)
    }

    /// Reject anything that would fail confusingly later.
    ///
    /// Checks that do not touch the filesystem, so this stays usable on a config being
    /// edited for a machine other than this one. Whether a mount point exists is the
    /// supervisor's problem (#17), at the point where it matters.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // A relative override reintroduces the CWD execution that discovery's
        // absolute-only PATH filter exists to prevent.
        if let Some(p) = &self.global.rclone_path {
            if !p.is_absolute() {
                return Err(ConfigError::Invalid(format!(
                    "global.rclone_path must be absolute, got {}",
                    p.display()
                )));
            }
        }

        let mut names = BTreeSet::new();
        let mut points = BTreeSet::new();

        for m in &self.mounts {
            // Checked as stored, not trimmed: this string reaches the systemd unit
            // name, D-Bus and `Config::mount()`.
            let n = m.name.as_str();
            if n.is_empty() {
                return Err(ConfigError::Invalid("a mount has an empty name".into()));
            }
            if n == "." || n == ".." {
                return Err(ConfigError::Invalid(format!(
                    "mount name {n:?} is reserved: the name is used to build paths"
                )));
            }
            // An identifier, not free text. Restricted now because the cost is
            // asymmetric — loosening later is additive, tightening invalidates configs
            // people already wrote.
            if !n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(ConfigError::Invalid(format!(
                    "mount name {n:?} may only contain ASCII letters, digits, '-', '_' and \
                     '.': it is used in the systemd unit name, on D-Bus and in lookups"
                )));
            }
            if !names.insert(n.to_string()) {
                return Err(ConfigError::Invalid(format!("duplicate mount name {n:?}")));
            }
            // The colon case first, so it keeps its more specific advice.
            if m.remote.contains(':') {
                return Err(ConfigError::Invalid(format!(
                    "mount {n:?}: remote should be the name alone, without a colon — put \
                     any path in `path`"
                )));
            }
            // A deliberately conservative subset of rclone's config-name rule, so the
            // problem is reported against the config that holds it rather than as
            // rclone's opaque "config name contains invalid characters" at mount time.
            //
            // This transcribes rclone's *error text* (`fs/fspath` `errInvalidCharacters`),
            // not `configNameRe` itself. The regex is
            // `[\w\p{L}\p{N}.+@]+(?:[ -]+[\w\p{L}\p{N}.+@-]+)*`, whose two halves
            // differ only in `-`, and that overlap means it also rejects a name ending in
            // a *lone* hyphen — `backup-` is invalid while `backup--`, `my -` and `a-b-`
            // are fine. Matching that exactly needs a backtracking scanner, which is not
            // worth it here: the gap only ever *under*-rejects, so no legal name is
            // blocked, and the cost is that `backup-` gets rclone's error instead of ours.
            // Verified against the compiled regex — the trailing-lone-hyphen family is the
            // only divergence.
            //
            // A comma is the sharp one: rclone reads `backup,key=value:` as a connection
            // string and silently applies the override instead of failing.
            let bad_char =
                |c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '+' | '@' | '-' | ' '));
            if m.remote.is_empty()
                || m.remote.chars().any(bad_char)
                || m.remote.starts_with(['-', ' '])
                || m.remote.ends_with(' ')
            {
                return Err(ConfigError::Invalid(format!(
                    "mount {n:?}: remote {:?} is not a usable rclone remote name — letters, \
                     digits, `_`, `.`, `+`, `@`, `-` and interior spaces only, not starting \
                     with `-` or a space and not ending with a space",
                    m.remote
                )));
            }
            if !m.mount_point.is_absolute() {
                return Err(ConfigError::Invalid(format!(
                    "mount {n:?}: mount_point must be absolute, got {}",
                    m.mount_point.display()
                )));
            }
            if !points.insert(m.mount_point.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "mount {n:?}: two mounts share the mount point {}",
                    m.mount_point.display()
                )));
            }
            if let Some(u) = &m.umask {
                if u32::from_str_radix(u.trim_start_matches("0o"), 8).is_err() {
                    return Err(ConfigError::Invalid(format!(
                        "mount {n:?}: umask {u:?} is not octal"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(name: &str, point: &str) -> Mount {
        Mount {
            name: name.into(),
            remote: "backup".into(),
            path: String::new(),
            mount_point: PathBuf::from(point),
            cache_mode: CacheMode::Writes,
            cache_max_size: None,
            cache_max_age: None,
            auto_mount: true,
            read_only: false,
            allow_other: false,
            uid: None,
            gid: None,
            umask: None,
            extra_args: Vec::new(),
        }
    }

    fn with(mounts: Vec<Mount>) -> Config {
        Config {
            version: CURRENT_VERSION,
            global: Global::default(),
            mounts,
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = with(vec![mount("photos", "/mnt/photos")]);
        c.mounts[0].path = "pictures/raw".into();
        c.mounts[0].cache_max_size = Some("10G".into());
        c.mounts[0].umask = Some("0022".into());
        c.mounts[0].extra_args = vec!["--transfers".into(), "8".into()];
        c.global.unmount_on_service_stop = true;

        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn a_minimal_mount_needs_only_three_fields() {
        // This file is hand-edited until #42, so the required set has to stay small.
        let c: Config = toml::from_str(
            r#"
            [[mount]]
            name = "photos"
            remote = "backup"
            mount_point = "/mnt/photos"
            "#,
        )
        .unwrap();
        let m = &c.mounts[0];
        assert_eq!(c.version, CURRENT_VERSION);
        assert_eq!(m.cache_mode, CacheMode::Writes);
        assert!(m.auto_mount, "a configured mount should come up by default");
        assert!(!c.global.unmount_on_service_stop, "must default off");
        c.validate().unwrap();
    }

    #[test]
    fn a_typo_is_rejected_rather_than_ignored() {
        // deny_unknown_fields: silently dropping `mountpoint` would leave someone
        // wondering why their edit did nothing.
        let e = toml::from_str::<Config>(
            r#"
            [[mount]]
            name = "photos"
            remote = "backup"
            mountpoint = "/mnt/photos"
            "#,
        );
        assert!(e.is_err());
    }

    #[test]
    fn fs_spec_matches_what_rclone_expects() {
        let mut m = mount("photos", "/mnt/photos");
        assert_eq!(m.fs_spec(), "backup:");
        m.path = "pictures".into();
        assert_eq!(m.fs_spec(), "backup:pictures");
        m.path = "/pictures".into();
        assert_eq!(m.fs_spec(), "backup:pictures", "leading slash is dropped");
    }

    #[test]
    fn validation_rejects_the_confusing_cases() {
        let dup_name = with(vec![mount("a", "/mnt/one"), mount("a", "/mnt/two")]);
        assert!(dup_name
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        let dup_point = with(vec![mount("a", "/mnt/one"), mount("b", "/mnt/one")]);
        assert!(dup_point
            .validate()
            .unwrap_err()
            .to_string()
            .contains("share"));

        let relative = with(vec![mount("a", "mnt/one")]);
        assert!(relative
            .validate()
            .unwrap_err()
            .to_string()
            .contains("absolute"));

        let mut spaced = with(vec![mount("my mount", "/mnt/one")]);
        assert!(
            spaced.validate().is_err(),
            "name reaches a systemd unit name"
        );
        spaced.mounts[0].name = "ok".into();
        assert!(spaced.validate().is_ok());

        let mut colon = with(vec![mount("a", "/mnt/one")]);
        colon.mounts[0].remote = "backup:".into();
        assert!(colon
            .validate()
            .unwrap_err()
            .to_string()
            .contains("without a colon"));

        let mut bad_umask = with(vec![mount("a", "/mnt/one")]);
        bad_umask.mounts[0].umask = Some("rwxr".into());
        assert!(bad_umask
            .validate()
            .unwrap_err()
            .to_string()
            .contains("octal"));
    }

    #[test]
    fn cache_mode_distinguishes_having_a_queue_from_catching_every_write() {
        // `minimal` does have a queue: read-write opens, and any file already cached, go
        // through it. Only write-only opens of uncached files stream past.
        assert!(CacheMode::Minimal.has_writeback());
        assert!(!CacheMode::Minimal.all_writes_queued());
        assert!(!CacheMode::Off.has_writeback());
        assert!(CacheMode::Writes.all_writes_queued() && CacheMode::Full.all_writes_queued());
        assert_eq!(CacheMode::Full.as_str(), "full");
    }

    #[test]
    fn the_identifier_rule_matches_what_its_message_claims() {
        // The message names systemd and D-Bus, so it has to be an identifier rule —
        // `..` walks out of a path just as `/` does.
        for bad in ["..", ".", "a/b", "a\"b", "a%b", "a b", "\u{65e5}\u{672c}"] {
            let mut c = with(vec![mount("placeholder", "/mnt/one")]);
            c.mounts[0].name = bad.into();
            assert!(c.validate().is_err(), "{bad:?} should be rejected");
        }
        for good in ["photos", "backup-2", "my_mount", "media.tv"] {
            let mut c = with(vec![mount("placeholder", "/mnt/one")]);
            c.mounts[0].name = good.into();
            assert!(c.validate().is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn a_padded_remote_is_rejected_rather_than_stored_unusable() {
        let mut c = with(vec![mount("a", "/mnt/one")]);
        c.mounts[0].remote = " backup ".into();
        assert_eq!(
            c.mounts[0].fs_spec(),
            " backup :",
            "what would reach rclone"
        );
        assert!(
            c.validate().is_err(),
            "rclone rejects an edge-padded config name"
        );
        // Interior spaces are legal to rclone, so do not over-reject.
        c.mounts[0].remote = "my backup".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn a_relative_rclone_path_override_is_rejected() {
        let mut c = with(vec![mount("a", "/mnt/one")]);
        c.global.rclone_path = Some(PathBuf::from("./rclone"));
        assert!(c.validate().is_err(), "would run from the process CWD");
        c.global.rclone_path = Some(PathBuf::from("/usr/local/bin/rclone"));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn a_padded_name_is_rejected_rather_than_stored_unfindable() {
        // Validating the trimmed value while storing the padded one lets " photos "
        // through, then `mount("photos")` finds nothing and the systemd unit name
        // contains a space.
        let mut c = with(vec![mount("x", "/mnt/one")]);
        c.mounts[0].name = " photos ".into();
        assert!(c.validate().is_err(), "padding must fail validation");
    }

    #[test]
    fn a_missing_file_is_the_default_config() {
        let dir = std::env::temp_dir().join(format!("rvt-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(
            cfg.version, CURRENT_VERSION,
            "a fresh install must not write a version no release ever produced"
        );
    }

    #[test]
    fn save_then_load_survives_the_trip() {
        let dir = std::env::temp_dir().join(format!("rvt-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        let c = with(vec![mount("photos", "/mnt/photos")]);
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);

        // No temp files left behind.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "left temp files: {strays:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_schema_is_refused_not_guessed_at() {
        let dir = std::env::temp_dir().join(format!("rvt-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // With an unknown field too: that is what a real schema bump looks like, and the
        // strict parse would otherwise reject it before the version check ran.
        std::fs::write(
            &path,
            format!(
                "version = {}\nsomething_added_later = true\n",
                CURRENT_VERSION + 1
            ),
        )
        .unwrap();

        let e = Config::load(&path).unwrap_err().to_string();
        assert!(e.contains("upgrade"), "should say what to do: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_refuses_to_write_an_invalid_config() {
        // Better to fail the D-Bus call than to persist something the service cannot
        // load on its next start.
        let dir = std::env::temp_dir().join(format!("rvt-inv-{}", std::process::id()));
        let path = dir.join("config.toml");
        let bad = with(vec![mount("a", "/mnt/one"), mount("a", "/mnt/two")]);
        assert!(bad.save(&path).is_err());
        assert!(!path.exists(), "nothing should have been written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_path_follows_xdg() {
        // Not parallel-safe with other env-touching tests, hence being the only one.
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-probe");
        assert_eq!(
            Config::default_path().unwrap(),
            PathBuf::from("/tmp/xdg-probe/rclone-vfsmount-tray/config.toml")
        );
        // An empty HOME must be treated as unset, not joined into a relative path.
        std::env::remove_var("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", "");
        assert!(
            matches!(Config::default_path(), Err(ConfigError::NoConfigDir)),
            "an empty HOME must not yield a relative .config path"
        );
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        std::env::set_var("HOME", "/home/probe");
        assert_eq!(
            Config::default_path().unwrap(),
            PathBuf::from("/home/probe/.config/rclone-vfsmount-tray/config.toml")
        );
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    fn a_mount(name: &str) -> Mount {
        Mount {
            name: name.into(),
            remote: "backup".into(),
            path: "pictures".into(),
            mount_point: PathBuf::from("/home/user/mnt/backup"),
            cache_mode: CacheMode::Writes,
            cache_max_size: None,
            cache_max_age: None,
            auto_mount: true,
            read_only: false,
            allow_other: false,
            uid: None,
            gid: None,
            umask: None,
            extra_args: Vec::new(),
        }
    }

    #[test]
    fn mount_args_carry_the_fs_spec_the_point_and_the_rc_socket() {
        let a = a_mount("backup").mount_args(Path::new("/run/user/1000/rvt/backup.sock"));
        assert_eq!(a[0], "mount");
        assert_eq!(a[1], "backup:pictures");
        assert_eq!(a[2], "/home/user/mnt/backup");
        // Every tier above T4 needs this socket, so it is never omitted.
        let joined = a.join(" ");
        assert!(
            joined.contains("--rc-addr unix:///run/user/1000/rvt/backup.sock"),
            "{joined}"
        );
        assert!(joined.contains("--vfs-cache-mode writes"), "{joined}");
    }

    #[test]
    fn extra_args_come_last_so_they_win() {
        // rclone takes the last occurrence of a repeated flag, which is what makes
        // extra_args an escape hatch rather than a suggestion.
        let mut m = a_mount("backup");
        m.extra_args = vec!["--vfs-cache-mode".into(), "full".into()];
        let a = m.mount_args(Path::new("/run/s.sock"));
        let first = a.iter().position(|x| x == "--vfs-cache-mode").unwrap();
        let last = a.iter().rposition(|x| x == "--vfs-cache-mode").unwrap();
        assert!(
            last > first,
            "the override must come after the composed flag"
        );
        assert_eq!(a[last + 1], "full");
    }

    #[test]
    fn optional_flags_appear_only_when_set() {
        let plain = a_mount("backup").mount_args(Path::new("/run/s.sock"));
        for absent in ["--read-only", "--allow-other", "--uid", "--umask"] {
            assert!(
                !plain.contains(&absent.to_string()),
                "{absent} should be absent"
            );
        }

        let mut m = a_mount("backup");
        m.read_only = true;
        m.allow_other = true;
        m.uid = Some(1000);
        m.gid = Some(1000);
        m.umask = Some("0022".into());
        m.cache_max_size = Some("10G".into());
        m.cache_max_age = Some("24h".into());
        let full = m.mount_args(Path::new("/run/s.sock")).join(" ");
        for present in [
            "--read-only",
            "--allow-other",
            "--uid 1000",
            "--gid 1000",
            "--umask 0022",
            "--vfs-cache-max-size 10G",
            "--vfs-cache-max-age 24h",
        ] {
            assert!(full.contains(present), "{present} missing from: {full}");
        }
    }

    #[test]
    fn a_root_mount_keeps_the_bare_colon() {
        let mut m = a_mount("backup");
        m.path = String::new();
        assert_eq!(m.mount_args(Path::new("/run/s.sock"))[1], "backup:");
    }

    #[test]
    fn unit_names_are_prefixed_and_survive_valid_characters() {
        assert_eq!(a_mount("backup").unit_name(), "rvt-mount-backup.service");
        // Validation permits these, so they must pass through unchanged — otherwise two
        // distinct configured mounts could land on one unit.
        assert_eq!(
            a_mount("my.mount_1-a").unit_name(),
            "rvt-mount-my.mount_1-a.service"
        );
    }
}
