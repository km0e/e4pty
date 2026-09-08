//! Platform backends. Exactly one is compiled in, selected by `cfg`.

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use unix::openpty;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::openpty;
