//! `js-agent enroll`: see the real module once it lands (this is the API only).

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// The first argument that selects the verb.
pub const VERB: &str = "enroll";

/// The instance file the agent is started with (`--instance-file`).
pub const INSTANCE_FILE: &str = "/etc/js-agent/instance";

/// The token file the agent is started with (`--token-file`).
pub const TOKEN_FILE: &str = "/etc/js-agent/token";

/// The host setup script the deb installs.
pub const SETUP_SCRIPT: &str = "/usr/share/js-agent/crun_host_setup.sh";

/// The shell the setup script is run with.
pub const SHELL: &str = "/usr/bin/bash";

/// What the verb takes from the machine it runs on.
pub struct Host {
    /// The effective user id of the process.
    pub euid: u32,
    /// Whether standard input is a terminal.
    pub stdin_is_terminal: bool,
    /// Where the instance file is.
    pub instance_file: PathBuf,
    /// Where the token file is.
    pub token_file: PathBuf,
    /// The host setup script.
    pub setup_script: PathBuf,
    /// The shell the script is run with.
    pub shell: PathBuf,
}

impl Host {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            euid: rustix::process::geteuid().as_raw(),
            stdin_is_terminal: std::io::IsTerminal::is_terminal(&std::io::stdin()),
            instance_file: PathBuf::from(INSTANCE_FILE),
            token_file: PathBuf::from(TOKEN_FILE),
            setup_script: PathBuf::from(SETUP_SCRIPT),
            shell: PathBuf::from(SHELL),
        }
    }
}

/// A join token: 64 lower-case hex digits.
pub struct Token;

/// Why standard input is not a join token.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// Standard input could not be read.
    #[error("cannot read standard input: {0}")]
    Read(std::io::Error),
    /// Standard input is not a token.
    #[error("the token on standard input {0}")]
    Invalid(&'static str),
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// The token in `bytes`: 64 lower-case hex digits and at most one line break.
    ///
    /// # Errors
    ///
    /// [`TokenError::Invalid`] saying which rule failed, never what was given.
    pub fn parse(bytes: &[u8]) -> Result<Self, TokenError> {
        let _ = bytes;
        todo!("Token::parse")
    }

    /// The token at the start of `from`, reading no more than a token and its line break
    /// and one byte more.
    ///
    /// # Errors
    ///
    /// [`TokenError::Read`], or [`TokenError::Invalid`] as for [`Token::parse`].
    pub fn read(from: &mut dyn Read) -> Result<Self, TokenError> {
        let _ = from;
        todo!("Token::read")
    }

    /// The sha256 of the token as 64 lower-case hex digits.
    #[must_use]
    pub fn sha256(&self) -> String {
        todo!("Token::sha256")
    }
}

/// Runs the verb on `args` (the program name and the verb first).
pub fn run(
    args: &[OsString],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host,
) -> ExitCode {
    let _ = (args, stdin, out, err, host);
    todo!("enroll::run")
}
