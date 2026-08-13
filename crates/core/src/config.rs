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
    /// **Off by default, deliberately.** The service crashes, and it gets restarted, and
    /// neither may take a filesystem with it (#54).
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

    /// Whether every write reaches that queue *eventually*.
    ///
    /// Under `minimal` only write-only opens of uncached files stream past it, so `false`
    /// means "may also have writes we cannot see", not "has no queue".
    ///
    /// `true` does not mean the queue is the whole story at any given instant: rclone
    /// enqueues a file when it is **closed**, so an open write sits dirty in the cache and
    /// absent from the queue for as long as it takes to write. Nothing over rc sees that —
    /// a non-empty cache does not imply it, since a clean entry lingers for
    /// `--vfs-cache-max-age` and under `full` a plain read creates one.
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
    /// A file mode mask, octal, as a string so `0022` survives a round trip. A leading `0`
    /// or `0o` is optional; it is re-spelled before it reaches rclone. See DESIGN.md.
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
            a.push(canonical_umask(umask));
        }

        // The rc socket is what every tier above T4 talks to, so it is not optional and
        // not configurable — a mount without it can only ever be scanned on disk.
        //
        // `--rc-no-auth` is safe *only* because the socket is unreachable by anyone but
        // its owner: rc access is equivalent to shell access as this user. rclone does no
        // chmod when binding, so the socket gets `0777 & ~umask`. What keeps it private is
        // the 0700 directory it lives in, not its own mode — the unit's umask cannot be
        // used for this, because rclone applies the same umask to every file it creates
        // inside the mount. See DESIGN.md.
        a.push("--rc".into());
        a.push("--rc-addr".into());
        a.push(format!("unix://{}", rc_socket.display()));
        a.push("--rc-no-auth".into());

        a.extend(self.extra_args.iter().cloned());
        a
    }

    /// The `umask` spelling this mount runs with: `extra_args` comes last in the argv and
    /// rclone takes the last occurrence, so a `--umask` there beats the field.
    pub fn effective_umask(&self) -> Option<&str> {
        let from_extra = self.extra_args.iter().enumerate().rev().find_map(|(i, a)| {
            a.strip_prefix("--umask=").or_else(|| {
                (a == "--umask")
                    .then(|| self.extra_args.get(i + 1).map(String::as_str))
                    .flatten()
            })
        });
        from_extra.or(self.umask.as_deref())
    }
}

/// Largest `umask` rclone will accept — `i32::MAX`, since 1.68.0 parses the flag as a
/// signed 32-bit octal. Only the low nine bits mean anything, but refusing a value rclone
/// itself takes is not this project's call (#69).
const MAX_UMASK: u128 = 0o17777777777;

/// The bits a [`Mount::umask`] spells. Too many digits saturates rather than failing, so
/// [`MAX_UMASK`] is what rejects an oversized value and `None` always means "not octal".
fn umask_bits(s: &str) -> Option<u128> {
    match u128::from_str_radix(s.trim_start_matches("0o"), 8) {
        Ok(bits) => Some(bits),
        Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => Some(u128::MAX),
        Err(_) => None,
    }
}

/// `--umask` as leading-zero octal, the one spelling every supported rclone reads alike.
/// Anything [`Config::validate`] would reject passes through untouched. See DESIGN.md.
fn canonical_umask(s: &str) -> String {
    match umask_bits(s) {
        Some(bits) if bits <= MAX_UMASK => format!("0{bits:o}"),
        _ => s.to_string(),
    }
}

