//! The scanner against a cache a real rclone wrote.
//!
//! Every unit test in `scan.rs` builds its own descriptors, so all of them agree with
//! whatever this author believed rclone writes. This one does not: it starts rclone, puts
//! a file through the VFS, and reads whatever lands on disk.
//!
//! `serve webdav` rather than `mount`: it builds the same VFS and the same write-back
//! cache without needing FUSE, so this runs on a bare CI runner. Skipped when rclone is
//! not on PATH.

use rvt_core::scan;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Removes its directory however the test leaves the stack.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the child however the test leaves the stack — an assertion between the spawn and
/// the teardown would otherwise leave rclone holding a listener for as long as the machine
/// stays up.
struct Rclone(Child);

impl Drop for Rclone {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn which_rclone() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("rclone"))
        .find(|p| p.is_file())
}

/// Where rclone puts this remote's cache: `<cache-dir>/<tree>/:local/<remote path>`.
///
/// Composed rather than discovered. An earlier version walked down while a directory had
/// a single child, which descends straight *past* the VFS root into the first
/// subdirectory — so every name came out relative to the wrong place. Composing states
/// the layout being relied on, and the caller asserts the result exists, so a change in
/// rclone fails loudly instead of silently scanning an empty directory.
fn cache_root(cache: &Path, tree: &str, remote: &Path) -> PathBuf {
    let rel = remote.strip_prefix("/").expect("an absolute remote path");
    cache.join(tree).join(":local").join(rel)
}

#[test]
fn a_real_rclone_cache_reads_back_as_one_dirty_file() {
    let Some(rclone) = which_rclone() else {
        eprintln!("skipped: no rclone on PATH");
        return;
    };

    let dir = TempDir(std::env::temp_dir().join(format!("rvt-scanlive-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    let (src, cache) = (dir.0.join("src"), dir.0.join("cache"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&cache).unwrap();

    // A long write-back keeps the file dirty for the whole test rather than racing us.
    let mut child = Rclone(
        Command::new(&rclone)
            .args([
                "serve",
                "webdav",
                &src.to_string_lossy(),
                "--addr",
                "127.0.0.1:0",
                "--vfs-cache-mode",
                "writes",
                "--vfs-write-back",
                "300s",
                "--cache-dir",
                &cache.to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rclone should start"),
    );

    // The kernel picks the port and rclone logs it. Writing into `src` directly would
    // bypass the VFS and leave the cache empty — which is how this test would quietly
    // stop testing anything.
    let stderr = child.0.stderr.take().expect("piped");
    let port = read_served_port(stderr).expect("rclone should log its address");

    let body = vec![b'z'; 512 * 1024];
    // WebDAV will not create a collection implicitly, and the nesting is the point: it is
    // what proves the tree really mirrors the VFS path rather than flattening it.
    mkcol(port, "sub");
    put(port, "sub/queued.bin", &body);

    let meta = cache_root(&cache, "vfsMeta", &src);
    let data = cache_root(&cache, "vfs", &src);
    assert!(
        meta.is_dir() && data.is_dir(),
        "rclone did not lay the cache out where expected — scanning the wrong directory \
         would make every assertion below vacuous: {meta:?} {data:?}"
    );
    let found = scan::scan(&meta, &data).expect("a cache rclone just wrote is readable");

    assert!(
        found.is_complete(),
        "nothing here should be unreadable: {found:?}"
    );
    let names: Vec<_> = found.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["sub/queued.bin"],
        "the scanner should see exactly the file written through the VFS, nested as written"
    );
    assert_eq!(
        found.files[0].bytes,
        Some(body.len() as u64),
        "the size comes from the data file, which is right even when the descriptor is not"
    );
    assert_eq!(
        found.known_bytes(),
        body.len() as u64,
        "and that is what a caller would show as bytes still to send"
    );
    assert!(
        !found.files[0].still_open,
        "the PUT completed, so rclone has closed the handle and filled in Size and Rs"
    );
}

/// Create a collection, so a nested PUT has somewhere to land.
fn mkcol(port: u16, path: &str) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("webdav listener");
    let req = format!(
        "MKCOL /{path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(
        resp.starts_with("HTTP/1.1 2"),
        "MKCOL {path} failed: {}",
        resp.lines().next().unwrap_or("<no response>")
    );
}

/// PUT a body through the VFS over WebDAV.
fn put(port: u16, path: &str, body: &[u8]) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("webdav listener");
    let head = format!(
        "PUT /{path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(
        resp.starts_with("HTTP/1.1 2"),
        "the PUT has to succeed or the cache stays empty: {}",
        resp.lines().next().unwrap_or("<no response>")
    );
}

/// Read rclone's log until it announces the port it bound.
fn read_served_port(stderr: std::process::ChildStderr) -> Option<u16> {
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if let Some(rest) = line.split("127.0.0.1:").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse() {
                return Some(p);
            }
        }
    }
    None
}
