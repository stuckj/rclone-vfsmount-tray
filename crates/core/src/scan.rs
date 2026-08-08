//! Reading pending uploads off disk, when rclone cannot be asked.
//!
//! The tier that needs no rc access: dirty items from a crashed rclone are still on disk,
//! where the rc endpoints only ever knew a running process's in-memory queue.
//!
//! It is also the only tier that sees a write while it is happening. `Dirty` is set when
//! the file is *written*, whereas rclone puts an item in `vfs/queue` only when it is
//! *closed* — measured both ways: a first write shows `Dirty: true` with `Size: 0`, and
//! rewriting an already-uploaded file flips its clean descriptor back to `Dirty: true`
//! mid-write. See DESIGN.md.
//!
//! The paths come from an rc response (`vfs/stats` `diskCache.path` and `pathMeta`), so
//! they are untrusted input: the walk reads only regular files, and refuses a descriptor
//! that turns into a symlink or a FIFO between being listed and being opened. It does not
//! defend the *directory* steps the same way — a directory swapped for a symlink between
//! the listing and the descent is followed — which bounds what this is: protection against
//! a wedged or runaway read, not isolation from a hostile filesystem.

use crate::models::VfsMetaItem;
use std::io;
use std::path::Path;

/// Cap on a single metadata descriptor.
///
/// These are a few hundred bytes. The cap is there because the tree is named by an rc
/// response, and a scanner that will happily read whatever it is pointed at is a way to
/// OOM this process.
const MAX_DESCRIPTOR: u64 = 64 * 1024;

/// How many entries a single scan will look at before giving up, reported as
/// [`CacheScan::truncated`]. See DESIGN.md for why a bounded walk is preferred to an
/// unbounded one here.
const MAX_ENTRIES: usize = 50_000;

/// One file the cache is holding that has not reached the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DirtyFile {
    /// Path within the VFS, as the user sees it in the mount.
    ///
    /// No decoding needed: the cache tree mirrors the VFS namespace, not the backend's
    /// encoded object names. Measured on `local` with and without a forced encoder — see
    /// issue #10.
    pub name: String,
    /// Bytes held for this file, taken from the **data file**, not from the descriptor.
    ///
    /// The descriptor's own `Size` is stale for as long as a handle is open: 0 for a file
    /// being created, and the *previous* size for one being rewritten. Summing it reports
    /// nothing outstanding during exactly the copy a user is watching (#10).
    /// `None` when the data file is missing or could not be measured.
    pub bytes: Option<u64>,
}

/// What one walk of a mount's cache found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CacheScan {
    /// Dirty files, in no particular order.
    pub files: Vec<DirtyFile>,
    /// Entries that exist but could not be read or parsed. Never folded into "clean":
    /// rclone rewrites descriptors in place, so a torn read is expected rather than a
    /// fault, and a zero over the top of one would be a claim.
    pub unreadable: u64,
    /// Whether the walk hit its entry cap with entries left unvisited.
    pub truncated: bool,
    /// Whether the metadata root was there at all.
    ///
    /// `false` is not "nothing outstanding". rclone creates the tree lazily, so a mount
    /// that has never cached anything genuinely has none — but a caller that took this
    /// path *from a `vfs/stats` that reported a cache* is looking at a tree that has since
    /// gone, and the queue draining is not the only way that happens. Only the caller
    /// knows which of the two it is in.
    pub root_present: bool,
}

impl CacheScan {
    /// Bytes known to be waiting, counting only files whose size could be measured.
    pub fn known_bytes(&self) -> u64 {
        // Saturating: the sizes come from an untrusted tree, and a wrapped total is a
        // confident wrong answer.
        self.files
            .iter()
            .filter_map(|f| f.bytes)
            .fold(0u64, u64::saturating_add)
    }

    /// Whether this scan is the whole story.
    ///
    /// False when something could not be read or the walk was cut short — in which case a
    /// zero here means "we did not finish looking", not "nothing to send".
    ///
    /// Says nothing about [`Self::root_present`]: an absent tree is completely read, and
    /// whether that counts as an answer depends on why the caller expected one.
    pub fn is_complete(&self) -> bool {
        self.unreadable == 0 && !self.truncated
    }
}

