//! Lifecycle-contract tests: the semantics documented in the crate root
//! ("Lifecycle contract"), pinned by experiment. See that section for the
//! definitions of reader-EOF vs. `wait`.
//!
//! Platform notes:
//! - `/proc` fd-census tests are `target_os = "linux"` only.
//! - EOF-before-exit (`std`-closing child) is a Unix pty property; under
//!   ConPTY the reader EOF tracks the console session end instead, so
//!   that test is Unix-only.
//! - `wait_can_resolve_before_reader_eof` stays `#[ignore]`d: it needs
//!   `pgrep(1)` and cleans up stray `sleep` processes, making it
//!   environment-sensitive; it documents a real, verified behavior.
//!
//! All scripts use `Script::exec` (no temp files) so these tests can run
//! concurrently with `smoke.rs`'s temp-file leak check.

use e4pty::prelude::*;
use std::time::Duration;
use tokio::io::AsyncReadExt;

#[cfg(unix)]
fn pgrep(pattern: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn kill_stray_sleeps() {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("pkill")
            .args(["-x", "sleep"])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "sleep.exe"])
            .status();
    }
}

/// A read-only consumer that never calls `wait` still receives EOF when
/// the child exits. This pins the EOF-liveness invariant: after spawn the
/// parent holds no slave fd copies (see `parent_holds_no_slave_fds`), so
/// EOF depends only on the child's session.
#[tokio::test]
async fn eof_without_wait() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "echo probe1; sleep 0.2; exit 0"]),
    )
    .expect("openpty failed");
    let (_ctl, _writer, mut reader) = pty.split();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_to_end(&mut out))
        .await
        .expect("EOF hijacked: read-only consumer never saw EOF")
        .expect("read failed");
    assert!(String::from_utf8_lossy(&out).contains("probe1"));
}

/// Output written just before the child exits is not lost across the
/// Linux `EIO`→EOF normalization (the kernel drains the tty buffer
/// before reporting EIO): 50k lines, immediate exit, all delivered.
#[tokio::test]
async fn no_output_loss_before_eof() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "seq 1 50000; sleep 1; exit 0"]),
    )
    .expect("openpty failed");
    let (mut ctl, _writer, mut reader) = pty.split();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), reader.read_to_end(&mut out))
        .await
        .expect("timed out")
        .expect("read failed");
    let code = tokio::time::timeout(Duration::from_secs(10), ctl.wait())
        .await
        .expect("wait timed out")
        .expect("wait failed");
    assert_eq!(code, 0);
    // ConPTY may hand us trailing empty lines alongside the payload.
    let text = String::from_utf8_lossy(&out);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 50000, "lines lost");
    assert_eq!(lines.last().unwrap().trim(), "50000", "tail lost");
}

/// `wait` without draining deadlocks: a chatty child fills the tty output
/// buffer and blocks in `write` forever. Drain-then-wait is the only safe
/// order — `Pty::finish` must drain first. Unix-only: the blocking is a
/// line-discipline/tty-buffer property; under ConPTY the output is a live
/// render stream and the child may exit regardless of the reader.
#[tokio::test]
#[cfg(unix)]
async fn wait_without_drain_deadlocks() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "seq 1 300000; exit 0"]),
    )
    .expect("openpty failed");
    let (mut ctl, _writer, mut reader) = pty.split();
    let wait_fut = ctl.wait();
    let res = tokio::time::timeout(Duration::from_secs(3), wait_fut).await;
    assert!(res.is_err(), "expected wait() to deadlock before any read");
    // Unblock the child and let it exit so the runtime shuts down cleanly.
    let mut sink = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), reader.read_to_end(&mut sink))
        .await
        .expect("drain timed out")
        .expect("read failed");
    let code = tokio::time::timeout(Duration::from_secs(10), ctl.wait())
        .await
        .expect("wait timed out after drain")
        .expect("wait failed");
    assert_eq!(code, 0);
}

