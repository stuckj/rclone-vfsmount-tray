//! `Capabilities::probe` against a socket that answers, and one that does not.
//!
//! An integration test because it needs only the public API, and because the thing being
//! guarded is the whole path — the command name, the field the paths are read from, and
//! the fallback — rather than any one function.

#![cfg(unix)]

use rvt_core::rc::RcError;
use rvt_core::{Capabilities, RcClient, Tier};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Serves one canned response, then keeps accepting so nothing blocks on connect.
async fn serve(tag: &str, body: &str) -> (PathBuf, PathBuf, tokio::task::JoinHandle<()>) {
    serve_status(tag, 200, "OK", body).await
}

/// As [`serve`], with control over the status line.
async fn serve_status(
    tag: &str,
    code: u16,
    reason: &str,
    body: &str,
) -> (PathBuf, PathBuf, tokio::task::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!("rvt-probe-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // `verify()` refuses anything a third party could reach or replace.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.join("rc.sock");

    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();

    // Shared rather than leaked: `'static` is only needed to move it into the spawned
    // tasks, and an `Arc` gives that without deliberately leaking in a suite that
    // elsewhere asserts on file-descriptor counts.
    let response = std::sync::Arc::new(format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let response = response.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    (dir, socket, handle)
}

fn captured_rc_list() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/rc-list-v1.75.0.json"
    ))
    .expect("testdata/rc-list-v1.75.0.json")
}

#[tokio::test]
async fn probing_a_real_rclone_reads_its_command_paths() {
    // The whole path, against the captured payload: if the wrong field were read, every
    // `has()` would answer false and every mount would resolve to T4 while looking as
    // though rclone had simply said so.
    let (dir, socket, server) = serve("ok", &captured_rc_list()).await;

    let caps = Capabilities::probe(&RcClient::new(&socket))
        .await
        .expect("a socket that answers is not an error");

    assert!(!caps.is_empty(), "the capture registers commands");
    assert!(caps.has("core/stats"), "core/stats must be recognised");
    assert!(caps.has("vfs/queue") && caps.has("vfs/stats"));
    assert_eq!(caps.tier(), Tier::T1);
    assert!(
        caps.degraded_reason().is_none(),
        "a genuine answer is not a degradation"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn probing_an_absent_socket_falls_through_to_the_disk_scan() {
    // #13: "fall through to T4 when the socket is unreachable entirely". Surfacing this
    // as an error instead would make an unmounted mount look broken on every poll.
    let missing =
        std::env::temp_dir().join(format!("rvt-probe-absent-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&missing);

    let caps = Capabilities::probe(&RcClient::new(&missing))
        .await
        .expect("an absent socket resolves to T4 rather than failing");

    assert!(caps.is_empty());
    assert_eq!(caps.tier(), Tier::T4);
    assert!(
        caps.degraded_reason().is_some(),
        "the reason must survive, or a refusal is indistinguishable from a genuine answer"
    );
}

#[tokio::test]
async fn a_socket_we_refuse_says_so_rather_than_looking_like_an_idle_rclone() {
    // rclone binds its socket 0777 & ~umask, so this is the shape of a real
    // misconfiguration — and it is permanent until the user fixes it. Both cases resolve
    // to T4, so the reason is the only thing that can tell them apart.
    let (dir, socket, server) = serve("insecure", &captured_rc_list()).await;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o777)).unwrap();

    let caps = Capabilities::probe(&RcClient::new(&socket))
        .await
        .expect("a refusal degrades rather than failing");

    assert_eq!(caps.tier(), Tier::T4);
    let why = caps
        .degraded_reason()
        .expect("a refused socket must carry its reason");
    assert!(
        why.contains("not private"),
        "the reason should name the problem the user can fix: {why}"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_rclone_that_answers_with_an_error_is_a_fault_not_a_degrade() {
    // The other half of the error model. Degrading here would hide a real rclone fault
    // behind a mount that merely looks unable to do much, permanently and with no log.
    let (dir, socket, server) = serve_status("500", 500, "Internal Server Error", "boom").await;

    let e = Capabilities::probe(&RcClient::new(&socket))
        .await
        .expect_err("rclone answered, so this is a fault");
    match e {
        RcError::Failed {
            command, status, ..
        } => {
            assert_eq!(command, "rc/list");
            assert_eq!(status, 500);
        }
        other => panic!("expected the error to surface, got {other:?}"),
    }

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_renamed_commands_field_is_an_error_rather_than_an_empty_answer() {
    // The drift this whole detection strategy exists to survive. If it defaulted to an
    // empty command set, every mount would sit at T4 reporting nothing wrong — disk-scan
    // fidelity presented as rclone's own answer, with no error anywhere to explain it.
    let (dir, socket, server) = serve("renamed", r#"{"list":[{"Path":"core/stats"}]}"#).await;

    let e = Capabilities::probe(&RcClient::new(&socket))
        .await
        .expect_err("a payload we cannot read must not look like an empty one");
    assert!(
        matches!(e, RcError::Decode { .. }),
        "wire-format drift must surface as a decode error, got {e:?}"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
