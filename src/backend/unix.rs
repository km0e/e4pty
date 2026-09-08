//! Unix backend (Linux / macOS): `openpty(3)` via `rustix-openpty`.
//!
//! I/O model: the pty master fds are switched to non-blocking mode and
//! wrapped in [`tokio::io::unix::AsyncFd`], so reads and writes are truly
//! asynchronous — no blocking-pool hops, no dedicated threads. The
//! notorious Linux behavior of returning `EIO` (instead of EOF) when the
//! slave side disappears is normalized in the reader.
//!
//! The child is spawned through `tokio::process`, so [`PtyCtl::wait`] is
//! genuinely asynchronous (SIGCHLD is reaped by the tokio driver) and
//! never blocks a worker thread.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use async_trait::async_trait;
use rustix_openpty::rustix::termios::{self, Winsize};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;

use crate::error::Result;
use crate::pty::{Pty, PtyCtl, PtyReader, PtyWriter, WindowSize};
use crate::spawn::SpawnSpec;

/// Truly asynchronous wait handle around the tokio child.
struct UnixCtl {
    child: Child,
}

#[async_trait]
impl PtyCtl for UnixCtl {
    async fn wait(&mut self) -> Result<i32> {
        use std::os::unix::process::ExitStatusExt;
        // Reaped by the tokio signal driver — no worker thread is blocked.
        let es = self.child.wait().await?;
        // Map the status the shell way: normal exit → code, killed by
        // signal → 128 + signal (e.g. SIGTERM → 143), anything else → 1.
        Ok(es
            .code()
            .unwrap_or_else(|| es.signal().map_or(1, |v| 128 + v)))
    }
}

/// Write half of the pty master: a non-blocking dup of the master fd,
/// registered with the tokio reactor for writability.
struct UnixWriter {
    fd: AsyncFd<std::fs::File>,
}

impl AsyncWrite for UnixWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            // `&File` implements `Write`, matching `try_io`'s `&AsyncFd<T>`.
            match guard.try_io(|inner| inner.get_ref().write(buf)) {
                Ok(res) => return Poll::Ready(res),
                Err(_would_block) => continue,
            }
        }
    }

    // A tty master has no buffering to flush.
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    // There is no half-close on a tty master; dropping the writer or
    // closing the whole pty is the only way to hang up.
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[async_trait]
impl PtyWriter for UnixWriter {
    async fn window_change(&self, width: u16, height: u16) -> Result<()> {
        termios::tcsetwinsize(
            self.fd.get_ref(),
            termios::Winsize {
                ws_row: height,
                ws_col: width,
                ws_xpixel: 0, // TODO: pixel dimensions
                ws_ypixel: 0,
            },
        )?;
        Ok(())
    }

    // A tty master fd cannot be half-closed; EOF to the child cannot be
    // signaled without tearing down the whole session, so this stays a
    // no-op on Unix (see `PtyWriter::eof` for the Windows behavior).
    async fn eof(&self) -> Result<()> {
        Ok(())
    }
}

/// Read half of the pty master: a non-blocking handle of the master fd,
/// registered with the tokio reactor for readability.
struct UnixReader {
    fd: AsyncFd<std::fs::File>,
}

impl AsyncRead for UnixReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            match guard.try_io(|inner| inner.get_ref().read(buf.initialize_unfilled())) {
                Ok(Ok(0)) => return Poll::Ready(Ok(())), // clean EOF
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => {
                    if e.raw_os_error()
                        == Some(rustix_openpty::rustix::io::Errno::IO.raw_os_error())
                    {
                        // Linux: "slave side closed" reads back as EIO.
                        // Normalize to EOF so every platform behaves the
                        // same (same approach as portable-pty, node-pty…).
                        return Poll::Ready(Ok(()));
                    }
                    if e.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    return Poll::Ready(Err(e));
                }
                Err(_would_block) => continue,
            }
        }
    }
}

impl PtyReader for UnixReader {}