/// Walk one mount's cache and report what has not reached the remote.
///
/// `meta_root` is `diskCache.pathMeta` and `data_root` is `diskCache.path`; the two trees
/// mirror each other, and a file's size comes from the second.
///
/// A missing `meta_root` is an empty scan: rclone creates the tree lazily, so a mount that
/// has never cached anything genuinely has nothing outstanding. Anything else that goes
/// wrong at the root is an error, because "could not look" is not "nothing there".
pub fn scan(meta_root: &Path, data_root: &Path) -> io::Result<CacheScan> {
    scan_bounded(meta_root, data_root, MAX_ENTRIES)
}

/// As [`scan`], with the entry cap supplied.
///
/// Exists so the cap's behaviour can be tested at a size that does not matter. Building
/// `MAX_ENTRIES` files to prove the walk stops is a RAM spike wherever the temp directory
/// is tmpfs, and it is a test that gets run in a loop.
fn scan_bounded(meta_root: &Path, data_root: &Path, max_entries: usize) -> io::Result<CacheScan> {
    let mut out = CacheScan {
        root_present: true,
        ..CacheScan::default()
    };
    let mut stack = vec![meta_root.to_path_buf()];
    let mut visited = 0usize;

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // The root is the caller's question: absent means "no cache here", and
            // anything else means the caller was pointed somewhere unusable.
            Err(e) if dir == meta_root => {
                if e.kind() == io::ErrorKind::NotFound {
                    out.root_present = false;
                    return Ok(out);
                }
                return Err(e);
            }
            // Counted, never skipped: a directory that goes missing was renamed, and
            // its dirty items went with it.
            Err(_) => {
                out.unreadable += 1;
                continue;
            }
        };

        for entry in entries {
            let Ok(entry) = entry else {
                out.unreadable += 1;
                continue;
            };
            if visited >= max_entries {
                out.truncated = true;
                return Ok(out);
            }
            visited += 1;

            let path = entry.path();
            // `file_type` here does not follow links, so a symlink is neither a dir nor a
            // file and falls through. That is deliberate: the root came from rclone, and
            // following a link out of it would read whatever it points at — a FIFO blocks
            // the walk forever, a device file never ends.
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => match read_item(&path) {
                    Some(item) if item.dirty => {
                        let Some(name) = relative_name(meta_root, &path) else {
                            out.unreadable += 1;
                            continue;
                        };
                        out.files.push(DirtyFile {
                            bytes: data_size(&data_root.join(&name)),
                            name,
                        });
                    }
                    Some(_) => {}
                    None => out.unreadable += 1,
                },
                // A socket, a FIFO, a symlink, or a type we could not determine.
                _ => out.unreadable += 1,
            }
        }
    }
    Ok(out)
}

/// Parse one descriptor, or `None` if it cannot be read as one.
fn read_item(path: &Path) -> Option<VfsMetaItem> {
    use std::os::unix::fs::OpenOptionsExt;
    // The type check in the walk was a separate stat, so it is only a filter, not a
    // guarantee: anything that can write in the cache can swap a descriptor for a FIFO in
    // between, and a plain `open` on one blocks until a writer appears — forever, holding
    // a blocking-pool thread per poll. `O_NONBLOCK` makes that return instead, and
    // `O_NOFOLLOW` refuses a symlink swapped in the same way.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    // And re-check through the handle, since the flags only stop the open from hanging.
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut buf = String::new();
    {
        use std::io::Read;
        file.take(MAX_DESCRIPTOR).read_to_string(&mut buf).ok()?;
    }
    serde_json::from_str(&buf).ok()
}

/// The size of a cached data file, or `None` when it is absent or not a regular file.
///
/// Absent is normal: the descriptor and the data file are written separately, so a walk
/// can land between them.
fn data_size(path: &Path) -> Option<u64> {
    let md = std::fs::symlink_metadata(path).ok()?;
    md.is_file().then_some(md.len())
}

