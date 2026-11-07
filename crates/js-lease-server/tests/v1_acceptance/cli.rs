//! The command line client: `js_cli::run_with` with the program's own services
//! (`Services::system()`, which `js`'s `main` passes), so a call dials the job port over
//! gRPC on loopback exactly as the `js` binary does. The server and token are flags.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// What one command printed, and its exit status.
#[derive(Debug)]
pub struct Ran {
    /// The exit status.
    pub status: u8,
    /// Standard output.
    pub out: String,
    /// Standard error.
    pub err: String,
}

impl Ran {
    /// Standard output as JSON (`--json`), which must have exited 0.
    pub fn json(&self) -> Value {
        assert_eq!(self.status, 0, "js failed: {}", self.err);
        serde_json::from_str(&self.out).unwrap_or_else(|e| panic!("{e}: {self:?}"))
    }
}

/// `js --server http://JOB --token-file TOKEN --json ...` against one server.
pub struct Js {
    server: String,
    token: PathBuf,
}

impl Js {
    /// The client of the job port `job`, with the token in `token`.
    pub fn new(job: SocketAddr, token: &Path) -> Self {
        Self {
            server: format!("http://{job}"),
            token: token.to_owned(),
        }
    }

    /// Runs `js` with `words` after the global flags.
    pub fn run(&self, words: &[&str]) -> Ran {
        let mut args: Vec<OsString> = ["js", "--server", &self.server, "--json", "--token-file"]
            .map(OsString::from)
            .into();
        args.push(self.token.clone().into_os_string());
        args.extend(words.iter().map(OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = js_cli::run_with(args, &mut out, &mut err, js_cli::Services::system());
        Ran {
            status,
            out: String::from_utf8_lossy(&out).into_owned(),
            err: String::from_utf8_lossy(&err).into_owned(),
        }
    }
}