/// The effective masks an ambiguous `umask` means to rclone before 1.68.0 and to rclone
/// now, when those differ. See DESIGN.md.
pub fn umask_readings(s: &str) -> Option<(String, String)> {
    let now = umask_bits(s)? & 0o777;
    let unsigned = s.strip_prefix('+').unwrap_or(s);
    let before = if unsigned.starts_with('0') {
        now
    } else {
        unsigned.parse::<u128>().ok()? & 0o777
    };
    (before != now).then(|| (format!("0{before:o}"), format!("0{now:o}")))
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
            // `--rc-addr` is a repeatable flag: rclone appends listeners rather than
            // replacing them, so an extra one here does not override the UNIX socket, it
            // adds a second endpoint beside it. Since the composed argv already carries
            // `--rc-no-auth` — safe only because the socket is unreachable to anyone else
            // — an added TCP listener would serve `core/command` and `config/dump`
            // unauthenticated to the network. rc access is shell access as this user.
            if let Some(bad) = m.extra_args.iter().find(|a| {
                a.strip_prefix("--rc")
                    .is_some_and(|r| r.is_empty() || r.starts_with('-') || r.starts_with('='))
            }) {
                return Err(ConfigError::Invalid(format!(
                    "mount {n:?}: extra_args may not set rc flags ({bad:?}). The rc endpoint \
                     is configured for you as a private UNIX socket; adding another listener \
                     would expose an unauthenticated rc API, which is equivalent to shell \
                     access as this user."
                )));
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
                match umask_bits(u) {
                    None => {
                        return Err(ConfigError::Invalid(format!(
                            "mount {n:?}: umask {u:?} is not octal"
                        )))
                    }
                    Some(bits) if bits > MAX_UMASK => {
                        return Err(ConfigError::Invalid(format!(
                            "mount {n:?}: umask {u:?} is larger than 0{MAX_UMASK:o}, which \
                             rclone 1.68.0 and later refuse"
                        )))
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_testutil::Scratch;

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
        let dir = Scratch::new("cfg");
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
        let dir = Scratch::new("save");
        let path = dir.join("config.toml");

        let c = with(vec![mount("photos", "/mnt/photos")]);
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);

        // No temp files left behind.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "left temp files: {strays:?}");
    }

    #[test]
    fn a_newer_schema_is_refused_not_guessed_at() {
        let dir = Scratch::new("ver");
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
    }

    #[test]
    fn saving_refuses_to_write_an_invalid_config() {
        // Better to fail the D-Bus call than to persist something the service cannot
        // load on its next start.
        let dir = Scratch::new("inv");
        let path = dir.join("config.toml");
        let bad = with(vec![mount("a", "/mnt/one"), mount("a", "/mnt/two")]);
        assert!(bad.save(&path).is_err());
        assert!(!path.exists(), "nothing should have been written");
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
            "--umask 022",
            "--vfs-cache-max-size 10G",
            "--vfs-cache-max-age 24h",
        ] {
            assert!(full.contains(present), "{present} missing from: {full}");
        }
    }

    /// The argv value for a `umask`, or `None` if the flag was not composed.
    fn umask_arg(spelling: &str) -> Option<String> {
        let mut m = a_mount("backup");
        m.umask = Some(spelling.into());
        let a = m.mount_args(Path::new("/run/s.sock"));
        let i = a.iter().position(|x| x == "--umask")?;
        a.get(i + 1).cloned()
    }

    #[test]
    fn every_spelling_of_a_umask_reaches_rclone_as_the_same_octal() {
        for spelling in ["22", "022", "0022", "00022", "0o22", "0o0022"] {
            assert_eq!(
                umask_arg(spelling).as_deref(),
                Some("022"),
                "umask {spelling:?} must reach rclone as 022"
            );
        }
    }

    #[test]
    fn a_composed_umask_always_leads_with_a_zero() {
        // The last two are the widths a fixed-width format gets wrong.
        for (spelling, want) in [
            ("0", "00"),
            ("7", "07"),
            ("077", "077"),
            ("0777", "0777"),
            ("1000", "01000"),
            ("07777", "07777"),
        ] {
            assert_eq!(
                umask_arg(spelling).as_deref(),
                Some(want),
                "umask {spelling:?} must reach rclone as {want}"
            );
        }
    }

    #[test]
    fn validate_accepts_every_spelling_a_config_can_already_hold() {
        // The composing tests reach past validate, so only this stops a tightening there
        // from silently invalidating configs that already load.
        for spelling in [
            "0",
            "22",
            "022",
            "0022",
            "0o22",
            "777",
            "0777",
            "1000",
            "07777",
            "+22",
            "017777777777",
        ] {
            let mut c = with(vec![mount("a", "/mnt/one")]);
            c.mounts[0].umask = Some(spelling.into());
            assert!(
                c.validate().is_ok(),
                "validate must accept umask {spelling:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_a_umask_rclone_would_refuse() {
        // These go through `mount_args` untouched, so validate is the only thing between
        // them and an rclone that will not start.
        for (spelling, want) in [
            ("rwxr", "not octal"),
            ("", "not octal"),
            ("08", "not octal"),
            ("20000000000", "larger than"),
            ("7777777777777777777777", "larger than"),
        ] {
            let mut c = with(vec![mount("a", "/mnt/one")]);
            c.mounts[0].umask = Some(spelling.into());
            let msg = match c.validate() {
                Err(e) => e.to_string(),
                Ok(()) => panic!("validate must reject umask {spelling:?}"),
            };
            assert!(
                msg.contains(want),
                "umask {spelling:?} rejected as {msg:?}, which does not say {want:?}"
            );
        }
    }

    #[test]
    fn only_a_umask_whose_mask_moved_is_worth_warning_about() {
        // `2160` differs only above the nine bits that reach a file; `+022` is the only
        // spelling that separates a leading zero from a leading character.
        for quiet in [
            "0022", "022", "0", "7", "0777", "0o22", "0o755", "2160", "+022",
        ] {
            assert_eq!(
                umask_readings(quiet),
                None,
                "umask {quiet:?} is the same mask either way, so it must not warn"
            );
        }
        assert_eq!(
            umask_readings("63"),
            Some(("077".into(), "063".into())),
            "the warning has to say which mask it was and which it now is"
        );
        for loud in ["22", "12", "755", "1000", "+22"] {
            assert!(
                umask_readings(loud).is_some(),
                "umask {loud:?} moved: rclone <1.68.0 read it as decimal"
            );
        }
    }

    #[test]
    fn a_umask_in_extra_args_is_the_one_that_takes_effect() {
        let mut m = a_mount("backup");
        assert_eq!(m.effective_umask(), None);

        m.umask = Some("0022".into());
        assert_eq!(m.effective_umask(), Some("0022"));

        m.extra_args = vec!["--umask".into(), "22".into()];
        assert_eq!(m.effective_umask(), Some("22"), "extra_args wins");

        m.extra_args = vec!["--umask=63".into()];
        assert_eq!(
            m.effective_umask(),
            Some("63"),
            "the =value form counts too"
        );

        m.extra_args = vec!["--umask".into(), "7".into(), "--umask".into(), "12".into()];
        assert_eq!(m.effective_umask(), Some("12"), "the last occurrence wins");

        m.extra_args = vec!["--transfers".into(), "8".into()];
        assert_eq!(m.effective_umask(), Some("0022"), "back to the field");
    }

    #[test]
    fn a_umask_that_is_not_octal_is_left_alone_for_validate_to_catch() {
        assert_eq!(umask_arg("rwxr").as_deref(), Some("rwxr"));
    }

    #[test]
    fn a_root_mount_keeps_the_bare_colon() {
        let mut m = a_mount("backup");
        m.path = String::new();
        assert_eq!(m.mount_args(Path::new("/run/s.sock"))[1], "backup:");
    }

    #[test]
    fn extra_args_may_not_add_an_rc_listener() {
        // The composed argv carries `--rc-no-auth`, which is safe only because the socket
        // is unreachable to anyone else. `--rc-addr` appends listeners rather than
        // replacing them, so one here would put an unauthenticated `core/command` and
        // `config/dump` on the network. This is the boundary, so it is checked in every
        // form the flag can take.
        for bad in [
            vec!["--rc-addr".to_string(), "0.0.0.0:5572".to_string()],
            vec!["--rc-addr=0.0.0.0:5572".to_string()],
            vec!["--rc".to_string()],
            vec!["--rc-no-auth".to_string()],
            vec!["--rc-user=admin".to_string()],
            vec!["--rc-web-gui".to_string()],
        ] {
            let mut c = Config::default();
            let mut m = a_mount("backup");
            m.extra_args = bad.clone();
            c.mounts.push(m);
            let e = c
                .validate()
                .expect_err(&format!("{bad:?} must be rejected"))
                .to_string();
            assert!(e.contains("rc flags"), "{bad:?} gave {e}");
        }

        // Flags that merely start with the same letters are not rc flags.
        for ok in [
            vec!["--rclone-is-not-a-flag".to_string()],
            vec!["--read-only".to_string()],
            vec!["--vfs-cache-mode".to_string(), "full".to_string()],
        ] {
            let mut c = Config::default();
            let mut m = a_mount("backup");
            m.extra_args = ok.clone();
            c.mounts.push(m);
            c.validate()
                .unwrap_or_else(|e| panic!("{ok:?} should be allowed: {e}"));
        }
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