/// Spawn `spec.script` attached to a fresh pseudo-terminal (Unix backend).
///
/// Steps:
/// 1. resolve the script to `(program, args)` (shell scripts materialize
///    into self-cleaning temp files, see [`crate::script`]);
/// 2. `openpty` with the requested window size;
/// 3. mark the terminal as UTF-8 (`IUTF8`) so the line discipline handles
///    multi-byte input correctly;
/// 4. wire the child's stdin/stdout/stderr to the slave side;
/// 5. in the child (before exec): `setsid` + `TIOCSCTTY` so the pty
///    becomes the controlling terminal of a new session — this is what
///    makes job control (Ctrl+C, SIGTSTP, …) work;
/// 6. in the parent: keep two non-blocking fds to the master — a dup for
///    writes (CLOEXEC, never leaks into children) and the original for
///    reads — both registered with the tokio reactor.
pub(crate) fn openpty(spec: SpawnSpec) -> Result<Pty> {
    let resolved = spec.script.materialize()?;
    let WindowSize { rows, cols } = spec.window_size;

    // controller = master (we talk to it), user = slave (child attaches).
    let pair = rustix_openpty::openpty(
        None,
        Some(&Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
    )?;

    // Set character encoding to UTF-8 for the line discipline.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Ok(mut t) = termios::tcgetattr(&pair.controller) {
        t.input_modes.set(termios::InputModes::IUTF8, true);
        let _ = termios::tcsetattr(&pair.controller, termios::OptionalActions::Now, &t);
    }

    let mut builder = std::process::Command::new(&resolved.program);
    builder.args(&resolved.args);
    if let Some(dir) = &spec.current_dir {
        builder.current_dir(dir);
    }
    if let Some(vars) = spec.env {
        // Exact-environment mode: drop the inherited one entirely, then
        // apply the materialized list (see `EnvMod::materialize`).
        builder.env_clear();
        builder.envs(vars);
    }
    // Setup child stdin/stdout/stderr (each `try_clone` is a fresh fd that
    // std re-dups onto fds 0/1/2 without CLOEXEC at spawn time).
    builder.stdin(pair.user.try_clone()?);
    builder.stderr(pair.user.try_clone()?);
    builder.stdout(pair.user.try_clone()?);
    // Parent-side handle of the master, taken before `pair` moves into the
    // pre_exec closure below.
    let reader_src = pair.controller.try_clone()?;

    unsafe {
        use std::os::unix::process::CommandExt;
        builder.pre_exec(move || {
            use rustix::{io, process};

            // Become a session leader detached from our controlling tty…
            process::setsid()?;
            // …and adopt the slave as the new controlling terminal.
            process::ioctl_tiocsctty(&pair.user)?;

            // The child only needs the dup'd stdio fds; drop the originals
            // so the fd inventory is clean before exec.
            io::close(pair.user.as_raw_fd());
            io::close(pair.controller.as_raw_fd());
            Ok(())
        });
    }
    // TODO: set signal handler

    let child = tokio::process::Command::from(builder).spawn()?;

    use rustix::io;
    // Writer fd: duplicate of the master, CLOEXEC so it never leaks into
    // subsequently spawned children, and non-blocking for AsyncFd.
    let wfd = io::dup(&reader_src)?;
    io::fcntl_setfd(&wfd, io::fcntl_getfd(&wfd)? | io::FdFlags::CLOEXEC)?;
    // SAFETY: `wfd` is a fresh, uniquely owned fd from `dup`.
    let wfile = std::fs::File::from(wfd);
    make_nonblocking(&wfile)?;
    let writer = UnixWriter {
        fd: AsyncFd::new(wfile)?,
    };

    // Reader fd: the master itself (`reader_src` is already CLOEXEC —
    // rustix `try_clone` sets it), made non-blocking and registered.
    // SAFETY: `reader_src` is a uniquely owned `OwnedFd`.
    let rfile = std::fs::File::from(reader_src);
    make_nonblocking(&rfile)?;
    let reader = UnixReader {
        fd: AsyncFd::new(rfile)?,
    };

    Ok(Pty::new(UnixCtl { child }, writer, reader))
}

/// Switch a pty master fd to non-blocking mode (required by `AsyncFd`).
///
/// `O_NONBLOCK` lives in the open file description, which the writer dup
/// and the reader handle share — one call per handle is still fine and
/// keeps each site self-explanatory.
fn make_nonblocking(file: &std::fs::File) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(file)?;
    rustix::fs::fcntl_setfl(file, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}
