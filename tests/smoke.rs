use e4pty::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serialize tests that create temp script files, so the leak check in
/// `smoke_temp_script_self_cleanup` is deterministic under parallel runs.
static TMP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Spawn a `sh` script in a pty, read it to EOF and return (output, exit).
async fn run_script(input: &str) -> (String, i32) {
    let mut pty = openpty(WindowSize::default(), Script::sh(input)).expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    (String::from_utf8_lossy(&out).into_owned(), code)
}

#[tokio::test]
async fn smoke_basic() {
    let _g = TMP_LOCK.lock().await;
    let (out, code) = run_script("echo hello-e4pty; exit 0").await;
    assert!(out.contains("hello-e4pty"), "unexpected output: {out:?}");
    assert_eq!(code, 0, "exit code mismatch");
}

#[tokio::test]
async fn smoke_line_form() {
    // `Script::line` tokenizes and execs directly, no temp file, no shell.
    // macOS/BSD flushes pty output queues when an instant-exiting child's
    // slave side closes, which can race away output written microseconds
    // before exit — so macOS wraps the echo in a shell that lingers.
    #[cfg(target_os = "macos")]
    let script = Script::exec("sh", ["-c", "echo line-form-works; sleep 0.2"]);
    #[cfg(not(target_os = "macos"))]
    let script = Script::line("echo line-form-works");
    let mut pty = openpty(WindowSize::default(), script).expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("line-form-works"), "output: {text:?}");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn smoke_exec_argv() {
    // `Script::exec` passes verbatim argv (spaces inside args survive).
    let mut pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "printf 'argv with  spaces'; exit 0"]),
    )
    .expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("argv with  spaces"), "output: {text:?}");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn smoke_write_and_window_change() {
    let _g = TMP_LOCK.lock().await;
    let mut pty = openpty(
        WindowSize::default(),
        Script::sh("read line; echo got:$line; stty size; exit 0"),
    )
    .expect("openpty failed");

    pty.writer
        .window_change(120, 40)
        .await
        .expect("resize failed");
    pty.writer
        .write_all(b"ping-from-test\r\n")
        .await
        .expect("write failed");

    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("got:ping-from-test"), "output: {text:?}");
    // stty size should reflect the resized window (40 rows 120 cols)
    assert!(text.contains("40 120"), "window size not applied: {text:?}");

    let code = pty.wait().await.expect("wait failed");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn smoke_exit_code_signal() {
    let _g = TMP_LOCK.lock().await;
    // child killed by signal -> exit code should be 128 + signal (unix);
    // Windows/msys signal emulation varies, so only unix asserts it.
    let (_out, code) = run_script("kill -TERM $$").await;
    #[cfg(unix)]
    assert_eq!(code, 128 + 15, "signal exit code mismatch, got {code}");
    #[cfg(not(unix))]
    let _ = code;
}