/// `wait` resolving does not imply reader EOF: a background-pgrp
/// grandchild (`bash -m`) inherits the slave fds and keeps the master
/// alive well past the spawned child's exit. Ignored because it needs
/// `pgrep(1)`, kills stray `sleep` processes, and is timing-sensitive —
/// it documents a verified kernel behavior rather than guarding a
/// regression.
#[tokio::test]
#[cfg(unix)]
#[ignore = "environment-sensitive (pgrep, stray-process cleanup); see module docs"]
async fn wait_can_resolve_before_reader_eof() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("bash", ["-m", "-c", "sleep 30 & echo spawned; exit 0"]),
    )
    .expect("openpty failed");
    let (mut ctl, _writer, mut reader) = pty.split();
    let mut first = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read(&mut first))
        .await
        .expect("timed out")
        .expect("read failed");
    assert!(first[..n].starts_with(b"spawned"));

    let code = tokio::time::timeout(Duration::from_secs(5), ctl.wait())
        .await
        .expect("wait timed out")
        .expect("wait failed");
    assert_eq!(code, 0);

    // Probe EOF while the grandchild still holds the slave fds.
    let mut scratch = Vec::new();
    let eof = tokio::time::timeout(
        Duration::from_millis(1500),
        reader.read_to_end(&mut scratch),
    )
    .await;
    let grandchild_alive = pgrep("sleep 30");
    kill_stray_sleeps();
    let _ = reader.read_to_end(&mut Vec::new()).await;

    assert!(grandchild_alive, "grandchild died -- probe inconclusive");
    assert!(eof.is_err(), "expected EOF to stay pending past wait()");
}

/// Reader EOF does not imply child exit: a child that closes its own
/// stdio (daemonizer / `ssh -f` shape) produces EOF at once while it
/// keeps running; `wait` stays pending. Linux-only: EOF-at-stdio-close
/// is a Linux pty property — on macOS it may be deferred until exit,
/// and under ConPTY the console session (not the stdio handles) drives
/// the reader's EOF.
#[tokio::test]
#[cfg(target_os = "linux")]
async fn eof_can_resolve_before_child_exit() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "echo ready; exec sleep 60 0<&- 1>&- 2>&-"]),
    )
    .expect("openpty failed");
    let (mut ctl, _writer, mut reader) = pty.split();
    let mut first = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read(&mut first))
        .await
        .expect("timed out")
        .expect("read failed");
    assert!(first[..n].starts_with(b"ready"));

    let mut sink = Vec::new();
    let eof = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut sink))
        .await
        .expect("no EOF although the child closed its stdio")
        .expect("read failed");
    assert_eq!(eof, 0, "expected clean EOF");

    // The child is still alive, so `wait` must not resolve. (If it did,
    // the probe is inconclusive rather than the contract broken — but on
    // Linux the exec'd `sleep` reliably survives closing its stdio.)
    let wait = tokio::time::timeout(Duration::from_millis(1500), ctl.wait()).await;
    kill_stray_sleeps();
    assert!(
        wait.is_err(),
        "wait() returned although the child is still alive"
    );
}

/// `pid` is reported and `kill` terminates the child: Unix reports
/// `128 + SIGKILL` = 137, Windows reports the `TerminateProcess` code 1.
#[tokio::test]
async fn pid_and_kill() {
    let mut pty =
        openpty(WindowSize::default(), Script::exec("sleep", ["30"])).expect("openpty failed");
    let pid = pty.pid().expect("backend did not report a pid");
    assert!(pid > 0, "bogus pid {pid}");
    tokio::time::timeout(Duration::from_secs(5), pty.kill())
        .await
        .expect("kill timed out")
        .expect("kill failed");
    let code = tokio::time::timeout(Duration::from_secs(10), pty.wait())
        .await
        .expect("wait after kill timed out")
        .expect("wait failed");
    assert_ne!(code, 0, "expected nonzero exit after kill, got {code}");
}

/// `finish` collects output and exit code in one safe-order call.
#[tokio::test]
async fn finish_collects_output_and_code() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "echo finish-me; sleep 0.2; exit 3"]),
    )
    .expect("openpty failed");
    let (out, code) = tokio::time::timeout(Duration::from_secs(10), pty.finish())
        .await
        .expect("finish timed out")
        .expect("finish failed");
    assert!(String::from_utf8_lossy(&out).contains("finish-me"));
    assert_eq!(code, 3);
}

/// `finish` drains first: a chatty child that would deadlock a
/// wait-first consumer completes normally.
#[tokio::test]
async fn finish_drains_chatty_child() {
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "seq 1 200000; sleep 1; exit 7"]),
    )
    .expect("openpty failed");
    let (out, code) = tokio::time::timeout(Duration::from_secs(30), pty.finish())
        .await
        .expect("finish timed out")
        .expect("finish failed");
    assert_eq!(code, 7);
    // ConPTY may hand us trailing empty lines alongside the payload.
    let text = String::from_utf8_lossy(&out);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 200000);
    assert_eq!(lines.last().unwrap().trim(), "200000");
}
