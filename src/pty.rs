//! Handle traits and the [`Pty`] bundle: the platform-independent contract
//! every backend must fulfill.

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::{Error, Result};

/// Desired terminal dimensions for a freshly created pty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSize {
    /// Number of text rows (lines).
    pub rows: u16,
    /// Number of columns per row.
    pub cols: u16,
}

impl WindowSize {
    /// A `rows` x `cols` window.
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

impl Default for WindowSize {
    /// The classic 24 x 80 terminal.
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// Write side of a pty: send input to the process and control the window.
///
/// Implemented together with [`AsyncWrite`], so plain `write_all` pushes
/// keystrokes/data into the terminal as if typed by a user.
#[async_trait]
pub trait PtyWriter: AsyncWrite + Send + Sync + Unpin {
    /// Inform the child that the terminal was resized to
    /// `width` x `height` columns/rows (drives a `SIGWINCH` on Unix).
    async fn window_change(&self, width: u16, height: u16) -> Result<()>;

    /// Signal end-of-input to the process.
    ///
    /// Best effort and platform dependent:
    /// - **Windows**: closes the ConPTY *input pipe*. Empirically ConPTY
    ///   treats a closed input stream as a console-close signal: attached
    ///   clients are terminated (`STATUS_CONTROL_C_EXIT`), rather than
    ///   merely observing stdin EOF. Prefer sending `0x1A` (`Ctrl+Z`) or
    ///   `0x04` (msys tools) followed by a newline when the child should
    ///   survive.
    /// - **Unix**: a tty master cannot be half-closed, so this is a
    ///   no-op — send `0x04` (canonical-mode EOT) instead.
    async fn eof(&self) -> Result<()>;
}

/// Owned type-erased [`PtyWriter`].
pub type BoxedPtyWriter = Box<dyn PtyWriter + 'static>;

/// Marker trait for the read side of a pty (a plain [`AsyncRead`] that
/// yields the process output, terminal escape sequences included).
///
/// Reading returns a clean EOF once the child has exited on every
/// platform (Linux `EIO` on the pty master is normalized away).
pub trait PtyReader: AsyncRead + Send + Sync + Unpin {}

/// Owned type-erased [`PtyReader`].
pub type BoxedPtyReader = Box<dyn PtyReader + 'static>;

/// Control handle of a pty: lifecycle of the spawned child.
#[async_trait]
pub trait PtyCtl: Send + Sync + Unpin {
    /// Wait until the child exits and return its exit code.
    ///
    /// On Unix, a child killed by a signal is reported the shell way,
    /// `128 + signal` (e.g. `SIGTERM` → 143); a normal exit yields the
    /// `exit(2)` status. The wait is truly asynchronous — it never blocks
    /// the executor's worker threads.
    ///
    /// This resolves when the *spawned child process* is reaped —
    /// independently of the reader's EOF: it may complete while the reader
    /// still has output pending (e.g. a descendant keeping the pty's slave
    /// side open), and the reader may see EOF while the process is still
    /// alive (the child closed its own stdio). See the
    /// [`lifecycle contract`](crate#lifecycle-contract) in the crate docs
    /// for the teardown order that handles both.
    async fn wait(&mut self) -> Result<i32>;

    /// The OS process id of the spawned child, if the backend can report
    /// it. Both built-in backends always do; the `None` default exists
    /// for third-party implementations.
    fn pid(&self) -> Option<u32> {
        None
    }