#[tokio::test]
async fn smoke_current_dir() {
    // canonicalize so `pwd` (physical cwd) matches even when the temp dir
    // itself is reached through a symlink (macOS /tmp → /private/tmp).
    let dir = std::env::temp_dir().canonicalize().unwrap();
    // Instant-exit children can race their own final output: macOS/BSD
    // flushes the pty queues at slave close, and on Windows the ConPTY is
    // closed when the spawned child exits (render-to-pipe lag). Keep the
    // session briefly alive on every platform.
    let script = Script::exec("sh", ["-c", "pwd; sleep 0.2"]);
    let mut pty = PtyBuilder::new(WindowSize::default(), script)
        .current_dir(&dir)
        .spawn()
        .expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    let text = String::from_utf8_lossy(&out);
    assert_eq!(code, 0);
    // Windows: msys `pwd` prints POSIX-style paths (`/c/...`), while the
    // canonicalized dir comes back as `\\?\C:\...` — convert for the
    // comparison and use `contains` (the output carries VT noise).
    #[cfg(windows)]
    {
        let s = dir.to_string_lossy();
        let s = s.trim_start_matches(r"\\?\");
        let drive = s.chars().next().unwrap().to_ascii_lowercase();
        let rest: String = s.chars().skip(2).collect();
        let msys = format!("/{drive}{}", rest.replace('\\', "/"));
        assert!(
            text.contains(&msys),
            "cwd not applied, got {text:?} want {msys:?}"
        );
    }
    #[cfg(not(windows))]
    assert!(
        text.trim().ends_with(&dir.to_string_lossy().to_string()),
        "cwd not applied, got {text:?} want {dir:?}"
    );
}

#[tokio::test]
async fn smoke_env_set_and_inherit() {
    // One var overridden, the rest of the parent environment inherited.
    let mut pty = PtyBuilder::new(
        WindowSize::default(),
        Script::exec("sh", ["-c", "echo v=$E4PTY_TEST i=${PATH:+inherited}"]),
    )
    .env("E4PTY_TEST", "hello-env")
    .spawn()
    .expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    let text = String::from_utf8_lossy(&out);
    assert_eq!(code, 0);
    assert!(text.contains("v=hello-env"), "env not set: {text:?}");
    assert!(text.contains("i=inherited"), "env not inherited: {text:?}");
}

#[tokio::test]
async fn smoke_env_remove() {
    // A neutral variable instead of `HOME`: msys environments (Windows)
    // re-derive HOME themselves, so removing it would not be observable.
    // Setting it in-process is safe under the serial test run.
    unsafe { std::env::set_var("E4PTY_REMOVE_ME", "1") };
    let mut pty = PtyBuilder::new(
        WindowSize::default(),
        Script::exec(
            "sh",
            [
                "-c",
                "[ -z \"$E4PTY_REMOVE_ME\" ] && echo gone || echo still-there",
            ],
        ),
    )
    .env_remove("E4PTY_REMOVE_ME")
    .spawn()
    .expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    let text = String::from_utf8_lossy(&out);
    assert_eq!(code, 0);
    assert!(text.contains("gone"), "env_remove failed: {text:?}");
}

#[tokio::test]
#[cfg(unix)] // a fully empty environment can upset msys sh.exe on Windows
async fn smoke_env_clear() {
    let mut pty = PtyBuilder::new(
        WindowSize::default(),
        Script::exec("sh", ["-c", "echo P=${PATH:+set} H=${HOME-unset}"]),
    )
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .spawn()
    .expect("openpty failed");
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await.expect("read failed");
    let code = pty.wait().await.expect("wait failed");
    let text = String::from_utf8_lossy(&out);
    assert_eq!(code, 0);
    assert!(text.contains("P=set"), "env after clear missing: {text:?}");
    assert!(text.contains("H=unset"), "env_clear failed: {text:?}");
}

#[tokio::test]
#[cfg(unix)] // inspects the unix temp dir for leaked script files
async fn smoke_temp_script_self_cleanup() {
    let _g = TMP_LOCK.lock().await;
    // Shell scripts write a temp file with a cleanup hook installed before
    // the user source; even an early `exit` must not leak the file.
    let count_tmp = || {
        std::fs::read_dir("/tmp")
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
            .count()
    };
    let before = count_tmp();
    let (out, code) = run_script("echo done; exit 0").await;
    assert!(out.contains("done"));
    assert_eq!(code, 0);
    let after = count_tmp();
    assert_eq!(before, after, "temp script files leaked");
}

#[tokio::test]
async fn smoke_split_handles() {
    // `Pty::split` must yield independently usable handles: drive reader
    // and ctl from different tasks while writing from this one. The child
    // reads a line and echoes it, then exits on its own — no EOT needed
    // (0x04 is a Unix line-discipline convention that conhost ignores).
    let _g = TMP_LOCK.lock().await;
    let pty = openpty(
        WindowSize::default(),
        Script::exec("sh", ["-c", "read line; echo got:$line; exit 0"]),
    )
    .expect("openpty failed");
    let (mut ctl, mut writer, mut reader) = pty.split();

    let reader_task = tokio::spawn(async move {
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("read failed");
        String::from_utf8_lossy(&out).into_owned()
    });
    let ctl_task = tokio::spawn(async move { ctl.wait().await.expect("wait failed") });

    writer
        .write_all(b"split-handles\r\n")
        .await
        .expect("write failed");
    // Keep the writer alive: dropping it closes conin, which ConPTY treats
    // as a console-close signal (clients terminated). The script exits on
    // its own after echoing.

    let out = reader_task.await.expect("reader task panicked");
    assert!(out.contains("got:split-handles"), "output: {out:?}");
    assert_eq!(ctl_task.await.expect("ctl task panicked"), 0);
}
