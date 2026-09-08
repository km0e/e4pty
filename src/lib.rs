//! Minimal, async PTY (pseudo-terminal) abstraction.
//!
//! `e4pty` exposes one uniform API over Unix `openpty` (Linux, macOS) and
//! Windows ConPTY: spawn a process attached to a pty, read its output,
//! write input to it, resize the terminal window and collect the exit
//! status — with identical semantics on every supported platform.
//!
//! Highlights:
//!
//! - **truly async I/O** — non-blocking fds via `AsyncFd` on Unix,
//!   dedicated I/O threads behind channels on Windows (ConPTY pipes are
//!   blocking-only); no executor thread is ever blocked;
//! - **truly async `wait`** — the child is reaped by the tokio driver;
//! - **EOF normalization** — reading the pty master after the child exits
//!   returns `EIO` on Linux but EOF elsewhere; `e4pty` normalizes this so
//!   consumers never special-case the OS;
//! - **uniform script handling** — shell scripts materialize into
//!   self-cleaning temp files in one shared code path (see [`script`]).
//!
//! # Example
//!
//! ```no_run
//! use e4pty::prelude::*;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! # async fn demo() -> e4pty::Result<()> {
//! let mut pty = openpty(
//!     WindowSize { rows: 24, cols: 80 },
//!     Script::sh("echo hello"),
//! )?;
//!
//! // Propagate a terminal resize to the child.
//! pty.writer.window_change(100, 30).await?;
//! // Feed input to the process.
//! pty.writer.write_all(b"ls\n").await?;
//!
//! // Read output until EOF (EOF is emitted once the child has exited).
//! let mut out = Vec::new();
//! pty.reader.read_to_end(&mut out).await?;
//! print!("{}", String::from_utf8_lossy(&out));
//!
//! // Collect the exit status (128 + signal if killed by a signal, Unix).
//! let code = pty.wait().await?;
//! println!("[exit: {code}]");
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

/// Everything needed to drive a pty: [`openpty`], [`Script`],
/// [`WindowSize`], the handle traits and [`Pty`].
pub mod prelude {
    /// Everything needed to drive a pty: [`openpty`], [`Script`],
    /// [`WindowSize`], the handle traits and [`Pty`].
    pub use crate::backend::openpty;
    pub use crate::pty::{
        BoxedPtyCtl, BoxedPtyReader, BoxedPtyWriter, Pty, PtyCtl, PtyReader, PtyWriter, WindowSize,
    };
    pub use crate::script::{Script, Shell};
}

mod backend;
mod error;
mod pty;
mod script;

pub use backend::openpty;
pub use error::{Error, Result};
pub use pty::{
    BoxedPtyCtl, BoxedPtyReader, BoxedPtyWriter, Pty, PtyCtl, PtyReader, PtyWriter, WindowSize,
};
pub use script::{Script, Shell};
