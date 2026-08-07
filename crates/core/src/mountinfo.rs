//! Which rclone mounts are live right now, read from `/proc/self/mountinfo`.
//!
//! The only mount check that works for mounts we did not start: no rc socket, no
//! cooperation from rclone, no record of our own.

use std::path::{Path, PathBuf};

/// Filesystem types a rclone FUSE mount can appear under.
///
/// rclone sets the `rclone` subtype, rendered `fuse.rclone`. An explicit `-o subtype`
/// override lands on bare `fuse`, matched on its source instead.
const RCLONE_FSTYPE: &str = "fuse.rclone";
const BARE_FUSE_FSTYPES: &[&str] = &["fuse"];

/// One live mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Where it is mounted.
    pub mount_point: PathBuf,
    /// The `remote:path` rclone was given, as recorded in the mount source field.
    pub source: String,
    /// Filesystem type as the kernel reports it, e.g. `fuse.rclone`.
    pub fstype: String,
}

impl MountEntry {
    /// Whether this looks like an rclone mount.
    ///
    /// Either the `fuse.rclone` subtype, or a bare FUSE mount whose source is
    /// remote-shaped — a colon, and no leading `/` to rule out a local path.
    pub fn is_rclone(&self) -> bool {
        if self.fstype == RCLONE_FSTYPE {
            return true;
        }
        BARE_FUSE_FSTYPES.contains(&self.fstype.as_str())
            && self.source.contains(':')
            && !self.source.starts_with('/')
    }

    /// The remote name, without the path — `backup` from `backup:pictures/raw`.
    pub fn remote(&self) -> Option<&str> {
        self.source.split_once(':').map(|(r, _)| r)
    }
}

/// Parse the contents of a `mountinfo` file.
///
/// Unparsable lines are skipped: one unexpected line must not blind us to every other
/// mount on the system.
pub fn parse(contents: &str) -> Vec<MountEntry> {
    contents.lines().filter_map(parse_line).collect()
}

/// Read and parse `/proc/self/mountinfo`.
pub fn read() -> std::io::Result<Vec<MountEntry>> {
    read_from(Path::new("/proc/self/mountinfo"))
}

/// Read and parse a specific mountinfo file. Separate from [`read`] so tests can supply
/// a fixture without a live `/proc`.
///
/// Decoded lossily on purpose: the kernel escapes only space, tab, newline and backslash,
/// so any other byte is written raw and one of them would make `read_to_string` fail for
/// the whole file — reporting every mount on the machine as absent.
pub fn read_from(path: &Path) -> std::io::Result<Vec<MountEntry>> {
    let raw = std::fs::read(path)?;
    Ok(parse(&String::from_utf8_lossy(&raw)))
}

/// Every live rclone mount.
pub fn rclone_mounts() -> std::io::Result<Vec<MountEntry>> {
    Ok(read()?.into_iter().filter(MountEntry::is_rclone).collect())
}

/// Whether `path` is currently an rclone mount point.
pub fn is_mounted_at(entries: &[MountEntry], path: &Path) -> bool {
    entries
        .iter()
        .any(|e| e.is_rclone() && e.mount_point == path)
}

/// Parse one mountinfo line.
///
/// Fixed up to field 7, then a variable number of optional fields terminated by a lone
/// `-`; the tail is positional again, so the separator must be located first:
///
/// ```text
/// 36 35 98:0 / /mnt rw,noatime shared:1 - fuse.rclone backup: rw,user_id=1000
///                                       ^ separator
/// ```
fn parse_line(line: &str) -> Option<MountEntry> {
    let mut fields = line.split(' ');
    let mount_point = fields.nth(4)?;

    // `position` consumes through the separator, leaving the iterator on the tail.
    fields.position(|f| f == "-")?;

    let fstype = fields.next()?;
    let source = fields.next()?;
    if mount_point.is_empty() || fstype.is_empty() {
        return None;
    }
    Some(MountEntry {
        mount_point: PathBuf::from(unescape(mount_point)),
        source: unescape(source),
        fstype: unescape(fstype),
    })
}