/// A descriptor's path within the cache, as the VFS presents it.
///
/// `None` for a path that is not under the root, or that does not render as UTF-8 — both
/// of which mean this entry cannot be named to a user, so it counts as unreadable rather
/// than being reported under a mangled name.
fn relative_name(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_str()?;
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A cache tree under a directory that removes itself.
    struct Tree(PathBuf);

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("rvt-scan-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("vfsMeta")).unwrap();
            std::fs::create_dir_all(root.join("vfs")).unwrap();
            Self(root)
        }

        fn meta(&self) -> PathBuf {
            self.0.join("vfsMeta")
        }
        fn data(&self) -> PathBuf {
            self.0.join("vfs")
        }

        /// Write a descriptor, and optionally the data file beside it.
        fn put(&self, name: &str, json: &str, data_bytes: Option<usize>) {
            let m = self.meta().join(name);
            std::fs::create_dir_all(m.parent().unwrap()).unwrap();
            std::fs::write(m, json).unwrap();
            if let Some(n) = data_bytes {
                let d = self.data().join(name);
                std::fs::create_dir_all(d.parent().unwrap()).unwrap();
                std::fs::write(d, vec![b'x'; n]).unwrap();
            }
        }

        fn scan(&self) -> CacheScan {
            scan(&self.meta(), &self.data()).expect("a readable tree is not an error")
        }
    }

    /// A closed, queued item: rclone fills in both `Size` and `Rs` on close.
    fn closed(size: u64) -> String {
        format!(
            r#"{{"ModTime":"2026-08-08T00:00:00Z","ATime":"2026-08-08T00:00:00Z","Size":{size},
                "Rs":[{{"Pos":0,"Size":{size}}}],"Fingerprint":"","Dirty":true}}"#
        )
    }

    /// An item with a handle still open: neither field is written until close.
    const OPEN: &str = r#"{"ModTime":"2026-08-08T00:00:00Z","ATime":"2026-08-08T00:00:00Z",
        "Size":0,"Rs":null,"Fingerprint":"","Dirty":true}"#;

    const CLEAN: &str = r#"{"ModTime":"2026-08-08T00:00:00Z","ATime":"2026-08-08T00:00:00Z",
        "Size":1024,"Rs":[{"Pos":0,"Size":1024}],"Fingerprint":"","Dirty":false}"#;

    #[test]
    fn only_dirty_items_are_reported() {
        // A `full` mount's cache is mostly read-cached files, which are clean. Counting
        // cache entries rather than dirty ones is what makes a read-only media mount look
        // permanently busy.
        let t = Tree::new("dirty");
        t.put("a.bin", &closed(100), Some(100));
        t.put("read-cached.bin", CLEAN, Some(9999));
        let s = t.scan();

        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].name, "a.bin");
        assert!(s.is_complete());
    }

    #[test]
    fn a_size_of_zero_while_open_does_not_hide_the_bytes_on_disk() {
        // The measurement this scanner exists for (#10): `Size` stays 0 for the whole time
        // a file is open, so `sum(Size where Dirty)` reports nothing outstanding during
        // exactly the copy the user is watching. The data file is the honest figure.
        let t = Tree::new("open");
        t.put("growing.bin", OPEN, Some(7_340_032));
        let s = t.scan();

        assert_eq!(s.files.len(), 1);
        assert_eq!(
            s.files[0].bytes,
            Some(7_340_032),
            "the descriptor says 0; the data file says 7MB, and the data file is right"
        );
        assert_eq!(s.known_bytes(), 7_340_032);
    }

    #[test]
    fn a_rewrite_in_progress_is_measured_from_the_data_file_not_the_stale_size() {
        // The shape neither other fixture has: `Size` non-zero, and *wrong*. Rewriting an
        // already-uploaded file leaves the descriptor's previous size and ranges in place
        // while the data file grows — measured on a live mount. "Trust the descriptor when
        // it says something, fall back to the data file when it says 0" passes every other
        // test here and under-reports this by every byte written past the old size.
        let t = Tree::new("rewrite");
        t.put(
            "grown.bin",
            r#"{"ModTime":"2026-08-08T00:00:00Z","ATime":"2026-08-08T00:00:00Z",
                "Size":4194304,"Rs":[{"Pos":0,"Size":4194304}],"Fingerprint":"","Dirty":true}"#,
            Some(8_388_608),
        );
        let s = t.scan();

        assert_eq!(
            s.files[0].bytes,
            Some(8_388_608),
            "the descriptor is stale at 4MB; the data file has 8MB and is the honest one"
        );
        assert_eq!(s.known_bytes(), 8_388_608);
    }

    #[test]
    fn a_descriptor_swapped_for_a_fifo_after_listing_does_not_block_the_walk() {
        // The walk's type check is a separate stat, so it is a filter rather than a
        // guarantee. `read_item` is what has to hold when the swap wins the race, and it
        // is reached directly here because the walk would otherwise filter the FIFO out
        // before it ever got there.
        let t = Tree::new("swap");
        let path = t.meta().join("swapped.bin");
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a path inside this test's own temp directory.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);

        // On a bounded wait rather than directly, because the regression this guards is a
        // *hang*: opening a FIFO with no writer blocks until one appears, holding a
        // blocking-pool thread for the life of the process. A test that hangs reports far
        // less than one that fails.
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_item(&probe).is_none());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(true) => {}
            Ok(false) => panic!("a FIFO is not a descriptor and must not parse as one"),
            Err(_) => panic!("read_item blocked on a FIFO — O_NONBLOCK is what stops that"),
        }

        use std::os::unix::fs::symlink;
        let outside = t.0.join("elsewhere.json");
        std::fs::write(&outside, closed(1)).unwrap();
        let link = t.meta().join("linked.bin");
        symlink(&outside, &link).unwrap();
        assert!(
            read_item(&link).is_none(),
            "and a symlink swapped in the same way must not be followed"
        );
    }

    #[test]
    fn a_closed_item_still_counts_until_it_uploads() {
        // `Dirty` stays true from the write until the upload completes, so a file that has
        // been closed and is merely waiting is every bit as outstanding as one mid-write.
        let t = Tree::new("closed");
        t.put("done.bin", &closed(4096), Some(4096));
        let s = t.scan();

        assert_eq!(s.files.len(), 1);
        assert_eq!(s.known_bytes(), 4096);
    }

    #[test]
    fn names_are_vfs_relative_and_keep_their_nesting() {
        // The tree mirrors the VFS namespace, so the path under the root is what the user
        // sees in the mount — no decoding, and no leaking the cache root into the UI.
        let t = Tree::new("nested");
        t.put("sub/deep/file with spaces.bin", &closed(1), Some(1));
        let s = t.scan();

        assert_eq!(s.files[0].name, "sub/deep/file with spaces.bin");
    }

    #[test]
    fn an_unparseable_descriptor_counts_as_unknown_rather_than_clean() {
        // rclone rewrites descriptors in place, so a torn read is expected — but a file
        // that might be dirty and cannot be read is not evidence of an empty queue.
        let t = Tree::new("torn");
        t.put("torn.bin", "", None);
        t.put("good.bin", &closed(10), Some(10));
        let s = t.scan();

        assert_eq!(s.files.len(), 1, "the good one still counts");
        assert_eq!(s.unreadable, 1);
        assert!(
            !s.is_complete(),
            "a zero here would be 'we did not finish looking', not 'nothing to send'"
        );
    }

    #[test]
    fn a_total_that_would_overflow_saturates_rather_than_wrapping() {
        // The sizes come from a tree named by an rc response. Sparse files can report
        // enormous apparent sizes, and a plain sum panics in debug — inside a poll — and
        // wraps to a small number in release, which is a confident wrong total.
        let s = CacheScan {
            files: vec![
                DirtyFile {
                    name: "a".into(),
                    bytes: Some(u64::MAX),
                },
                DirtyFile {
                    name: "b".into(),
                    bytes: Some(u64::MAX),
                },
            ],
            unreadable: 0,
            truncated: false,
            root_present: true,
        };
        assert_eq!(s.known_bytes(), u64::MAX);
    }

    #[test]
    fn a_data_file_that_is_a_symlink_is_not_measured_through_it() {
        // The data root comes from the same untrusted response as the metadata root, so a
        // size must not be read through a link out of the tree — that reports somebody
        // else's file as this mount's backlog.
        use std::os::unix::fs::symlink;
        let t = Tree::new("datalink");
        t.put("linked.bin", &closed(10), None);
        let outside = t.0.join("elsewhere.bin");
        std::fs::write(&outside, vec![b'x'; 4096]).unwrap();
        std::fs::create_dir_all(t.data()).unwrap();
        symlink(&outside, t.data().join("linked.bin")).unwrap();

        let s = t.scan();
        assert_eq!(s.files.len(), 1);
        assert_eq!(
            s.files[0].bytes, None,
            "a symlinked data file is unmeasured, not 4096 bytes of someone else's data"
        );
    }

    #[test]
    fn a_missing_data_file_leaves_the_size_unknown_rather_than_zero() {
        // The descriptor and the data file are written separately, so a walk can land
        // between them. Reporting 0 bytes would understate the total silently.
        let t = Tree::new("orphan");
        t.put("no-data.bin", &closed(500), None);
        let s = t.scan();

        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].bytes, None);
        assert_eq!(s.known_bytes(), 0, "unknown contributes nothing to a total");
    }

    #[test]
    fn a_cache_that_has_never_held_anything_is_empty_not_an_error() {
        // rclone creates the tree lazily. A mount that has cached nothing has nothing
        // outstanding, and that is a real answer rather than a failure to look.
        let t = Tree::new("absent");
        let missing = t.0.join("never-existed");
        let s = scan(&missing, &t.data()).expect("an absent tree is not an error");

        assert!(s.files.is_empty());
        assert!(s.is_complete(), "there was nothing to fail to read");
        assert!(
            !s.root_present,
            "the caller has to be able to tell 'never cached anything' from 'the cache \
             it told us about has gone'"
        );
    }

    #[test]
    fn a_subdirectory_that_cannot_be_listed_is_counted_not_skipped() {
        // A directory can hide an arbitrarily large subtree, so passing over one silently
        // turns it into a confident zero. The live case is a rename: eviction removes
        // items, each counted where it was listed, and then purges directories that are
        // already empty — but a *non-empty* directory going missing mid-walk means it
        // moved, and its dirty items moved with it.
        use std::os::unix::fs::PermissionsExt;
        let t = Tree::new("subdir");
        t.put("visible.bin", &closed(64), Some(64));
        t.put("hidden/big.bin", &closed(1_000_000), Some(1_000_000));
        let hidden = t.meta().join("hidden");
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();

        let s = t.scan();
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(s.files.len(), 1, "only the entry it could reach");
        assert_eq!(s.unreadable, 1, "and the directory it could not");
        assert!(
            !s.is_complete(),
            "a subtree went unlooked-at, so this zero is not an answer"
        );
    }

    #[test]
    fn a_walk_that_hits_its_cap_says_so_rather_than_reporting_what_it_reached() {
        // The cap is the only thing standing between a media-sized cache and a partial
        // walk presented as a total. Under `full` every read-cached file is an entry, so
        // this is reachable on a mount nobody has written to in hours.
        //
        // At a cap of 12 rather than the real one: building 50,000 files to prove the walk
        // stops is a RAM spike wherever the temp directory is tmpfs, which is a poor trade
        // for a mechanism that does not care what the number is.
        let t = Tree::new("capped");
        for i in 0..20 {
            t.put(&format!("f{i}.bin"), &closed(1), None);
        }
        let s = scan_bounded(&t.meta(), &t.data(), 12).unwrap();

        assert!(s.truncated, "the walk stopped early and has to say so");
        // Exactly the cap, not merely "at most": every entry in a flat all-dirty tree is
        // one visit and one file, so a cap that is off by one shows up here.
        assert_eq!(s.files.len(), 12);
        // And the public entry point uses the real cap — asserted rather than exercised,
        // for the reason above.
        assert_eq!(MAX_ENTRIES, 50_000);
        assert!(
            !s.is_complete(),
            "what it found is a floor, not a total — reporting it as one is the false \
             idle this tier exists to prevent"
        );
    }

    #[test]
    fn a_root_that_cannot_be_listed_is_an_error_not_an_empty_scan() {
        // "Could not look" and "nothing there" are the two readings this whole module
        // exists to keep apart.
        use std::os::unix::fs::PermissionsExt;
        let t = Tree::new("denied");
        std::fs::set_permissions(t.meta(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let got = scan(&t.meta(), &t.data());
        std::fs::set_permissions(t.meta(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(got.is_err(), "an unreadable root must not read as empty");
    }

    #[test]
    fn a_fifo_is_skipped_rather_than_opened() {
        // The roots come from an rc response. Reading whatever is found there blocks the
        // walk forever on a FIFO — and DESIGN.md already requires treating the path as
        // untrusted. Counted as unknown, not silently ignored.
        let t = Tree::new("fifo");
        let fifo = t.meta().join("pipe.bin");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a path we just built inside our own temp directory.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        t.put("real.bin", &closed(8), Some(8));

        let s = t.scan();
        assert_eq!(s.files.len(), 1, "the real entry is still found");
        assert_eq!(s.unreadable, 1, "the FIFO is unknown, not clean");
    }

    #[test]
    fn a_symlink_out_of_the_tree_is_not_followed() {
        use std::os::unix::fs::symlink;
        let t = Tree::new("link");
        let outside = t.0.join("outside.json");
        std::fs::write(&outside, closed(999)).unwrap();
        symlink(&outside, t.meta().join("link.bin")).unwrap();

        let s = t.scan();
        assert!(
            s.files.is_empty(),
            "following the link would report a file that is not in this cache"
        );
        assert_eq!(s.unreadable, 1);
    }
}
