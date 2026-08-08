//! Talking to rclone's rc API over a UNIX socket.
//!
//! # Why never TCP
//!
//! rc access is equivalent to shell access as the rclone user — `core/command` re-execs
//! the binary, `config/dump` returns every credential, and auth is all-or-nothing. So this
//! client speaks only to a UNIX socket, and refuses one that is not private.

use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a whole rc call may take, retries included.
///
/// A budget for the *call*, not for each attempt, so retrying cannot multiply how long a
/// caller is blocked.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts per call, including the first.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

/// Cap on a response body.
///
/// rc responses are untrusted input; without a cap a runaway body allocates until this
/// process is OOM-killed. The largest real response is far below this.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Why an rc call failed.
///
/// The variants a caller must distinguish are separate: "rclone is not there" drives a
/// fall to the on-disk tier, while "rclone said no" is a real error to surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RcError {
    /// No socket at that path. Normal — the mount may simply not be up.
    #[error("no rc socket at {path}")]
    NotListening { path: PathBuf },

    /// The socket exists but is not safe to use.
    ///
    /// Connecting needs only write permission, so a group- or world-writable socket hands
    /// rc access to anyone who qualifies.
    #[error("rc socket {path} is not private: {reason}. Refusing to connect.")]
    InsecureSocket { path: PathBuf, reason: String },

    #[error("could not connect to rc socket {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("rc call {command} timed out after {after:.1?}")]
    Timeout { command: String, after: Duration },

    /// rclone answered, but not with success.
    #[error("rc call {command} failed ({status}): {body}")]
    Failed {
        command: String,
        status: u16,
        body: String,
    },

    /// rclone's answer did not match what we expected.
    #[error("rc call {command} returned unreadable JSON: {source}")]
    Decode {
        command: String,
        #[source]
        source: serde_json::Error,
    },

    /// The request could not be built. A programming error, not a transport fault, so it
    /// is never retried.
    #[error("rc command {command} is not a valid request path: {source}")]
    InvalidCommand {
        command: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The response exceeded the size cap. rclone answered, so this is a fault to
    /// surface rather than a reason to silently degrade — and retrying would only
    /// download it again.
    #[error("rc call {command} returned more than {limit} bytes")]
    ResponseTooLarge { command: String, limit: usize },

    #[error("rc call {command} failed: {source}")]
    Transport {
        command: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl RcError {
    /// Whether this means "no usable rc here", so the caller should drop to the on-disk
    /// tier rather than report a failure.
    ///
    /// An insecure socket counts — we will not use it, so there is no rc — as do a
    /// timeout and a transport fault: a wedged rclone accepts and never answers, and a
    /// restarting one accepts and closes. `Failed` and `Decode` are excluded, because
    /// rclone answered and degrading silently would hide a real fault.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            RcError::NotListening { .. }
                | RcError::InsecureSocket { .. }
                | RcError::Connect { .. }
                | RcError::Timeout { .. }
                | RcError::Transport { .. }
        )
    }
}

/// A client for one rclone process's rc socket.
#[derive(Debug, Clone)]
pub struct RcClient {
    socket: PathBuf,
    timeout: Duration,
    max_attempts: u32,
    max_body: usize,
    /// Who the socket file and its server must belong to. Always this user in production.
    /// Separate knobs in tests because the two checks fire at different points and a test
    /// cannot arrange for a real peer running as somebody else.
    expected_owner_uid: Option<u32>,
    expected_peer_uid: Option<u32>,
}