/// Decode the octal escapes the kernel writes for space, tab, newline and backslash.
/// Without it a mount point containing a space never compares equal to the configured
/// one, and the mount reads as down.
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // Take exactly three digits; anything else is not an escape we produced, so the
        // backslash is literal and is kept as-is.
        let rest: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&rest, 8) {
            Ok(byte) if rest.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a machine with an rclone mount up, trimmed to the interesting lines.
    const REAL: &str = "\
23 28 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
28 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
150 28 0:52 / /home/user/mnt/backup rw,nosuid,nodev,relatime shared:78 - fuse.rclone backup:pictures/raw rw,user_id=1000,group_id=1000
151 28 0:53 / /home/user/mnt/gdrive rw,nosuid,nodev,relatime shared:79 - fuse.rclone gdrive: rw,user_id=1000,group_id=1000";

    #[test]
    fn finds_rclone_mounts_among_the_rest() {
        let all = parse(REAL);
        assert_eq!(all.len(), 4, "every line should parse");
        let rclone: Vec<_> = all.into_iter().filter(MountEntry::is_rclone).collect();
        assert_eq!(rclone.len(), 2);
        assert_eq!(rclone[0].mount_point, Path::new("/home/user/mnt/backup"));
        assert_eq!(rclone[0].source, "backup:pictures/raw");
        assert_eq!(rclone[0].remote(), Some("backup"));
        assert_eq!(rclone[1].remote(), Some("gdrive"));
    }

    #[test]
    fn ext4_and_proc_are_not_rclone() {
        for e in parse(REAL).iter().filter(|e| !e.is_rclone()) {
            assert!(
                e.fstype == "proc" || e.fstype == "ext4",
                "{:?} should not have matched",
                e
            );
        }
    }

    #[test]
    fn a_bare_fuse_mount_needs_a_remote_shaped_source() {
        // Without the subtype there is nothing but the source to go on.
        let yes = parse("1 2 0:1 / /mnt rw - fuse gdrive:photos rw");
        assert!(yes[0].is_rclone());

        // sshfs is also bare fuse; its source has no colon.
        let no = parse("1 2 0:1 / /mnt rw - fuse sshfs rw");
        assert!(!no[0].is_rclone());

        // A local path with a colon in it is not a remote spec.
        let local = parse("1 2 0:1 / /mnt rw - fuse /srv/odd:name rw");
        assert!(!local[0].is_rclone());
    }

    #[test]
    fn optional_fields_are_skipped_however_many_there_are() {
        // Zero optional fields, and several — both must land on the same tail.
        let none = parse("1 2 0:1 / /mnt rw - fuse.rclone r: rw");
        assert_eq!(none[0].fstype, "fuse.rclone");
        let many =
            parse("1 2 0:1 / /mnt rw shared:1 master:2 propagate_from:3 - fuse.rclone r: rw");
        assert_eq!(many[0].fstype, "fuse.rclone");
        assert_eq!(many[0].source, "r:");
    }

    #[test]
    fn escaped_characters_in_paths_are_decoded() {
        // A mount point with a space compares equal to the configured path only if the
        // \040 is decoded.
        let e = parse("1 2 0:1 / /home/user/my\\040drive rw - fuse.rclone backup: rw");
        assert_eq!(e[0].mount_point, Path::new("/home/user/my drive"));

        let tab = parse("1 2 0:1 / /mnt/a\\011b rw - fuse.rclone backup: rw");
        assert_eq!(tab[0].mount_point, Path::new("/mnt/a\tb"));

        // A literal backslash that is not a three-digit escape survives untouched.
        let lit = parse("1 2 0:1 / /mnt/a\\b rw - fuse.rclone backup: rw");
        assert_eq!(lit[0].mount_point, Path::new("/mnt/a\\b"));
    }

    #[test]
    fn a_non_utf8_byte_elsewhere_does_not_hide_every_mount() {
        // The kernel writes such bytes raw, and any user can create a directory
        // containing one and mount something there. Reading the file strictly would make
        // every mount on the machine invisible.
        let dir = std::env::temp_dir().join(format!("rvt-mi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mountinfo");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(REAL.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(b"200 28 0:99 / /tmp/");
        bytes.push(0xFF);
        bytes.extend_from_slice(b"bad rw shared:9 - fuse.sshfs u@h:/x rw\n");
        std::fs::write(&path, &bytes).unwrap();

        let got = read_from(&path).expect("an unreadable byte must not fail the read");
        assert_eq!(
            got.iter().filter(|e| e.is_rclone()).count(),
            2,
            "both rclone mounts must still be visible: {got:?}"
        );
        assert!(is_mounted_at(&got, Path::new("/home/user/mnt/backup")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_line_does_not_discard_the_others() {
        let mixed = format!("garbage\n{}\nalso garbage", REAL.lines().nth(2).unwrap());
        let got = parse(&mixed);
        assert_eq!(got.len(), 1, "the one good line must still be seen");
        assert_eq!(got[0].source, "backup:pictures/raw");
    }

    #[test]
    fn a_line_with_no_separator_is_skipped() {
        assert!(parse("1 2 0:1 / /mnt rw shared:1 fuse.rclone backup: rw").is_empty());
    }

    #[test]
    fn is_mounted_at_matches_only_rclone() {
        let all = parse(REAL);
        assert!(is_mounted_at(&all, Path::new("/home/user/mnt/backup")));
        assert!(!is_mounted_at(&all, Path::new("/home/user/mnt/absent")));
        // The root filesystem is mounted, but not by rclone.
        assert!(!is_mounted_at(&all, Path::new("/")));
    }
}
