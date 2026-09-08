//! Handle traits and the [`Pty`] bundle: the platform-independent contract
//! every backend must fulfill.

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Result;

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
    /// Best effort and platform dependent: on Windows it closes the ConPTY
    /// input pipe (the child observes EOF on stdin); on Unix a tty master
    /// cannot be half-closed, so this is currently a no-op there.
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
    async fn wait(&mut self) -> Result<i32>;
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
    pub fn new(ctl: impl PtyCtl + 'static, writer: impl PtyWriter + 'static, reader: impl PtyReader + 'static) -> Self {
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

    /// Convenience passthrough to [`PtyCtl::wait`].
    pub async fn wait(&mut self) -> Result<i32> {
        self.ctl.wait().await
    }
}

impl From<(BoxedPtyCtl, BoxedPtyWriter, BoxedPtyReader)> for Pty {
    fn from((ctl, writer, reader): (BoxedPtyCtl, BoxedPtyWriter, BoxedPtyReader)) -> Self {
        Self { ctl, writer, reader }
    }
}