    /// Initiate termination of the spawned child: `SIGKILL` on Unix,
    /// `TerminateProcess` (exit code `1`) on Windows. No-op if the child
    /// has already exited.
    ///
    /// This only signals the spawned child — other processes in the pty
    /// session are not touched. Tear the whole session down by dropping
    /// the reader and writer handles (closing the master sends `SIGHUP`
    /// to the foreground process group on Unix; closing the ConPTY
    /// implicitly ends the session on Windows).
    ///
    /// Non-blocking: collect the exit code afterwards with
    /// [`PtyCtl::wait`] (Unix reports `128 + SIGKILL` = `137`).
    async fn kill(&mut self) -> Result<()> {
        let _ = self;
        Err(Error::IO(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this pty backend does not support kill",
        )))
    }
}

/// Owned type-erased [`PtyCtl`].
pub type BoxedPtyCtl = Box<dyn PtyCtl + 'static>;

/// The three handles of one spawned pty session.
///
/// Fields are public, so the bundle can be destructured directly; the
/// handles are independent objects and are meant to live in separate
/// tasks (reader task, writer task, supervisor).
pub struct Pty {
    /// Lifecycle control (wait for exit).
    pub ctl: BoxedPtyCtl,
    /// Input / resize side.
    pub writer: BoxedPtyWriter,
    /// Output side (EOF-normalized, see [`PtyReader`]).
    pub reader: BoxedPtyReader,
}

impl Pty {
    /// Bundle three concrete handle implementations into a [`Pty`].
    pub fn new(
        ctl: impl PtyCtl + 'static,
        writer: impl PtyWriter + 'static,
        reader: impl PtyReader + 'static,
    ) -> Self {
        Self {
            ctl: Box::new(ctl),
            writer: Box::new(writer),
            reader: Box::new(reader),
        }
    }

    /// Consume the bundle and return the three handles separately.
    pub fn split(self) -> (BoxedPtyCtl, BoxedPtyWriter, BoxedPtyReader) {
        (self.ctl, self.writer, self.reader)
    }

    /// The OS process id of the spawned child, if the backend reports it.
    pub fn pid(&self) -> Option<u32> {
        self.ctl.pid()
    }

    /// Initiate termination of the spawned child; see [`PtyCtl::kill`].
    ///
    /// Non-blocking: collect the exit code afterwards with [`Pty::wait`].
    pub async fn kill(&mut self) -> Result<()> {
        self.ctl.kill().await
    }

    /// Convenience passthrough to [`PtyCtl::wait`].
    pub async fn wait(&mut self) -> Result<i32> {
        self.ctl.wait().await
    }

    /// Consume the pty, drain the reader to EOF and collect the exit code.
    ///
    /// Batch-mode convenience that implements the safe teardown order
    /// (see the [`lifecycle contract`](crate#lifecycle-contract)): the
    /// reader is drained *first* — a child blocked writing to a full tty
    /// output buffer would otherwise never exit, and a `wait`-first call
    /// would deadlock — while `wait` runs concurrently so children that
    /// close their stdio early are still reaped. Returns
    /// `(output, exit_code)`.
    ///
    /// This completes only when the reader sees EOF: descendants that
    /// keep the pty's slave side open postpone it past `wait`, and a
    /// child that blocks *reading* input never produces it — end the
    /// input with `0x04` (canonical-mode EOT) or [`PtyWriter::eof`]
    /// (Windows) instead.
    pub async fn finish(self) -> Result<(Vec<u8>, i32)> {
        let (mut ctl, writer, mut reader) = self.split();
        // Reap the child concurrently; the drain below is the sequencing
        // authority and also unblocks children stuck writing. The writer
        // is deliberately held until the end: on Windows closing the
        // conin pipe is a console-close signal (clients die with
        // STATUS_CONTROL_C_EXIT), and on Unix a tty master cannot be
        // half-closed anyway — so both platforms keep input open here.
        let waiter = tokio::spawn(async move { ctl.wait().await });
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await?;
        let code = waiter
            .await
            .map_err(|join| Error::IO(std::io::Error::other(join)))??;
        drop(writer);
        Ok((out, code))
    }
}

impl From<(BoxedPtyCtl, BoxedPtyWriter, BoxedPtyReader)> for Pty {
    fn from((ctl, writer, reader): (BoxedPtyCtl, BoxedPtyWriter, BoxedPtyReader)) -> Self {
        Self {
            ctl,
            writer,
            reader,
        }
    }
}