impl RcClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: CALL_TIMEOUT,
            max_attempts: MAX_ATTEMPTS,
            max_body: MAX_BODY,
            expected_owner_uid: None,
            expected_peer_uid: None,
        }
    }

    /// Shrink the response cap, so the limit path can be exercised without a 64MB body.
    #[cfg(test)]
    fn with_max_body(mut self, max_body: usize) -> Self {
        self.max_body = max_body;
        self
    }

    /// The uid the socket *file* must be owned by.
    #[cfg(test)]
    fn with_expected_owner_uid(mut self, uid: u32) -> Self {
        self.expected_owner_uid = Some(uid);
        self
    }

    /// The uid the process answering on the socket must be running as.
    #[cfg(test)]
    fn with_expected_peer_uid(mut self, uid: u32) -> Self {
        self.expected_peer_uid = Some(uid);
        self
    }

    fn required_owner_uid(&self) -> u32 {
        self.expected_owner_uid.unwrap_or_else(current_uid)
    }

    fn required_peer_uid(&self) -> u32 {
        self.expected_peer_uid.unwrap_or_else(current_uid)
    }

    /// Override the whole-call budget and attempt count.
    pub fn with_limits(mut self, timeout: Duration, max_attempts: u32) -> Self {
        self.timeout = timeout;
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Where this user's rc sockets live: `$XDG_RUNTIME_DIR/rclone-vfsmount-tray`.
    ///
    /// Per-user and mode 0700, which is the control that does not depend on rclone
    /// cooperating.
    pub fn socket_dir() -> Option<PathBuf> {
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join("rclone-vfsmount-tray"))
    }

    /// The conventional socket path for a named mount.
    ///
    /// A configured name cannot contain `/`, but a foreign mount is named by its absolute
    /// mount point, and `Path::join` given an absolute argument discards the base — so
    /// without the substitution `/srv/media` resolves to `/srv/media.sock`, anywhere on
    /// the filesystem. The service builds the same name from its own runtime directory and
    /// has to agree with this.
    pub fn socket_path_for(name: &str) -> Option<PathBuf> {
        Some(Self::socket_dir()?.join(format!("{}.sock", name.replace('/', "_"))))
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Check the socket is present, is a socket, is ours, and is private.
    ///
    /// Before every connect, not once at startup: the socket is recreated each time
    /// rclone restarts.
    pub fn verify(&self) -> Result<(), RcError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};

            // `symlink_metadata`: following the link would approve whatever it points at
            // right now, and it can be repointed before the connect.
            let md = std::fs::symlink_metadata(&self.socket).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    RcError::NotListening {
                        path: self.socket.clone(),
                    }
                } else {
                    RcError::Connect {
                        path: self.socket.clone(),
                        source: e,
                    }
                }
            })?;

            if !md.file_type().is_socket() {
                return Err(RcError::InsecureSocket {
                    path: self.socket.clone(),
                    reason: if md.file_type().is_symlink() {
                        "a symlink, not a socket".into()
                    } else {
                        "not a socket".into()
                    },
                });
            }

            // What matters is whether anyone else can *reach* the socket, which the
            // directory decides: a directory with no group or other bits cannot be
            // traversed, so the mode of the socket inside it is unreachable either way.
            // See DESIGN.md — the directory is the control, the socket's own mode is
            // defence in depth.
            //
            // This is not academic. rclone does no chmod when binding, so under the
            // ordinary umask its socket is 0755, and the mount unit deliberately keeps an
            // ordinary umask because rclone applies the same one to every file inside the
            // mount. Demanding 0600 of the socket itself would refuse every socket the
            // service creates.
            let mut dir_is_private = false;
            if let Some(dir) = self.socket.parent() {
                match std::fs::metadata(dir) {
                    Ok(d) => {
                        if d.mode() & 0o022 != 0 {
                            return Err(RcError::InsecureSocket {
                                path: self.socket.clone(),
                                reason: format!(
                                    "its directory {} is mode {:04o} and writable by \
                                     others, so the socket can be replaced",
                                    dir.display(),
                                    d.mode() & 0o777
                                ),
                            });
                        }
                        dir_is_private = d.mode() & 0o077 == 0 && d.uid() == current_uid();
                    }
                    // Not a connect failure: nothing has been connected to. We simply
                    // cannot establish that the socket is private, so we refuse.
                    Err(e) => {
                        return Err(RcError::InsecureSocket {
                            path: self.socket.clone(),
                            reason: format!(
                                "its directory {} could not be inspected: {e}",
                                dir.display()
                            ),
                        })
                    }
                }
            }

            // rclone does no chmod when binding, so the socket gets `0777 & ~umask` —
            // 0755 under the common umask 022, 0775 under the 002 some distributions
            // ship. A real condition, not a defensive one. Group and other must have
            // nothing at all: write permission alone is enough to connect, and connecting
            // is all an attacker needs.
            // Only when the directory does not already exclude everyone else. Connecting
            // needs write permission, so a loose socket in a traversable directory hands
            // rc access — and therefore shell access as this user — to anyone who
            // qualifies.
            let mode = md.mode() & 0o777;
            if mode & 0o077 != 0 && !dir_is_private {
                return Err(RcError::InsecureSocket {
                    path: self.socket.clone(),
                    reason: format!(
                        "mode {mode:04o} allows access beyond its owner, and its \
                         directory does not exclude other users either"
                    ),
                });
            }

            let uid = self.required_owner_uid();
            if md.uid() != uid {
                return Err(RcError::InsecureSocket {
                    path: self.socket.clone(),
                    reason: format!("owned by uid {}, not {uid}", md.uid()),
                });
            }
        }
        Ok(())
    }

    /// Make one rc call and deserialise its result.
    ///
    /// `params` is the JSON object rclone expects as the request body; pass
    /// `serde_json::json!({})` when the command takes none.
    pub async fn call<T: DeserializeOwned>(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<T, RcError> {
        let body = self.call_raw(command, params).await?;
        serde_json::from_slice(&body).map_err(|source| RcError::Decode {
            command: command.to_string(),
            source,
        })
    }

    /// As [`call`](Self::call), returning the raw response body.
    pub async fn call_raw(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<Vec<u8>, RcError> {
        // One budget for the whole call: per-attempt would block the caller for
        // `timeout * max_attempts` while every error still quoted `timeout`.
        let deadline = tokio::time::Instant::now() + self.timeout;
        let expired = || RcError::Timeout {
            command: command.to_string(),
            after: self.timeout,
        };

        let mut attempt = 0;
        loop {
            attempt += 1;
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(expired());
            }
            match tokio::time::timeout(left, self.attempt(command, &params)).await {
                Ok(Ok(body)) => return Ok(body),
                // The budget is gone by construction, so there is nothing to retry into.
                Err(_) => return Err(expired()),
                Ok(Err(e)) => {
                    // An absent or unsafe socket stays that way, and a rejection is an
                    // answer. Only a transport fault is worth another go.
                    if !matches!(e, RcError::Transport { .. }) || attempt >= self.max_attempts {
                        return Err(e);
                    }
                    let backoff = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                    if backoff >= deadline.saturating_duration_since(tokio::time::Instant::now()) {
                        // Out of budget, but nothing timed out. Reporting a timeout here
                        // would name a duration that never elapsed and discard the
                        // connection reset that identifies the real problem.
                        return Err(e);
                    }
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    async fn attempt(&self, command: &str, params: &serde_json::Value) -> Result<Vec<u8>, RcError> {
        use http_body_util::BodyExt;

        self.verify()?;

        // Built before connecting: a malformed command is a permanent error, so it must
        // not land in the retryable transport class.
        // Infallible for a `Value`; `expect` rather than a fallback because the only
        // fallback available is an empty body, which would send a different request than
        // the caller asked for and say nothing about it.
        let body = http_body_util::Full::new(hyper::body::Bytes::from(
            serde_json::to_vec(params).expect("a serde_json::Value always serialises"),
        ));
        // The Host header is required by HTTP/1.1 and ignored by rclone; the authority is
        // meaningless over a UNIX socket, so any valid value does.
        let req = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(format!("/{command}"))
            .header(hyper::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(body)
            .map_err(|e| RcError::InvalidCommand {
                command: command.to_string(),
                source: Box::new(e),
            })?;

        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound
                    || source.kind() == std::io::ErrorKind::ConnectionRefused
                {
                    RcError::NotListening {
                        path: self.socket.clone(),
                    }
                } else {
                    RcError::Connect {
                        path: self.socket.clone(),
                        source,
                    }
                }
            })?;

        // Who is actually on the other end. `verify()` can only describe a path, and the
        // path can be replaced between the check and the connect; this cannot be raced.
        #[cfg(unix)]
        {
            let peer = stream.peer_cred().map_err(|source| RcError::Connect {
                path: self.socket.clone(),
                source,
            })?;
            let me = self.required_peer_uid();
            if peer.uid() != me {
                return Err(RcError::InsecureSocket {
                    path: self.socket.clone(),
                    reason: format!("served by uid {}, not {me}", peer.uid()),
                });
            }
        }

        let transport = |e: Box<dyn std::error::Error + Send + Sync>| RcError::Transport {
            command: command.to_string(),
            source: e,
        };

        // One connection per call: a pool would add a stale-connection failure mode for
        // no measurable gain.
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|e| transport(Box::new(e)))?;
        // The connection task must be driven for the request to make progress. It ends
        // when the response completes, so it is not leaked.
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| transport(Box::new(e)))?;
        let status = resp.status();
        let body = http_body_util::Limited::new(resp.into_body(), self.max_body)
            .collect()
            .await
            .map_err(|e| {
                if e.downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some()
                {
                    RcError::ResponseTooLarge {
                        command: command.to_string(),
                        limit: self.max_body,
                    }
                } else {
                    transport(e)
                }
            })?
            .to_bytes();

        if !status.is_success() {
            return Err(RcError::Failed {
                command: command.to_string(),
                status: status.as_u16(),
                // rclone puts its reason in the body; the status alone says nothing
                // useful about which command was rejected or why.
                body: String::from_utf8_lossy(&body).trim().to_string(),
            });
        }
        Ok(body.to_vec())
    }
}

/// This process's real user id.
#[cfg(unix)]
fn current_uid() -> u32 {
    // Always succeeds; POSIX defines no error for it.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_conventional_socket_path_never_escapes_the_runtime_directory() {
        // The client's half of the rule the service enforces in `rc_socket_path`. A
        // foreign mount is named by its absolute mount point, and `Path::join` given an
        // absolute path throws the base away — so a client resolving `/srv/media` by name
        // would look for an rc socket at `/srv/media.sock` and, finding any user-owned
        // socket in a private directory there, POST `rc/list` at an unrelated daemon.
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: single-threaded test; restored before returning.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };

        let root = RcClient::socket_dir().expect("set above");
        for name in ["/srv/media", "/", "../../etc/passwd", "a/b"] {
            let p = RcClient::socket_path_for(name).expect("set above");
            assert!(
                p.starts_with(&root),
                "{name:?} produced {p:?}, outside {root:?}"
            );
        }
        assert_eq!(
            RcClient::socket_path_for("backup"),
            Some(root.join("backup.sock")),
            "a configured name must keep the path it has always had"
        );

        match saved {
            // SAFETY: as above.
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    /// A stand-in rc server: accepts one connection, reads the request, and replies with
    /// exactly the bytes given. Raw HTTP rather than a server library, because the point
    /// is to exercise *this* client against wire bytes we control — including the shapes
    /// a library would smooth over.
    #[cfg(unix)]
    struct FakeRc {
        dir: PathBuf,
        socket: PathBuf,
        handle: tokio::task::JoinHandle<Option<String>>,
    }

    #[cfg(unix)]
    impl FakeRc {
        async fn serving(tag: &str, response: &'static str) -> Self {
            let dir = std::env::temp_dir().join(format!("rvt-fakerc-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // 0700, as `socket_dir()` demands in production: the directory is what keeps
            // the socket unreachable, and `verify()` refuses one others can write to.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.join("rc.sock");

            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            // verify() refuses anything looser, and rclone's own default is 0775.
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

            let handle = tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let (mut stream, _) = listener.accept().await.ok()?;
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.ok()?;
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                stream.write_all(response.as_bytes()).await.ok()?;
                stream.flush().await.ok()?;
                Some(request)
            });

            Self {
                dir,
                socket,
                handle,
            }
        }

        fn client(&self) -> RcClient {
            RcClient::new(&self.socket)
        }

        /// The request line and headers the client actually sent.
        async fn request(self) -> String {
            let r = self.handle.await.ok().flatten().unwrap_or_default();
            let _ = std::fs::remove_dir_all(&self.dir);
            r
        }
    }

    #[cfg(unix)]
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Version {
        version: String,
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_successful_call_is_parsed_into_the_expected_type() {
        let server = FakeRc::serving(
            "ok",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 24\r\n\r\n{\"version\":\"v1.75.0\"}\r\n\r\n",
        )
        .await;
        let got: Version = server
            .client()
            .call("core/version", serde_json::json!({}))
            .await
            .expect("a 200 with JSON should deserialise");
        assert_eq!(got.version, "v1.75.0");

        let req = server.request().await;
        // rclone routes on the path, and the rc API is POST-only.
        assert!(req.starts_with("POST /core/version "), "{req}");
        assert!(req.contains("content-type: application/json"), "{req}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_chunked_response_is_read_whole() {
        // rclone's Go server chunks larger bodies. A hand-rolled reader that assumed
        // Content-Length would silently truncate here, which is why this is not
        // hand-rolled.
        let server = FakeRc::serving(
            "chunked",
            // Two chunks, split mid-value: D = 13 bytes, 8 = 8 bytes, then the terminator.
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nD\r\n{\"version\":\"v\r\n8\r\n1.75.0\"}\r\n0\r\n\r\n",
        )
        .await;
        let got: Version = server
            .client()
            .call("core/version", serde_json::json!({}))
            .await
            .expect("a chunked body must be reassembled before parsing");
        assert_eq!(got.version, "v1.75.0");
        let _ = server.request().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_non_2xx_keeps_rclones_explanation() {
        // rclone puts the reason in the body; the status alone does not say which command
        // was rejected or why.
        let server = FakeRc::serving(
            "err",
            "HTTP/1.1 404 Not Found\r\nContent-Length: 32\r\n\r\ncouldn't find method \"vfs/nope\"\r\n",
        )
        .await;
        match server
            .client()
            .call::<serde_json::Value>("vfs/nope", serde_json::json!({}))
            .await
        {
            Err(RcError::Failed {
                command,
                status,
                body,
            }) => {
                assert_eq!(command, "vfs/nope");
                assert_eq!(status, 404);
                assert!(body.contains("couldn't find method"), "{body}");
            }
            other => panic!("expected Failed carrying the body, got {other:?}"),
        }
        let _ = server.request().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_body_that_is_not_the_expected_shape_is_a_decode_error() {
        // Distinct from Failed: rclone answered successfully, we just could not read it.
        // Conflating the two would send a schema drift to the user as "rclone said no".
        let server = FakeRc::serving(
            "shape",
            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"unexpected\":1}",
        )
        .await;
        match server
            .client()
            .call::<Version>("core/version", serde_json::json!({}))
            .await
        {
            Err(RcError::Decode { command, .. }) => assert_eq!(command, "core/version"),
            other => panic!("expected Decode, got {other:?}"),
        }
        let _ = server.request().await;
    }

    /// Accepts repeatedly. The first `fail_first` connections are closed without a
    /// reply — what an rclone that is restarting does — and the rest are answered.
    #[cfg(unix)]
    async fn flaky_server(
        tag: &str,
        fail_first: usize,
        response: &'static str,
    ) -> (
        PathBuf,
        PathBuf,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = std::env::temp_dir().join(format!("rvt-flaky-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

        let connects = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = connects.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < fail_first {
                    // Close mid-conversation: hyper reports an incomplete message, which
                    // is the transport fault worth retrying.
                    drop(stream);
                    continue;
                }
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        (dir, socket, connects)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_transport_fault_is_retried_until_it_succeeds() {
        use std::sync::atomic::Ordering;

        let (dir, socket, connects) = flaky_server(
            "retry",
            2,
            "HTTP/1.1 200 OK\r\nContent-Length: 21\r\n\r\n{\"version\":\"v1.75.0\"}",
        )
        .await;

        let got: Version = RcClient::new(&socket)
            .with_limits(Duration::from_secs(5), 3)
            .call("core/version", serde_json::json!({}))
            .await
            .expect("the third attempt should succeed");
        assert_eq!(got.version, "v1.75.0");
        assert_eq!(
            connects.load(Ordering::SeqCst),
            3,
            "two failures then a success"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retries_stop_at_the_attempt_limit() {
        use std::sync::atomic::Ordering;

        // Always fails, so the limit is what ends it rather than success.
        let (dir, socket, connects) = flaky_server("exhaust", usize::MAX, "").await;

        let e = RcClient::new(&socket)
            .with_limits(Duration::from_secs(5), 3)
            .call::<Version>("core/version", serde_json::json!({}))
            .await
            .expect_err("every attempt fails");
        assert!(matches!(e, RcError::Transport { .. }), "{e:?}");
        assert_eq!(
            connects.load(Ordering::SeqCst),
            3,
            "exactly max_attempts connections, no more"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rclone_answering_with_an_error_is_not_retried() {
        use std::sync::atomic::Ordering;

        // A 404 is an answer, not a fault to try again — retrying would triple the load
        // and the latency for a result that cannot change.
        let (dir, socket, connects) = flaky_server(
            "noretry",
            0,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 5\r\n\r\nnope!",
        )
        .await;

        let e = RcClient::new(&socket)
            .with_limits(Duration::from_secs(5), 3)
            .call::<Version>("vfs/nope", serde_json::json!({}))
            .await
            .expect_err("a 404 is an error");
        assert!(matches!(e, RcError::Failed { status: 404, .. }), "{e:?}");
        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "answered once, not retried"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Accepts, waits, then resets — a crash-looping rclone. Counts connections so a
    /// test can tell how many attempts actually ran.
    #[cfg(unix)]
    async fn slow_failing_server(
        tag: &str,
        delay: Duration,
    ) -> (
        PathBuf,
        PathBuf,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = std::env::temp_dir().join(format!("rvt-slowfail-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

        let connects = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = connects.clone();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    drop(stream);
                });
            }
        });
        (dir, socket, connects, handle)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_peer_that_never_answers_times_out() {
        // Pins the timeout arm itself. Without this nothing in the suite produces a
        // `Timeout` from a real call, so the whole expiry path could be removed unnoticed
        // — and a wedged rclone would freeze every poll tick forever.
        let dir = std::env::temp_dir().join(format!("rvt-wedged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("rc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let held = tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                keep.push(s);
            }
        });

        let budget = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let e = RcClient::new(&socket)
            .with_limits(budget, 3)
            .call::<Version>("core/version", serde_json::json!({}))
            .await
            .expect_err("a peer that never answers must time out");
        let elapsed = started.elapsed();

        match e {
            RcError::Timeout { after, .. } => assert_eq!(after, budget),
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(
            elapsed < budget * 3,
            "took {elapsed:?} for a {budget:?} budget"
        );

        held.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_timeout_bounds_the_whole_call_not_each_attempt() {
        use std::sync::atomic::Ordering;

        // The parameters matter. The peer must fail slowly enough that several attempts
        // do not fit in the budget, and the budget must leave room for a second attempt
        // after the first backoff — otherwise the loop stops at the backoff guard after
        // one attempt and a per-attempt budget is indistinguishable from a whole-call one.
        //
        // 250ms failures, 600ms budget: attempt 1 fails at 250ms, backoff 100ms, attempt 2
        // starts at 350ms with 250ms left and is cut off by the budget at 600ms. A
        // per-attempt budget would instead give each attempt its own 600ms.
        let delay = Duration::from_millis(250);
        let budget = Duration::from_millis(600);
        let (dir, socket, connects, server) = slow_failing_server("budget", delay).await;

        let started = std::time::Instant::now();
        let e = RcClient::new(&socket)
            .with_limits(budget, 5)
            .call::<Version>("core/version", serde_json::json!({}))
            .await
            .expect_err("every attempt fails");
        let elapsed = started.elapsed();
        let attempts = connects.load(Ordering::SeqCst);

        assert!(
            attempts >= 2,
            "only {attempts} attempt(s) ran, so this test cannot distinguish a whole-call \
             budget from a per-attempt one — the parameters are wrong, not the code"
        );
        assert!(
            matches!(e, RcError::Timeout { .. }),
            "the budget, not the attempt limit, must end this: {e:?}"
        );
        assert!(
            elapsed < budget + delay,
            "the call took {elapsed:?} against a {budget:?} budget — the timeout is being \
             applied per attempt rather than to the whole call"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_untrusted_socket_is_never_even_connected_to() {
        use std::sync::atomic::Ordering;

        // "Refused before anything is sent" has to mean the connection is never made, not
        // merely that the reply is distrusted. Counting accepts is what pins the ordering:
        // moving the check after the request would still return the right error.
        let (dir, socket, connects, server) =
            slow_failing_server("unconnected", Duration::from_millis(10)).await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o775)).unwrap();

        let e = RcClient::new(&socket)
            .call::<serde_json::Value>("core/version", serde_json::json!({}))
            .await
            .expect_err("a group-accessible socket must not be used");
        assert!(matches!(e, RcError::InsecureSocket { .. }), "{e:?}");

        // `connect` on a UNIX socket succeeds against the listen backlog, so the server's
        // `accept` — and the counter with it — lags the client by a scheduling hop.
        // Reading it immediately would see zero whether or not a connection was made.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            connects.load(Ordering::SeqCst),
            0,
            "the client connected to a socket it had already judged unsafe"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_peer_running_as_another_user_is_refused_after_connecting() {
        // The check that closes the TOCTOU window: `verify()` can only describe a path,
        // and the path can be swapped before the connect. This asks who actually answered.
        // A test cannot run a server as another uid, so the expected uid is moved instead.
        let server =
            FakeRc::serving("peer", "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").await;

        let wrong = current_uid().wrapping_add(1);
        let e = RcClient::new(&server.socket)
            .with_expected_peer_uid(wrong)
            .call::<serde_json::Value>("core/version", serde_json::json!({}))
            .await
            .expect_err("a peer that is not us must be refused");
        match e {
            RcError::InsecureSocket { reason, .. } => {
                assert!(reason.contains("served by uid"), "{reason}");
            }
            other => panic!("expected InsecureSocket from the peer check, got {other:?}"),
        }

        server.handle.abort();
        let _ = std::fs::remove_dir_all(&server.dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_loose_socket_inside_a_private_directory_is_accepted() {
        // This is what the service actually produces. rclone does no chmod when binding,
        // so its socket is 0755 under an ordinary umask, and the mount unit keeps an
        // ordinary umask deliberately — rclone applies the same one to every file inside
        // the mount. A 0700 directory cannot be traversed, so nobody else can reach the
        // socket whatever its own mode says. Refusing it would reject every socket this
        // project creates, on a default install.
        let dir = std::env::temp_dir().join(format!("rvt-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("rc.sock");
        let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        RcClient::new(&path)
            .verify()
            .expect("a 0755 socket in a 0700 directory is unreachable by anyone else");

        // Open the directory and the same socket must now be refused.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(
                RcClient::new(&path).verify(),
                Err(RcError::InsecureSocket { .. })
            ),
            "with a traversable directory the socket's own mode is what decides"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_owned_by_another_user_is_refused() {
        let dir = std::env::temp_dir().join(format!("rvt-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("rc.sock");
        let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let wrong = current_uid().wrapping_add(1);
        match RcClient::new(&path).with_expected_owner_uid(wrong).verify() {
            Err(RcError::InsecureSocket { reason, .. }) => {
                assert!(reason.contains("owned by uid"), "{reason}");
            }
            other => panic!("a socket owned by somebody else must be refused, got {other:?}"),
        }
        // Same socket, right owner.
        RcClient::new(&path)
            .verify()
            .expect("our own socket is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_standing_in_for_the_socket_is_refused() {
        // Judged on its own account. Following it would approve whatever it points at
        // right now, and the link can be repointed before the connect.
        let dir = std::env::temp_dir().join(format!("rvt-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let real = dir.join("real.sock");
        let _l = std::os::unix::net::UnixListener::bind(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();

        let link = dir.join("link.sock");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        match RcClient::new(&link).verify() {
            Err(RcError::InsecureSocket { reason, .. }) => {
                assert!(reason.contains("symlink"), "{reason}");
            }
            other => panic!("a symlink must be refused, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_oversized_body_is_reported_rather_than_retried() {
        use std::sync::atomic::Ordering;

        // A body cap violation is rclone answering, not a transport fault: retrying would
        // download it again, and degrading silently would hide it.
        let body = "x".repeat(200);
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: 200\r\n\r\n{body}").into_boxed_str(),
        );
        let (dir, socket, connects) = flaky_server("toolarge", 0, resp).await;

        let e = RcClient::new(&socket)
            .with_limits(Duration::from_secs(5), 3)
            .with_max_body(8)
            .call_raw("core/stats", serde_json::json!({}))
            .await
            .expect_err("200 bytes exceeds an 8 byte cap");
        match e {
            RcError::ResponseTooLarge { command, limit } => {
                assert_eq!(command, "core/stats");
                assert_eq!(limit, 8);
            }
            other => panic!("expected ResponseTooLarge, got {other:?}"),
        }
        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "an oversized body must not be fetched again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_socket_in_a_group_writable_directory_is_refused() {
        // The socket's own mode is not enough: anyone who can write to the directory can
        // unlink it and put their own socket at the same path.
        let dir = std::env::temp_dir().join(format!("rvt-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("rc.sock");
        let _l = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        match RcClient::new(&socket).verify() {
            Err(RcError::InsecureSocket { reason, .. }) => {
                assert!(reason.contains("directory"), "{reason}");
            }
            other => panic!("a group-writable directory must be refused, got {other:?}"),
        }

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        RcClient::new(&socket)
            .verify()
            .expect("0700 directory with a 0600 socket is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_insecure_socket_is_refused_before_anything_is_sent() {
        let server =
            FakeRc::serving("insecure", "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").await;
        // A traversable directory, so the socket's own mode is what decides; inside a
        // 0700 directory a loose socket is unreachable and is accepted.
        std::fs::set_permissions(&server.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&server.socket, std::fs::Permissions::from_mode(0o775)).unwrap();

        let e = server
            .client()
            .call::<serde_json::Value>("core/version", serde_json::json!({}))
            .await
            .expect_err("a group-accessible socket must not be used");
        assert!(matches!(e, RcError::InsecureSocket { .. }), "{e:?}");
        assert!(e.is_unreachable());

        server.handle.abort();
        let _ = std::fs::remove_dir_all(&server.dir);
    }

    #[test]
    fn a_missing_socket_reads_as_unreachable_not_as_an_error() {
        // The common case: the mount is simply not up. This must steer the caller to the
        // on-disk tier rather than surface as a fault.
        let c = RcClient::new("/nonexistent/definitely/not/a.sock");
        let e = c.verify().unwrap_err();
        assert!(matches!(e, RcError::NotListening { .. }), "{e:?}");
        assert!(e.is_unreachable());
    }

    #[cfg(unix)]
    #[test]
    fn a_group_accessible_socket_is_refused() {
        let dir = std::env::temp_dir().join(format!("rvt-rc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("perms.sock");
        let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();

        // A traversable directory, so the socket's own mode is what decides. Inside a
        // 0700 directory a loose socket is unreachable and is accepted — that case has
        // its own test below.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // rclone's own default. Connecting needs only write permission, so this hands rc
        // access to the user's group — and rc access is shell access.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o775)).unwrap();
        let c = RcClient::new(&path);
        match c.verify() {
            Err(RcError::InsecureSocket { reason, .. }) => {
                assert!(reason.contains("0775"), "{reason}");
            }
            other => panic!("a 0775 socket must be refused, got {other:?}"),
        }

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        c.verify().expect("0600 is private and must be accepted");

        // Group-read with no write is still refused: the check is deliberately stricter
        // than "can connect", because a mode that loose means something else set it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(c.verify(), Err(RcError::InsecureSocket { .. })));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_regular_file_is_not_a_socket() {
        let dir = std::env::temp_dir().join(format!("rvt-rc-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("not.sock");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        match RcClient::new(&path).verify() {
            Err(RcError::InsecureSocket { reason, .. }) => assert_eq!(reason, "not a socket"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_unreachable_variants_send_the_caller_to_the_on_disk_tier() {
        // Getting this wrong in either direction is bad: a real rclone error silently
        // downgraded looks like a missing mount, and a missing mount reported as an
        // error is noise on every poll.
        assert!(RcError::NotListening {
            path: PathBuf::from("/x")
        }
        .is_unreachable());
        // rclone answered, so these are real faults and must not be silently degraded.
        assert!(!RcError::Failed {
            command: "vfs/queue".into(),
            status: 500,
            body: "boom".into()
        }
        .is_unreachable());

        // A wedged rclone accepts the connection and never answers. That is the same
        // "process unreachable" situation the on-disk tier exists for, so it must not
        // surface as a fault with no data behind it.
        assert!(RcError::Timeout {
            command: "core/stats".into(),
            after: Duration::from_secs(10)
        }
        .is_unreachable());

        // Pinned in both directions, because moving any one of these silently changes
        // whether a whole class of problem reaches the user or vanishes into a fallback.
        assert!(RcError::InsecureSocket {
            path: PathBuf::from("/x"),
            reason: "mode 0755".into()
        }
        .is_unreachable());

        // Schema drift must stay visible: if this degraded silently, the fixture suite
        // would catch a renamed field in CI while every mount quietly dropped to T4 in
        // the field, which is the opposite of what that suite is for.
        let decode = serde_json::from_str::<u32>("nope").unwrap_err();
        assert!(!RcError::Decode {
            command: "core/stats".into(),
            source: decode
        }
        .is_unreachable());

        assert!(!RcError::ResponseTooLarge {
            command: "vfs/list".into(),
            limit: 64
        }
        .is_unreachable());

        // A command string we could not turn into a request is a programming error, not a
        // reason to fall back — degrading would hide it behind a mount that merely looks
        // unable to do much.
        assert!(!RcError::InvalidCommand {
            command: "vfs/ nope".into(),
            source: "invalid uri character".into()
        }
        .is_unreachable());

        // An rclone restarting closes the connection mid-response. Surfacing that as a
        // fault would put a connection-reset message on every poll tick for the length of
        // a `systemctl --user restart`, where the disk scan would have answered.
        assert!(RcError::Transport {
            command: "rc/list".into(),
            source: "connection reset by peer".into()
        }
        .is_unreachable());

        assert!(RcError::Connect {
            path: PathBuf::from("/run/x.sock"),
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused)
        }
        .is_unreachable());
    }

    #[test]
    fn a_failed_call_names_the_command_and_keeps_rclones_reason() {
        let e = RcError::Failed {
            command: "vfs/queue".into(),
            status: 404,
            body: "command not found".into(),
        };
        let msg = e.to_string();
        assert!(
            msg.contains("vfs/queue") && msg.contains("command not found"),
            "{msg}"
        );
    }
}
