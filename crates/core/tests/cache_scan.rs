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
use rvt_testutil::Scratch;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

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

/// Where rclone puts this remote's cache: `<cache-dir>/<tree>/<backend>/<remote path>`.
///
/// The backend segment is read from disk rather than assumed, because it is not a fixed
/// string: a bare path gives `local`, while a connection string gives `:local{hash}` with
/// a hash of the string. The rest is composed, so the layout being relied on is stated —
/// an earlier version walked down while a directory had one child, which descends past
/// the VFS root into the first subdirectory and made every name relative to the wrong
/// place.
fn cache_root(cache: &Path, tree: &str, remote: &Path) -> Option<PathBuf> {
    let base = cache.join(tree);
    let mut backends: Vec<_> = std::fs::read_dir(&base).ok()?.flatten().collect();
    let backend = match backends.len() {
        1 => backends.pop().unwrap().path(),
        // One serve, one backend. Anything else means this test no longer knows what it
        // is looking at, which is worse than failing.
        _ => return None,
    };
    let rel = remote.strip_prefix("/").expect("an absolute remote path");
    Some(backend.join(rel))
}

/// Wait until a scan satisfies `done`, or give up.
///
/// rclone writes the data file and its descriptor separately, and both after the PUT
/// returns, so scanning immediately afterwards races it rather than exposing a bug in the
/// scanner. The predicate is the condition the caller is about to assert — waiting on the
/// file count and then asserting on bytes leaves exactly the gap this is meant to close.
fn scan_until(
    cache: &Path,
    remote: &Path,
    what: &str,
    done: impl Fn(&scan::CacheScan) -> bool,
) -> scan::CacheScan {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        if let (Some(meta), Some(data)) = (
            cache_root(cache, "vfsMeta", remote),
            cache_root(cache, "vfs", remote),
        ) {
            if let Ok(found) = scan::scan(&meta, &data) {
                if done(&found) {
                    return found;
                }
                last = Some(found);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("cache never settled to {what}; last saw {last:?}");
}

#[test]
fn a_real_rclone_cache_reads_back_as_one_dirty_file() {
    let Some(rclone) = which_rclone() else {
        eprintln!("skipped: no rclone on PATH");
        return;
    };

    let dir = Scratch::new("scanlive");
    let (src, cache) = (dir.join("src"), dir.join("cache"));
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

    let found = scan_until(&cache, &src, "one fully-written dirty file", |f| {
        f.files.len() == 1 && f.known_bytes() == body.len() as u64
    });

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
}

#[test]
fn a_write_still_in_flight_is_dirty_on_disk_before_it_reaches_the_queue() {
    // The claim this whole tier rests on: `Dirty` is set when the file is *written*, while
    // rclone only puts an item in `vfs/queue` when it is *closed*. If that were the other
    // way round the disk would be no better than the rc endpoints, and the fallback would
    // be pointless.
    //
    // Held open by starting a PUT with a Content-Length and stopping half way, which is a
    // write in progress as far as the VFS is concerned.
    let Some(rclone) = which_rclone() else {
        eprintln!("skipped: no rclone on PATH");
        return;
    };

    let dir = Scratch::new("scanmid");
    let (src, cache) = (dir.join("src"), dir.join("cache"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let rc = dir.join("rc.sock");

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
                "--rc",
                "--rc-addr",
                &format!("unix://{}", rc.display()),
                "--rc-no-auth",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rclone should start"),
    );
    let stderr = child.0.stderr.take().expect("piped");
    let port = read_served_port(stderr).expect("rclone should log its address");

    // Announce 4MB, send 1MB, and keep the connection open.
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("webdav listener");
    let total = 4 * 1024 * 1024;
    stream
        .write_all(
            format!(
                "PUT /halfway.bin HTTP/1.1\r\nHost: localhost\r\nContent-Length: {total}\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(&vec![b'q'; 1024 * 1024]).unwrap();
    stream.flush().unwrap();

    // Wait for bytes, not just for the entry: the descriptor can appear before the data
    // file has anything in it, and bytes are what this test is about.
    let found = scan_until(&cache, &src, "a partly-written dirty file", |f| {
        f.files.len() == 1 && f.known_bytes() > 0
    });
    assert_eq!(
        found
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        vec!["halfway.bin"],
        "the disk knows about a write nothing has closed yet"
    );
    assert!(
        found.known_bytes() > 0,
        "and it knows roughly how much has arrived — the descriptor's own Size reads 0 \
         here, which is why the size comes from the data file: {found:?}"
    );
    assert!(
        found.known_bytes() < total as u64,
        "less than the whole file, since the PUT is only part way through: {found:?}"
    );

    // The other half of the claim in this test's name, asserted at the same instant: the
    // queue has not heard of it. Without this the test proves only that the disk knows.
    assert_eq!(
        rc_post(&rc, "vfs/queue").replace([' ', '\n', '\t'], ""),
        r#"{"queue":[]}"#,
        "rclone queues on close, so a write still in flight is invisible over rc"
    );

    drop(stream);
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

/// Read rclone's log until it announces the port it bound, and keep draining it after.
///
/// The draining is not optional. Dropping the reader closes the pipe, and Go raises
/// SIGPIPE on a write to a closed fd 2 — reproduced: rclone died with signal 13 on its
/// first log line after the close, and the test then failed 20s later pointing at the
/// scanner rather than at the dead child. It survives at the default log level only
/// because nothing is logged per request.
fn read_served_port(stderr: std::process::ChildStderr) -> Option<u16> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if sent {
                continue;
            }
            if let Some(rest) = line.split("127.0.0.1:").nth(1) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = digits.parse::<u16>() {
                    let _ = tx.send(p);
                    sent = true;
                }
            }
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(20)).ok()
}

/// One rc call over the UNIX socket, as raw HTTP. Enough to read `vfs/queue`; the typed
/// client lives in the crate under test and is not what this file is exercising.
fn rc_post(socket: &Path, command: &str) -> String {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(mut s) = UnixStream::connect(socket) {
            let req = format!(
                "POST /{command} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            s.write_all(req.as_bytes()).unwrap();
            s.flush().unwrap();
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            if let Some((_, body)) = resp.split_once("\r\n\r\n") {
                return body.to_string();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rclone never answered on {socket:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
