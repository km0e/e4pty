# e4pty

**A minimal, async PTY abstraction for Rust** — one uniform API over
Unix `openpty` (Linux / macOS) and Windows ConPTY, built on `tokio`.

Spawn a process attached to a pseudo-terminal, read its output, write
input to it, resize the terminal window, and collect the exit status —
with the same code on every supported platform.

## Features

- **Cross-platform**: Unix openpty (`rustix-openpty`) and Windows ConPTY
  (`windows` crate) behind a single API.
- **Truly async I/O**: non-blocking fds via `AsyncFd` on Unix; dedicated
  I/O threads behind tokio channels on Windows (ConPTY pipes are
  blocking-only). No executor thread is ever blocked.
- **Truly async `wait`**: children are spawned through `tokio::process`
  and reaped by the tokio driver.
- **EOF normalization**: reading the pty master after the child exits
  returns `EIO` on Linux but EOF on other platforms; e4pty normalizes it
  to a clean EOF so consumers get identical behavior everywhere.
- **Script execution**: run one-shot scripts through `sh`, `bash` or
  PowerShell via a self-cleaning temp file — one shared code path for
  all backends, and the cleanup hook is installed *before* the user
  source, so early `exit` cannot leak the file.
- **Terminal resize**: `window_change` propagates new dimensions to the
  pty (`TIOCSWINSZ` on Unix, `ResizePseudoConsole` on Windows).
- **Sane exit codes**: waits report `128 + signal` for signal-terminated
  children on Unix, mirroring shell convention.

## Platform support

| Platform | Backend | Status |
|---|---|---|
| Linux (glibc / musl, x86_64 / aarch64) | `openpty` | runtime-tested |
| macOS (Intel / Apple Silicon) | `openpty` | compile-verified; runtime-tested in CI (`macos-latest`) |
| Windows (x86_64 / aarch64, msvc + gnu) | ConPTY | compile-verified; runtime-tested in CI (`windows-latest`) |

MSRV: **1.85** (edition 2024).

## Usage

```toml
[dependencies]
e4pty = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-util"] }
```

```rust
use e4pty::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> e4pty::Result<()> {
    let mut pty = openpty(
        WindowSize { rows: 24, cols: 80 },
        Script::sh("echo hello from $TERM"),
    )?;

    // Optional: resize the terminal.
    pty.writer.window_change(100, 30).await?;

    // Feed input to the process.
    pty.writer.write_all(b"ls\n").await?;

    // Read until EOF (emitted when the child exits).
    let mut out = Vec::new();
    pty.reader.read_to_end(&mut out).await?;
    print!("{}", String::from_utf8_lossy(&out));

    // Collect the exit status.
    let code = pty.wait().await?;
    println!("[exit: {code}]");
    Ok(())
}
```

The three handles are independent — split them across tasks:

```rust
let (mut ctl, writer, mut reader) = pty.split();
tokio::spawn(async move { /* drain reader */ });
tokio::spawn(async move { /* wait for exit */ });
```

## API overview

`openpty(window_size, script) -> Result<Pty>` spawns the process:

| Field / method | Purpose |
|---|---|
| `pty.reader` (`AsyncRead`) | process output, terminal escape sequences included |
| `pty.writer` (`PtyWriter`) | input (`AsyncWrite`), `window_change`, `eof` |
| `pty.ctl` (`PtyCtl`) | `wait` for the exit code |
| `pty.wait()` | shorthand for `ctl.wait()` |
| `pty.split()` | destructure into the three owned handles |

### Scripts

`Script` describes what to run; **no shell is ever involved unless you
ask for one**:

- `Script::line("vim /tmp/f")` — tokenize on whitespace, exec directly.
  Convenient for quick commands; quoting is *not* interpreted.
- `Script::exec("docker", ["compose", "up", "-d"])` — verbatim argv
  (arguments may contain spaces). Also: `Script::from(&["ls", "-la"])`.
- `Script::sh("...")` / `Script::bash("...")` / `Script::powershell("...")`
  — write the source to a temp file and execute it with the interpreter.
  The file installs a self-cleanup hook **before** the user source, so
  the temp file is removed even when the script calls `exit` early.

