//! Platform backends. Exactly one is compiled in, selected by `cfg`.
//!
//! Both backends consume the same `crate::spawn::SpawnSpec`: a resolved
//! `(program, args)` pair, an optional working directory and a
//! materialized environment (`None` → inherit the parent's).

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub(crate) use unix::openpty;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::openpty;
