//! `js-agent image-pull`: pulls one image into the store the crun runtime reads.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use js_contain::oci::OciConfig;
use js_oci::{Allow, Client, Policy};

/// The first argument that selects the verb.
pub const VERB: &str = "image-pull";

/// What the verb takes from the machine it runs on.
pub struct Host<'a> {
    /// The effective user id of the process.
    pub euid: u32,
    /// The registry client for an allow-list.
    pub client: &'a dyn Fn(Vec<Allow>) -> Client,
    /// Adjusts the runtime's configuration.
    pub configure: &'a dyn Fn(&mut OciConfig),
}

fn system_client(allow: Vec<Allow>) -> Client {
    Client::new(Policy::new(allow))
}

fn system_configure(_: &mut OciConfig) {}

impl Host<'static> {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            euid: rustix::process::geteuid().as_raw(),
            client: &system_client,
            configure: &system_configure,
        }
    }
}

/// Runs the verb on `args` (the program name and the verb first).
pub fn run(
    _args: &[OsString],
    _out: &mut dyn Write,
    err: &mut dyn Write,
    _host: &Host<'_>,
) -> ExitCode {
    let _ = writeln!(err, "js-agent image-pull: not implemented");
    ExitCode::from(1)
}
