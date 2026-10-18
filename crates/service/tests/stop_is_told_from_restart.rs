//! The service, run as a real systemd user unit: it attempts its `auto_mount` mounts as it
//! starts, and it tells an explicit stop from a restart.
//!
//! `unmount_on_service_stop` rests on the second: both arrive as the same `SIGTERM`, and
//! only the job systemd has queued on the unit says which it was. The one mount is served
//! by a stub rclone that answers `version` and refuses to mount, so this needs neither
//! rclone nor FUSE — but it does need a systemd user instance, and it skips without one.
//! It also skips when a service already owns the bus name, rather than disturb it.

use rvt_testutil::Scratch;
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_rclone-vfsmount-trayd");

/// Real `rclone version` output, from the machine used in #9.
const VERSION_OUTPUT: &str = "rclone v1.75.0\n- os/version: ubuntu 26.04 (64 bit)\n";

fn ok(cmd: &mut Command) -> bool {
    cmd.output().is_ok_and(|o| o.status.success())
}

fn systemctl(args: &[&str]) -> bool {
    ok(Command::new("systemctl").arg("--user").args(args))
}

/// The unit's journal since it was started, with the ANSI colouring taken out.
fn journal(unit: &str) -> String {
    let out = Command::new("journalctl")
        .args(["--user", "-u", unit, "-o", "cat", "--no-pager"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut plain = String::with_capacity(out.len());
    let mut chars = out.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
}

/// Wait until the journal shows `needle` at least `times` times.
fn await_line(unit: &str, needle: &str, times: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = journal(unit);
        if text.matches(needle).count() >= times || Instant::now() > deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Stops and clears units however the test leaves.
struct StopOnDrop([String; 2]);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        for unit in &self.0 {
            systemctl(&["stop", unit]);
            systemctl(&["reset-failed", unit]);
        }
    }
}

/// The units a mount unit is ordered before, once it has been started.
fn mount_unit_before(unit: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = Command::new("systemctl")
            .args(["--user", "show", "-p", "Before", "--value", unit])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if !out.is_empty() || Instant::now() > deadline {
            return out;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn the_real_unit_auto_mounts_and_tells_a_stop_from_a_restart() {
    if !systemctl(&["show-environment"]) {
        eprintln!("skipped: no systemd user instance");
        return;
    }
    if ok(Command::new("busctl").args(["--user", "status", "io.github.stuckj.RcloneVfsmountTray"]))
    {
        eprintln!("skipped: a service already owns the bus name");
        return;
    }

    let dir = Scratch::new("stop-told-from-restart");
    // Answers `version`; any `mount` fails, so the auto-mount below is attempted and
    // nothing is ever actually mounted.
    let rclone = dir.write(
        "rclone",
        format!("#!/bin/sh\n[ \"$1\" = version ] || exit 1\ncat <<'EOF'\n{VERSION_OUTPUT}EOF\n")
            .as_bytes(),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rclone, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mount = format!("stoptest-{}", std::process::id());
    let mount_unit = format!("rvt-mount-{mount}.service");
    let config = dir.write(
        "config.toml",
        format!(
            "version = 1\n[global]\nrclone_path = {:?}\nunmount_on_service_stop = true\n\
             [[mount]]\nname = {mount:?}\nremote = \"stub\"\nmount_point = {:?}\n",
            rclone.display().to_string(),
            dir.join("mnt").display().to_string(),
        )
        .as_bytes(),
    );

    let unit = format!("rvt-stoptest-{}.service", std::process::id());
    let started = ok(Command::new("systemd-run").args([
        "--user",
        "--quiet",
        &format!("--unit={unit}"),
        "-p",
        "Type=exec",
        // The budget comes from this, so the log line below can be pinned.
        "-p",
        "TimeoutStopSec=20",
        BIN,
        "--config",
        &config.display().to_string(),
    ]));
    assert!(started, "systemd-run could not start the service");
    // A running one holds the real bus name, so it must not outlive a failed assertion.
    let _cleanup = StopOnDrop([unit.clone(), mount_unit.clone()]);

    let up = await_line(&unit, "serving", 1);
    if up.contains("already owns") {
        eprintln!("skipped: a service took the bus name first");
        return;
    }
    assert!(up.contains("serving"), "the service never came up:\n{up}");
    let attempted = await_line(&unit, "auto-mounting", 1);
    let ordered = mount_unit_before(&mount_unit);

    assert!(systemctl(&["restart", &unit]));
    let restarted = await_line(&unit, "serving", 2);

    assert!(systemctl(&["stop", &unit]));
    let stopped = await_line(&unit, "budget_secs", 1);

    assert!(
        ordered.split_whitespace().any(|u| u == unit),
        "a mount unit must be ordered before the service's own unit, so a logout stops \
         the service first; it had Before={ordered}"
    );
    assert!(
        attempted.contains(&format!("auto-mounting mount=\"{mount}\"")),
        "a mount marked auto_mount (the default) must be brought up at start:\n{attempted}"
    );

    let left = restarted
        .lines()
        .find(|l| l.contains("mounts are left as they are"))
        .unwrap_or_else(|| panic!("the restart did not say what it left:\n{restarted}"));
    assert!(
        left.contains("why=Restart"),
        "a restart must be read as one, whatever the option says: {left}"
    );
    assert!(
        stopped.contains("budget_secs=15"),
        "an explicit stop with the option on takes the unmount path, inside the unit's \
         stop timeout less a margin:\n{stopped}"
    );
}
