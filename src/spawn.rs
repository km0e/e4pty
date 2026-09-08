//! Spawn configuration: [`PtyBuilder`] glues together window size, script,
//! working directory and environment, and is the single entry point both
//! platform backends consume (via the internal [`SpawnSpec`]).
//!
//! The environment follows `std::process::Command` semantics: by default
//! the parent environment is inherited untouched; `env`/`envs` add or
//! override individual variables, `env_remove` removes them, and
//! `env_clear` starts from an empty environment (subsequent `env` calls
//! then build it up).

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Result;
use crate::pty::{Pty, WindowSize};
use crate::script::Script;

/// Everything a backend needs to spawn one pty session.
#[derive(Debug, Clone)]
pub(crate) struct SpawnSpec {
    pub(crate) window_size: WindowSize,
    pub(crate) script: Script,
    pub(crate) current_dir: Option<PathBuf>,
    /// Materialized environment, `None` → inherit the parent environment
    /// untouched (fast path: no env handling at all).
    pub(crate) env: Option<Vec<(OsString, OsString)>>,
}

/// Modifications applied on top of the inherited environment.
#[derive(Debug, Clone, Default)]
struct EnvMod {
    clear: bool,
    set: Vec<(OsString, OsString)>,
    remove: Vec<OsString>,
}

impl EnvMod {
    fn is_noop(&self) -> bool {
        !self.clear && self.set.is_empty() && self.remove.is_empty()
    }

    /// Collapse the modifications into one full environment, or `None`
    /// when the parent environment should be inherited as-is.
    fn materialize(&self) -> Option<Vec<(OsString, OsString)>> {
        if self.is_noop() {
            return None;
        }
        let mut vars: Vec<(OsString, OsString)> = if self.clear {
            Vec::new()
        } else {
            std::env::vars_os().collect()
        };
        for key in &self.remove {
            if let Some(idx) = vars.iter().position(|(n, _)| n == key) {
                vars.swap_remove(idx);
            }
        }
        for (key, value) in &self.set {
            match vars.iter_mut().find(|(n, _)| n == key) {
                Some(slot) => slot.1 = value.clone(),
                None => vars.push((key.clone(), value.clone())),
            }
        }
        Some(vars)
    }
}

/// Builder for a pty session: window size, script, working directory and
/// environment.
///
/// `spawn()` consumes the builder and is the only fallible step; the rest
/// is plain configuration. `openpty(window_size, script)` is exactly
/// `PtyBuilder::new(window_size, script).spawn()` without cwd/env tweaks.
///
/// ```no_run
/// use e4pty::prelude::*;
///
/// # fn demo() -> e4pty::Result<()> {
/// let mut pty = PtyBuilder::new(WindowSize::default(), Script::sh("ls -la"))
///     .current_dir("/tmp")
///     .env("TERM", "xterm-256color")
///     .env_remove("GIT_DIR")
///     .spawn()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct PtyBuilder {
    window_size: WindowSize,
    script: Script,
    current_dir: Option<PathBuf>,
    env: EnvMod,
}

impl PtyBuilder {
    /// Configure a new pty session for `script` at `window_size`.
    pub fn new(window_size: WindowSize, script: Script) -> Self {
        Self {
            window_size,
            script,
            current_dir: None,
            env: EnvMod::default(),
        }
    }

    /// Set the working directory of the spawned process.
    /// Without this call the process inherits the parent's cwd.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Add or override one environment variable of the spawned process.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.set.push((key.into(), value.into()));
        self
    }

    /// Add or override several environment variables.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env
            .set
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Remove an environment variable the process would otherwise inherit.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env.remove.push(key.into());
        self
    }

    /// Start from an empty environment instead of the parent's one
    /// (subsequent `env`/`envs` calls then build it up; be aware most
    /// programs expect at least `PATH`).
    pub fn env_clear(mut self) -> Self {
        self.env.clear = true;
        self
    }

    /// Spawn the configured session.
    pub fn spawn(self) -> Result<Pty> {
        let spec = SpawnSpec {
            window_size: self.window_size,
            script: self.script,
            current_dir: self.current_dir,
            env: self.env.materialize(),
        };
        crate::backend::openpty(spec)
    }
}

/// Spawn `script` attached to a fresh pseudo-terminal.
///
/// Shortcut for `PtyBuilder::new(window_size, script).spawn()` — the
/// process inherits the parent's working directory and environment.
pub fn openpty(window_size: WindowSize, script: Script) -> Result<Pty> {
    PtyBuilder::new(window_size, script).spawn()
}
