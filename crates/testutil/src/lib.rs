//! Scratch directories for tests, removed however the test leaves the stack.
//!
//! Every path a test needs on disk comes from [`Scratch`]. Nothing else in the workspace
//! calls [`std::env::temp_dir`] — `tests/no_stray_scratch.rs` enforces that — because a
//! directory left behind on a machine where `/tmp` is tmpfs is resident memory that no
//! later run reclaims.
//!
//! Paths are kept short: tests bind UNIX sockets inside a [`Scratch`], and `sockaddr_un`
//! truncates a path over 108 bytes rather than reporting it. They are also per-process
//! and per-user, so two people running the suite on one machine never meet.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Set this to any value to keep scratch directories after a test finishes, and have each
/// one's path printed as it would have been removed.
pub const KEEP_ENV: &str = "RVT_KEEP_SCRATCH";

/// Number of live [`Scratch`] values, and the lock that makes [`root`] safe to create and
/// remove from several test threads at once.
static LIVE: Mutex<usize> = Mutex::new(0);

/// This process's directory, holding one subdirectory per [`Scratch`].
///
/// Directly inside the temporary directory, with no shared `rvt-test` level above it. A
/// fixed shared name would be created by whichever user ran the suite first, at their
/// umask, and every later user on the machine would then fail to write inside it — for
/// the rest of the boot, with an error naming a directory they have no reason to know
/// about. It would also be a stable, world-writable-parent path for anyone to pre-plant.
///
/// The name carries the pid *and* a timestamp because pids are reused: a run killed hard
/// enough to skip every destructor would otherwise hand its leftovers to a later process
/// that drew the same pid, which would then see a dirty scratch and pass or fail wrongly.
fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        std::env::temp_dir().join(format!(
            "rvt-test-{}-{:x}",
            std::process::id(),
            stamp & 0xffff_ffff
        ))
    })
}

/// Whether to keep scratch directories, read once.
///
/// Reading it per drop would mean a `getenv` on a test thread every time, while other
/// tests in the same binary call `set_var` — and those two race.
fn keep() -> bool {
    static KEEP: OnceLock<bool> = OnceLock::new();
    *KEEP.get_or_init(|| std::env::var_os(KEEP_ENV).is_some())
}

/// A unique, empty directory that removes itself on drop — including while a panic
/// unwinds, which is when a failing test would otherwise leave the most behind.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create an empty directory. `tag` appears in the path, so a leftover from a process
    /// killed before its destructors ran still says which test made it.
    pub fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = root().join(format!("{n:04}-{}", sanitised(tag)));

        // Under the lock so a concurrent Drop cannot remove the root between the two
        // directories this creates.
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("create scratch {}: {e}", path.display()));
        *live += 1;
        drop(live);

        Self { path }
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory. Nothing is created; the parent already exists.
    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    /// A subdirectory, created along with any missing parents.
    pub fn dir(&self, name: impl AsRef<Path>) -> PathBuf {
        let p = self.join(name);
        std::fs::create_dir_all(&p).unwrap_or_else(|e| panic!("create {}: {e}", p.display()));
        p
    }

    /// A file inside the directory, created along with any missing parents.
    pub fn write(&self, name: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
        let p = self.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("create {}: {e}", parent.display()));
        }
        std::fs::write(&p, contents).unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
        p
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if keep() {
            eprintln!("{KEEP_ENV} set, keeping {}", self.path.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.path);
        }

        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        *live -= 1;
        if *live == 0 {
            // Non-recursive, so this takes the root only once nothing is left under it,
            // and leaves it alone when KEEP_ENV held the contents back. The root belongs
            // to this process alone, so no other process can be creating inside it.
            let _ = std::fs::remove_dir(root());
        }
    }
}

/// Reduce a tag to characters that are unambiguous in a path.
fn sanitised(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_exists_and_is_empty() {
        let s = Scratch::new("empty");
        assert!(s.path().is_dir());
        assert_eq!(std::fs::read_dir(s.path()).unwrap().count(), 0);
    }

    #[test]
    fn two_scratches_never_share_a_path() {
        let a = Scratch::new("same");
        let b = Scratch::new("same");
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn dropping_removes_the_tree_including_its_contents() {
        let path = {
            let s = Scratch::new("contents");
            s.write("a/b/c.txt", b"data");
            assert!(s.join("a/b/c.txt").is_file());
            s.path().to_path_buf()
        };
        assert!(!path.exists(), "{} outlived its Scratch", path.display());
    }

    /// The case the plain `remove_dir_all` at the end of a test body cannot cover.
    #[test]
    fn a_panicking_test_still_has_its_directory_removed() {
        let path = std::sync::Mutex::new(PathBuf::new());
        let caught = std::panic::catch_unwind(|| {
            let s = Scratch::new("panicking");
            s.write("evidence", b"x");
            *path.lock().unwrap() = s.path().to_path_buf();
            panic!("as a test body would");
        });
        assert!(caught.is_err());

        let path = path.lock().unwrap();
        assert!(
            path.is_absolute(),
            "the closure did not reach the assignment"
        );
        assert!(!path.exists(), "{} survived the unwind", path.display());
    }

    #[test]
    fn a_tag_cannot_escape_the_root() {
        let s = Scratch::new("../../etc");
        assert_eq!(s.path().parent(), Some(root()));
    }

    /// A shared parent directory would be made by whichever user ran the suite first, at
    /// their umask, locking every other user on the machine out of the whole suite.
    #[test]
    fn the_root_sits_directly_in_the_temporary_directory() {
        let tmp = std::env::temp_dir();
        assert_eq!(
            root().parent(),
            Some(tmp.as_path()),
            "a level between {} and the root is shared between users",
            tmp.display()
        );
    }

    #[test]
    fn paths_leave_room_for_a_unix_socket() {
        // The deepest socket the suite actually binds, from the supervisor tests:
        // `<scratch>/run/<mount>.sock`, with a tag longer than any in use.
        let s = Scratch::new("a-fairly-descriptive-tag");
        let sock = s.join("run").join("backup.sock");
        assert!(
            sock.as_os_str().len() < 108,
            "{} is too long to bind",
            sock.display()
        );
    }
}
