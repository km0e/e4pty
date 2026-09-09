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
//! # Lifecycle contract
//!
//! A pty session exposes two independent completion signals, and neither
//! implies the other:
//!
//! - **reader EOF** — emitted when nothing holds the pty's *slave* side
//!   open anymore (or the session was hung up). It does *not* mean "the
//!   child exited": a child that closes its own stdio and keeps running
//!   (daemonizers, `ssh -f`) yields EOF while still alive, and
//!   descendants that inherit the slave can delay EOF long after the
//!   spawned child exited. On Windows/ConPTY, EOF instead tracks the end
//!   of the console *session* (all clients exited, or the HPCON was
//!   closed) rather than the child's stdio handles.
//! - **[`PtyCtl::wait`]** — resolves when the *spawned child process*
//!   was reaped. The exit code exists only here. It may complete before
//!   the reader's EOF (descendants keep the slave open) or after it (the
//!   child closed its stdio early).
//!
//! Safe teardown order for batch use: **drain the reader to EOF first,
//! then `wait`** — never `wait` without consuming output, because a
//! child blocked writing to a full tty output buffer never exits
//! (deadlock). [`Pty::finish`] implements the safe order and returns
//! `(output, exit_code)`.
//!
//! For interactive use, split the handles ([`Pty::split`]) and run the
//! reader in its own task; `wait` may be awaited at spawn time or later,//! whichever suits the consumer — the reader's EOF does not depend on it.
//!
//! Dropping the handles tears the session down:
//!
//! | handle drop | Unix | Windows |
//! |---|---|---|
//! | `ctl` | no effect on the child (tokio reaps it; no zombie) | closes the ConPTY — attached clients are signalled and exit |
//! | `reader` + `writer` | closes the master → `SIGHUP` to the foreground process group | writer drop delivers stdin EOF; the console session stays up until `ctl` drop |
//!
//! To end a session explicitly, use [`PtyCtl::kill`] (`SIGKILL` /
//! `TerminateProcess`) followed by `wait`. To end the *input* only, send
//! `0x04` (canonical-mode EOT) — a tty master cannot be half-closed on
//! Unix, so [`PtyWriter::eof`] is a no-op there (on Windows it closes
//! the ConPTY input pipe, which the child observes as stdin EOF).
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
//! // Read output until EOF (see the lifecycle contract below: EOF fires
//! // when nothing holds the pty's slave side open — for a child that
//! // exits, that is right after its exit).
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
    /// Everything needed to drive a pty: [`openpty`], [`PtyBuilder`],
    /// [`Script`], [`WindowSize`], the handle traits and [`Pty`].
    pub use crate::pty::{
        BoxedPtyCtl, BoxedPtyReader, BoxedPtyWriter, Pty, PtyCtl, PtyReader, PtyWriter, WindowSize,
    };
    pub use crate::script::{Script, Shell};
    pub use crate::spawn::{PtyBuilder, openpty};
}

mod backend;
mod error;
mod pty;
mod script;
mod spawn;

pub use error::{Error, Result};
pub use pty::{
    BoxedPtyCtl, BoxedPtyReader, BoxedPtyWriter, Pty, PtyCtl, PtyReader, PtyWriter, WindowSize,
};
pub use script::{Script, Shell};
pub use spawn::{PtyBuilder, openpty};