> Platform notes on shell scripts:
> - Unix `sh`/`bash` are looked up on `PATH`; PowerShell is invoked as
>   `powershell -File <script>` (use `pwsh` aliases yourself if needed).
> - On Windows, `sh`/`bash` require a POSIX environment on `PATH`
>   (e.g. Git-for-Windows); there is no implicit WSL fallback.

## Design notes

- **Unix**: the master fds are non-blocking and registered with the tokio
  reactor (`AsyncFd`) — reads/writes never hop through the blocking pool.
  The writer fd is a `CLOEXEC` dup, so it never leaks into children.
  Children start with `setsid` + `TIOCSCTTY`, making the pty the
  controlling terminal of a new session — job control (`Ctrl+C`,
  `SIGTSTP`) works as expected.
- **Windows**: ConPTY pipes only do blocking I/O, so two dedicated threads
  own the pipe ends and bridge to async via bounded tokio channels.
  Drops are safe in any order: the `HPCON` lives behind an
  `Arc<Mutex<Option<_>>>` taken by `ClosePseudoConsole`, and closing the
  conin handle is what delivers EOF to the child. All handles are closed
  with `CloseHandle`/`ClosePseudoConsole`; arguments are quoted with the
  standard Windows argv escaping rules and the program is resolved via
  `SearchPathW`.
- `PtyWriter::eof` is best-effort: it closes the ConPTY input pipe on
  Windows (child sees EOF on stdin); a Unix tty master cannot be
  half-closed, so it is a no-op there — send `0x04` (Ctrl+D) to `cat`-like
  readers in canonical mode instead.

## Testing

The repository ships integration tests (`tests/smoke.rs`) covering
output, verbatim argv, input echo, window resize, signal exit codes,
temp-file cleanup and independent handle usage across tasks:

```sh
cargo test
```

Verified environments: Arch (glibc 2.44), Debian glibc (`rust:latest`),
Alpine musl (`rust:alpine`), MSRV 1.85, plus `cargo check` cross-builds
for `aarch64-unknown-linux-gnu`, `aarch64/x86_64-apple-darwin` and
`x86_64/aarch64-pc-windows-{msvc,gnu}`.

> **Why no macOS Docker image?** Containers share the host kernel and
> Apple does not license macOS for non-Apple hardware, so macOS cannot be
> containerized ("Docker-OSX" style projects are full VMs and violate the
> EULA). macOS is therefore verified in two layers: `cargo check --target
> {aarch64,x86_64}-apple-darwin` for compile adaptation (works on any
> Linux host, no SDK needed), and the `macos-latest` GitHub Actions job
> for runtime tests. Windows follows the same pattern
> (`*-pc-windows-{msvc,gnu}` checks + `windows-latest` CI job).

CI (`.github/workflows/ci.yml`) runs the test suite on all three
platforms, an MSRV job, clippy/doc lints and the cross-target matrix.

## Changelog

### 0.2.0 (breaking)

- `openpty_local` → `openpty`; `BoxedPty` → `Pty` (`destruct` → `split`).
- `Script` rewritten: owned data, one lifetime, `Line` / `Exec` / `Shell`
  variants with `line` / `exec` / `sh` / `bash` / `powershell`
  constructors; `ScriptExecutor` renamed to `Shell`.
- Unix I/O moved from `tokio::fs::File` (blocking pool) to `AsyncFd`;
  `wait` moved from blocking `std::process` to `tokio::process`.
- Windows: dedicated blocking-I/O threads replace blocking
  `ReadFile`/`WriteFile` in `poll_*`; correct `CloseHandle` hygiene
  (the previous code freed pipe handles with `CoTaskMemFree`-style
  `free()` and leaked the child thread handle); spin-wait drops removed.
- Windows: command lines are now properly quoted; PowerShell scripts use
  `-File`; error enum lost the unused `Unknown` variant.

### 0.1.9

- Fixed Linux `EIO`-instead-of-EOF on the pty master reader.
- Fixed shell-script temp-file cleanup (hook is now installed before the
  user script, so early `exit` cannot leak it).
- Added missing `Win32_Security` feature required to build for Windows.

## License

MIT — see [LICENSE](LICENSE).
