//! Talking to rclone's rc API over a UNIX socket.
//!
//! # Why never TCP
//!
//! rc access is equivalent to shell access as the rclone user: `core/command` re-executes
//! the rclone binary, and `config/dump` returns every backend credential. Authentication
//! is all-or-nothing, with no per-endpoint scoping. A TCP bind would expose that to
//! anything that can reach the port, so this client speaks only to a UNIX socket and
//! refuses one whose permissions do not make it private.

use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one rc call may take before it is abandoned.
///
/// rc calls are local and answer in milliseconds. A long timeout here does not rescue a
/// wedged rclone; it only holds up whatever asked.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts per call, including the first.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

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
    /// Connecting to a UNIX socket needs write permission, so a group- or world-writable
    /// socket hands rc access — and therefore shell access as this user — to anyone who
    /// qualifies.
    #[error("rc socket {path} is not private: {reason}. Refusing to connect.")]
    InsecureSocket { path: PathBuf, reason: String },

    #[error("could not connect to rc socket {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("rc call {command} timed out after {}s", CALL_TIMEOUT.as_secs())]
    Timeout { command: String },

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
    /// An insecure socket counts: we will not use it, so as far as everything above is
    /// concerned there is no rc. It is reported separately so the user can be told why.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            RcError::NotListening { .. } | RcError::InsecureSocket { .. } | RcError::Connect { .. }
        )
    }
}

/// A client for one rclone process's rc socket.
#[derive(Debug, Clone)]
pub struct RcClient {
    socket: PathBuf,
}

impl RcClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Check the socket is present, is a socket, is ours, and is private.
    ///
    /// Called before every connect rather than once at startup: the socket is recreated
    /// each time rclone restarts, and a check that ran against a previous incarnation
    /// says nothing about the current one.
    pub fn verify(&self) -> Result<(), RcError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};

            let md = std::fs::metadata(&self.socket).map_err(|e| {
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
                    reason: "not a socket".into(),
                });
            }

            // rclone creates the socket 0775 whatever it is asked for, so this is a real
            // condition rather than a defensive one; the mount unit sets UMask=0077 to
            // bring it down. Group and other must have nothing at all — write alone is
            // enough to connect.
            let mode = md.mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(RcError::InsecureSocket {
                    path: self.socket.clone(),
                    reason: format!("mode {mode:04o} allows access beyond its owner"),
                });
            }

            let uid = unsafe { libc_getuid() };
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
        let mut attempt = 0;
        loop {
            attempt += 1;
            match tokio::time::timeout(CALL_TIMEOUT, self.attempt(command, &params)).await {
                Ok(Ok(body)) => return Ok(body),
                Ok(Err(e)) => {
                    // A socket that is absent or unsafe will be just as absent or unsafe
                    // on the next attempt, and rclone rejecting the call is an answer.
                    // Only transport faults are worth retrying.
                    let retryable = matches!(e, RcError::Transport { .. });
                    if !retryable || attempt >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                }
                Err(_) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(RcError::Timeout {
                            command: command.to_string(),
                        });
                    }
                }
            }
            tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1)).await;
        }
    }

    async fn attempt(&self, command: &str, params: &serde_json::Value) -> Result<Vec<u8>, RcError> {
        use http_body_util::BodyExt;

        self.verify()?;

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

        let transport = |e: Box<dyn std::error::Error + Send + Sync>| RcError::Transport {
            command: command.to_string(),
            source: e,
        };

        // One connection per call. rc calls are infrequent and a pool would add failure
        // modes — a stale pooled connection to an rclone that has since restarted — for
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

        // The Host header is required by HTTP/1.1 and ignored by rclone; the authority is
        // meaningless over a UNIX socket, so any valid value does.
        let req = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(format!("/{command}"))
            .header(hyper::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Full::new(hyper::body::Bytes::from(
                serde_json::to_vec(params).unwrap_or_else(|_| b"{}".to_vec()),
            )))
            .map_err(|e| transport(Box::new(e)))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| transport(Box::new(e)))?;
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| transport(Box::new(e)))?
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

/// `getuid` without pulling in the `libc` crate for one call.
///
/// `rvt-core` links no system libraries by policy, and `libc` would be the first crack in
/// that. This is the only foreign function the crate needs.
#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            use std::os::unix::fs::PermissionsExt;

            let dir = std::env::temp_dir().join(format!("rvt-fakerc-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn an_insecure_socket_is_refused_before_anything_is_sent() {
        use std::os::unix::fs::PermissionsExt;

        let server =
            FakeRc::serving("insecure", "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").await;
        // rclone's own default, which is exactly what this check exists to catch.
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
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("rvt-rc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perms.sock");
        let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();

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
        let path = dir.join("not.sock");
        std::fs::write(&path, b"").unwrap();
        use std::os::unix::fs::PermissionsExt;
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
        assert!(!RcError::Failed {
            command: "vfs/queue".into(),
            status: 500,
            body: "boom".into()
        }
        .is_unreachable());
        assert!(!RcError::Timeout {
            command: "core/stats".into()
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
