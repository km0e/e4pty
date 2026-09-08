//! Error type shared by all backends.

/// Errors produced by e4pty.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A std I/O error (spawn failures, pipe/terminal I/O, invalid
    /// command lines, …).
    #[error("io error: {0}")]
    IO(#[from] std::io::Error),
    /// The Unix `openpty` syscall sequence failed.
    #[cfg(not(windows))]
    #[error("openpty error: {0}")]
    Errno(#[from] rustix_openpty::rustix::io::Errno),
    /// A Win32 call of the ConPTY setup/teardown failed.
    #[cfg(windows)]
    #[error("openpty error: {0}")]
    Windows(#[from] windows::core::Error),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
