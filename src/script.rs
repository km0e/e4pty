//! What to run inside the pty: [`Script`] descriptions, the [`Shell`]
//! selector, and the single shared resolution step ([`Script::materialize`])
//! that both backends consume.
//!
//! Shell-script execution works by writing the source into a temp file,
//! prefixed with a self-cleanup hook, and invoking the interpreter on it.
//! The hook is written *before* the user source so it is installed even
//! when the script calls `exit` early; the interpreter removes the file
//! itself once it stops.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::Write;

use tempfile::NamedTempFile;

/// Interpreter used to run [`Script::Shell`] payloads.
#[derive(Debug, Clone, PartialEq, Eq, strum::EnumString, serde::Deserialize, strum::AsRefStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// POSIX `sh`, looked up on `PATH`.
    #[strum(serialize = "sh")]
    Sh,
    /// `bash`, looked up on `PATH`.
    #[strum(serialize = "bash")]
    Bash,
    /// Windows PowerShell, invoked with `-File <script>`.
    #[strum(serialize = "powershell")]
    Powershell,
}

impl Display for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

impl Shell {
    /// Interpreter binary name to look up on `PATH`.
    fn binary(&self) -> &'static str {
        match self {
            Shell::Sh => "sh",
            Shell::Bash => "bash",
            Shell::Powershell => "powershell",
        }
    }

    /// Interpreter-specific self-cleanup hook, prepended to the user
    /// source (see the module docs for why it comes first).
    fn cleanup_hook(&self) -> &'static str {
        match self {
            // sh/bash: an EXIT trap deletes the script file ($0).
            Shell::Sh | Shell::Bash => "\ntrap 'rm -f -- \"$0\"' EXIT;\n",
            // PowerShell parses the whole file before running, so deleting
            // itself on the first line is safe and fires on early `exit`.
            Shell::Powershell => "\r\nRemove-Item $MyInvocation.MyCommand.Path\r\n",
        }
    }
}

/// What to spawn inside the pty.
///
/// All variants are exec'd directly — no shell is ever involved unless
/// you ask for one via [`Script::sh`] and friends.
#[derive(Debug, Clone)]
pub enum Script {
    /// A full command line string, tokenized on whitespace and exec'd
    /// directly. Convenient for quick commands; quoting is *not*
    /// interpreted — use [`Script::Exec`] when arguments may contain
    /// whitespace.
    Line(String),
    /// Explicit argv: `program` plus verbatim arguments.
    Exec {
        /// Executable to launch (looked up on `PATH` by the OS).
        program: OsString,
        /// Arguments passed verbatim.
        args: Vec<OsString>,
    },
    /// Script source executed through a [`Shell`] via a self-cleaning
    /// temp file.
    Shell {
        /// Interpreter to run the source with.
        shell: Shell,
        /// Script source code.
        source: String,
    },
}

impl Script {
    /// Tokenize `line` on whitespace and exec it directly.
    pub fn line(line: impl Into<String>) -> Self {
        Script::Line(line.into())
    }

    /// Exec `program` with verbatim `args` (no shell, no re-tokenization).
    pub fn exec<P, I>(program: P, args: I) -> Self
    where
        P: Into<OsString>,
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        Script::Exec {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Run `source` through POSIX `sh` via a self-cleaning temp file.
    pub fn sh(source: impl Into<String>) -> Self {
        Script::Shell {
            shell: Shell::Sh,
            source: source.into(),
        }
    }

    /// Run `source` through `bash` via a self-cleaning temp file.
    pub fn bash(source: impl Into<String>) -> Self {
        Script::Shell {
            shell: Shell::Bash,
            source: source.into(),
        }
    }

    /// Run `source` through PowerShell via a self-cleaning `.ps1` temp file.
    pub fn powershell(source: impl Into<String>) -> Self {
        Script::Shell {
            shell: Shell::Powershell,
            source: source.into(),
        }
    }

    /// Resolve the script into a flat `(program, args)` pair.
    ///
    /// This is the single place where shell scripts are materialized to
    /// temp files, so both platform backends see the exact same behavior.
    /// The temp file is intentionally *kept* (the cleanup hook deletes it
    /// after the interpreter ran) and deliberately orphaned when spawning
    /// fails.
    pub(crate) fn materialize(self) -> std::io::Result<Resolved> {
        match self {
            Script::Line(line) => {
                let mut it = line.split_whitespace();
                let program = it
                    .next()
                    .ok_or_else(|| std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "empty command line",
                    ))?;
                Ok(Resolved {
                    program: program.into(),
                    args: it.map(Into::into).collect(),
                })
            }
            Script::Exec { program, args } => Ok(Resolved { program, args }),
            Script::Shell { shell, source } => {
                let mut temp = match shell {
                    Shell::Sh | Shell::Bash => NamedTempFile::new(),
                    Shell::Powershell => NamedTempFile::with_suffix(".ps1"),
                }?;
                // Cleanup hook FIRST, user source after: the hook must be
                // installed before the script can possibly `exit`.
                temp.write_all(shell.cleanup_hook().as_bytes())?;
                temp.write_all(source.as_bytes())?;
                let path = temp.into_temp_path().keep()?;
                let args = match shell {
                    Shell::Powershell => vec![OsString::from("-File"), path.into_os_string()],
                    Shell::Sh | Shell::Bash => vec![path.into_os_string()],
                };
                Ok(Resolved {
                    program: OsString::from(shell.binary()),
                    args,
                })
            }
        }
    }
}

impl From<&str> for Script {
    fn from(line: &str) -> Self {
        Script::Line(line.to_owned())
    }
}

impl From<String> for Script {
    fn from(line: String) -> Self {
        Script::Line(line)
    }
}

impl From<&[&str]> for Script {
    fn from(argv: &[&str]) -> Self {
        match argv.split_first() {
            None => Script::Line(String::new()), // materialize() reports this
            Some((program, args)) => Script::exec(*program, args),
        }
    }
}

/// A resolved, ready-to-spawn command; the common currency shared by the
/// platform backends after [`Script::materialize`].
#[derive(Debug)]
pub(crate) struct Resolved {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}
