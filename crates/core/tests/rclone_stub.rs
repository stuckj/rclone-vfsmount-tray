//! Exercise rclone discovery against a stub binary.
//!
//! A shell script standing in for rclone, so the discovery path, the version gate and the
//! output parsers are tested end-to-end without needing rclone installed. The stub emits
//! the exact output a real rclone v1.75.0 produces — captured, not invented.
//!
//! Its own integration test binary because it manipulates `PATH`, which is
//! process-global; keeping it separate stops that racing other tests.

use rvt_core::rclone::{Rclone, RcloneError, MINIMUM_VERSION};
use std::path::{Path, PathBuf};

/// Real `rclone version` output, from the machine used in #9.
const VERSION_OUTPUT: &str = r#"rclone v1.75.0
- os/version: ubuntu 26.04 (64 bit)
- os/kernel: 6.8.12-39-pve (x86_64)
- os/type: linux
- os/arch: amd64
- go/version: go1.26.5
- go/linking: static
- go/tags: none"#;

// Three lines, as rclone prints them (cmd/config/config.go). The parser ignores the
// third; the stub carries it so that is actually exercised rather than assumed.
const CONFIG_PATHS_OUTPUT: &str = "Config file: /home/user/.config/rclone/rclone.conf\n\
                                   Cache dir:   /home/user/.cache/rclone\n\
                                   Temp dir:    /tmp\n";

struct Stub {
    dir: PathBuf,
}

impl Stub {
    fn new(tag: &str, version: &str) -> Self {
        Self::build(tag, version, false)
    }

    /// A stub whose `config paths` fails, for exercising the error path.
    fn failing_paths(tag: &str) -> Self {
        Self::build(tag, VERSION_OUTPUT, true)
    }

    /// Write a stub `rclone` that reports `version` and canned subcommand output.
    fn build(tag: &str, version: &str, fail_paths: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("rvt-stub-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let script = format!(
            r#"#!/bin/sh
case "$1 $2" in
  "version ")        printf '%b\n' "{version}" ;;
  "config paths")    {paths_branch} ;;
  "listremotes ")    printf 'backup:\ngdrive:\n\n' ;;
  *)                 echo "unexpected: $*" >&2; exit 64 ;;
esac
"#,
            version = version.replace('\n', "\\n"),
            paths_branch = if fail_paths {
                r#"echo "config paths: exit status 1: failed to load config" >&2; exit 1"#
                    .to_string()
            } else {
                format!(
                    r#"printf '%b' "{}""#,
                    CONFIG_PATHS_OUTPUT.replace('\n', "\\n")
                )
            },
        );
        let bin = dir.join("rclone");
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Stub { dir }
    }

    fn bin(&self) -> PathBuf {
        self.dir.join("rclone")
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn discovers_and_parses_a_real_version_string() {
    let stub = Stub::new("ok", VERSION_OUTPUT);
    let r = Rclone::discover(Some(&stub.bin())).expect("stub should be accepted");
    assert_eq!(r.version().to_string(), "1.75.0");
    assert_eq!(r.path(), stub.bin());
}

#[test]
fn an_old_rclone_is_rejected_with_both_versions_named() {
    let stub = Stub::new("old", "rclone v1.50.0");
    match Rclone::discover(Some(&stub.bin())) {
        Err(RcloneError::TooOld { found, minimum }) => {
            assert_eq!(found.to_string(), "1.50.0");
            assert_eq!(minimum, MINIMUM_VERSION);
        }
        other => panic!("expected TooOld, got {other:?}"),
    }
}

#[test]
fn unparsable_version_output_says_what_it_saw() {
    let stub = Stub::new("weird", "this is not rclone");
    match Rclone::discover(Some(&stub.bin())) {
        Err(RcloneError::UnparsableVersion { output }) => {
            assert!(output.contains("not rclone"), "{output}");
        }
        other => panic!("expected UnparsableVersion, got {other:?}"),
    }
}

#[test]
fn a_missing_binary_lists_where_it_looked() {
    let missing = Path::new("/nonexistent/definitely/not/rclone");
    match Rclone::discover(Some(missing)) {
        Err(RcloneError::NotExecutable { path, .. }) => assert_eq!(path, missing),
        other => panic!("expected NotExecutable, got {other:?}"),
    }
}

#[test]
fn config_paths_are_parsed_from_the_real_output_shape() {
    let stub = Stub::new("paths", VERSION_OUTPUT);
    let r = Rclone::discover(Some(&stub.bin())).unwrap();
    let p = r.config_paths().unwrap();
    assert_eq!(
        p.config_file,
        Path::new("/home/user/.config/rclone/rclone.conf")
    );
    // The cache dir is what makes the on-disk tier work with no rc endpoint.
    assert_eq!(p.cache_dir, Path::new("/home/user/.cache/rclone"));
}

#[test]
fn listremotes_strips_colons_and_blanks() {
    let stub = Stub::new("remotes", VERSION_OUTPUT);
    let r = Rclone::discover(Some(&stub.bin())).unwrap();
    assert_eq!(r.list_remotes().unwrap(), vec!["backup", "gdrive"]);
}

#[test]
fn a_failing_subcommand_carries_rclones_stderr() {
    // A bare "it failed" is useless; rclone's own message has to reach the user.
    let stub = Stub::failing_paths("fail");
    let r = Rclone::discover(Some(&stub.bin())).unwrap();
    match r.config_paths() {
        Err(RcloneError::CommandFailed { args, stderr, .. }) => {
            assert_eq!(args, "config paths");
            assert!(stderr.contains("failed to load config"), "{stderr}");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
}

#[test]
fn discovery_finds_rclone_on_path() {
    // The only test that touches PATH — hence this separate test binary.
    let stub = Stub::new("path", VERSION_OUTPUT);
    let prev = std::env::var_os("PATH");
    let joined = std::env::join_paths(
        std::iter::once(stub.dir.clone())
            .chain(std::env::split_paths(prev.as_deref().unwrap_or_default())),
    )
    .unwrap();
    std::env::set_var("PATH", &joined);

    let found = Rclone::discover(None);

    match prev {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }

    let r = found.expect("should have found the stub on PATH");
    assert_eq!(r.version().to_string(), "1.75.0");
}

/// The shipped example must be loadable — it is the only documentation of the format
/// until the GTK editor lands, and an example that does not parse is worse than none.
#[test]
fn the_shipped_example_config_is_valid() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.example.toml");
    let text = std::fs::read_to_string(path).expect("config.example.toml");
    let cfg: rvt_core::Config = toml::from_str(&text).expect("example should parse");
    cfg.validate().expect("example should validate");
    assert_eq!(cfg.mounts.len(), 1, "one uncommented [[mount]] block");
    assert_eq!(cfg.mounts[0].fs_spec(), "backup:pictures/raw");
    assert!(cfg.mounts[0].cache_mode.has_writeback());
}
