//! Scratch directories for tests, removed however the test leaves the stack.
//!
//! Every path a test needs on disk comes from [`Scratch`]. Nothing else in the workspace
//! calls [`std::env::temp_dir`] — `tests/no_stray_scratch.rs` enforces that — because a
//! directory left behind on a machine where `/tmp` is tmpfs is resident memory that no
//! later run reclaims.
//!
//! Keep tags short — nothing here enforces it. Tests bind UNIX sockets inside a
//! [`Scratch`], and a path of 108 bytes or more fails to bind: measured on Linux 6.8,
//! `UnixListener::bind` returns `InvalidInput`, "path must be shorter than SUN_LEN", at
//! 108 and succeeds at 107. Under `/tmp` with a seven-digit pid that leaves a tag about
//! 55 characters before `<scratch>/run/<name>.sock` breaches it; the longest tag in use
//! is 13.
//!
//! No level of a path is shared between processes, so two people running the suite on one
//! machine never meet.

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
/// Sits directly in the temporary directory: no level here may be shared between users.
/// The stamp is there because pids are reused, and truncating it to 32 bits — worth 11
/// bytes of the socket budget above — makes a repeat improbable rather than impossible.
/// Two processes collide only on the same pid *and* start times an exact multiple of
/// 4.295s apart. [`create_exclusive`] is what catches that, rather than this. See
/// `CONTRIBUTING.md`.
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

/// Whether to keep scratch directories. Read once: a `getenv` per drop would race the
/// `set_var` that other tests in the same binary call.
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
        std::fs::create_dir_all(root())
            .unwrap_or_else(|e| panic!("create scratch root {}: {e}", root().display()));
        create_exclusive(&path);
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
            remove_tree(&self.path);
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

/// Create a directory, refusing to adopt one that is already there.
///
/// `create_dir_all` would take it, and the only way this path can exist is a root drawn by
/// an earlier process that died without its destructors — see [`root`]. Its leftovers
/// would then decide whether a test passes, so this fails loudly instead.
fn create_exclusive(path: &Path) {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => panic!(
            "scratch {} already exists: an earlier process drew the same root and left it \
             behind. Remove it and re-run.",
            path.display()
        ),
        Err(e) => panic!("create scratch {}: {e}", path.display()),
    }
}

/// Remove a tree, putting back any permissions that stop the walk.
///
/// A test that chmods a directory `0o000` and panics before restoring it leaves a tree
/// its own owner cannot enter, which plain `remove_dir_all` cannot remove. Anything still
/// left after the retry is reported, never panicked on: a panic here would replace the
/// test's own failure with this one.
fn remove_tree(path: &Path) {
    if std::fs::remove_dir_all(path).is_ok() {
        return;
    }
    #[cfg(unix)]
    make_walkable(path);
    if let Err(e) = std::fs::remove_dir_all(path) {
        if path.exists() {
            eprintln!("rvt-testutil: {} left behind: {e}", path.display());
        }
    }
}

/// Give every directory in a tree back its owner bits, so the tree can be walked.
///
/// Symlinks are read with `symlink_metadata` and never followed, so a link pointing out
/// of the scratch cannot have its target's mode changed.
#[cfg(unix)]
fn make_walkable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !md.is_dir() {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for e in entries.flatten() {
        make_walkable(&e.path());
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

    /// The tests below assert that a directory was removed, which is exactly what
    /// [`KEEP_ENV`] suppresses. Without this they fail for anyone using the debugging aid
    /// `CONTRIBUTING.md` points them at, burying the failure they set it to read.
    fn skip_if_keeping(what: &str) -> bool {
        if keep() {
            eprintln!("skipped: {KEEP_ENV} is set, so {what} is not removed");
        }
        keep()
    }

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
        if skip_if_keeping("the tree") {
            return;
        }
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
        if skip_if_keeping("the directory") {
            return;
        }
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

    /// Three tests in this workspace chmod a directory `0o000`, call the thing under test,
    /// and restore it — `scan.rs` twice and `poller.rs` once. The call between the two is
    /// what can panic, and it skips the restore when it does. `remove_dir_all` alone
    /// cannot enter what that leaves.
    #[cfg(unix)]
    #[test]
    fn a_directory_nothing_can_enter_is_still_removed() {
        if skip_if_keeping("the tree") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let path = {
            let s = Scratch::new("denied");
            let hidden = s.dir("tree/hidden");
            s.write("tree/hidden/inside", b"x");
            std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
            s.path().to_path_buf()
        };

        if path.exists() {
            // Leave nothing behind even when the assertion below is the thing that fails.
            let _ = std::fs::set_permissions(
                path.join("tree/hidden"),
                std::fs::Permissions::from_mode(0o700),
            );
            let _ = std::fs::remove_dir_all(&path);
            panic!("a tree containing an unreadable directory outlived its Scratch");
        }
    }

    /// `create_dir_all` returns `Ok` for a directory that is already there, which would
    /// hand a test whatever the previous owner of the path left in it.
    #[test]
    fn an_existing_directory_is_refused_rather_than_adopted() {
        let s = Scratch::new("adopt");
        let taken = s.dir("already-here");
        std::fs::write(taken.join("someone-elses"), b"x").unwrap();

        let err = std::panic::catch_unwind(|| create_exclusive(&taken))
            .expect_err("an existing directory must not be accepted");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(msg.contains("already exists"), "unhelpful panic: {msg}");

        create_exclusive(&s.join("fresh"));
        assert!(s.join("fresh").is_dir(), "a new path must still be created");
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
        // Deeper than any socket the suite binds — those all sit at `<scratch>/*.sock` —
        // and with a tag longer than any in use. The extra level matches where the
        // supervisor tests put their placeholder socket files, so the budget still holds
        // if one of those ever becomes a real listener.
        let s = Scratch::new("a-fairly-descriptive-tag");
        let sock = s.join("run").join("backup.sock");
        assert!(
            sock.as_os_str().len() < 108,
            "{} is too long to bind",
            sock.display()
        );
    }
}
