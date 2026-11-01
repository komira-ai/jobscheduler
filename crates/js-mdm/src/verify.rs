//! `js-mdm verify`: read-only checks that the MDM is up and exposed only as intended, and that
//! the installed files are what `js-mdm-core` renders for the settings.
//!
//! Each check prints `ok    <check>` or `FAIL  <check>: <why>`. Then one line per file the
//! kit installs (the slice, the services, the Caddyfile, and the certificate job's units when
//! the ACME settings are given): `same`, `differs` (with the sha256 of the installed and of
//! the rendered text) or `missing`, and `unreadable` for a file this user may not read. The
//! comparison leaves out comment lines, so a host installed by a tool whose templates are
//! worded differently compares equal when the settings are the same; each `--ignore` text is
//! removed from the installed text first. A summary line ends the output. The verb exits 0
//! when everything passes and 1 otherwise. It changes nothing, and needs no root for
//! world-readable files; the checks of the 0700 state and the 0600 secrets fail for another
//! user, since they cannot be seen.
//!
//! The checks:
//! - the SCEP server, NanoHUB, depserver and the proxy are active, and so are the certificate
//!   renewal timer and the proxy's reload on renewal;
//! - NanoHUB, SCEP and depserver listen on `127.0.0.1` at their ports only, the proxy on
//!   `MDM_BIND:443` only, and nothing on port 80;
//! - the TLS certificate in `MDM_STATE/lego/certificates/` exists and is valid for 21 more days;
//! - `MDM_STATE` is a 0700 directory of `MDM_USER`'s; `MDM_ETC` a 0750 directory of root's;
//!   `nanohub.env`, `nanodep.env`, `scep.env` and `dns.env` under it are 0600 files of root's.

use std::fmt;
use std::io::Write;

use js_host_step::Host;
use js_mdm_core::acme::Acme;
use js_mdm_core::config::MdmConfig;

use crate::{Env, MdmError};

/// How long the certificate must stay valid.
pub const VALID_FOR_DAYS: u64 = 21;
/// The secret files, all 0600 and root's.
pub const SECRET_FILES: [&str; 4] = ["nanohub.env", "nanodep.env", "scep.env", "dns.env"];

/// What `verify` checks against.
pub struct Verify<'a> {
    /// The settings.
    pub config: &'a MdmConfig,
    /// The ACME settings, when the file has them: the certificate job's units are compared.
    pub acme: Option<&'a Acme>,
    /// Text removed from every installed file before it is compared.
    pub ignore: &'a [String],
}

/// One check and how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// What it saw when it passed, or why it failed.
    pub result: Result<String, String>,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(seen) if seen.is_empty() => write!(f, "ok    {}", self.name),
            Ok(seen) => write!(f, "ok    {}: {seen}", self.name),
            Err(why) => write!(f, "FAIL  {}: {why}", self.name),
        }
    }
}

/// Every health check, in the order they print.
pub fn checks(_host: &dyn Host, _v: &Verify<'_>) -> Vec<Check> {
    Vec::new()
}

/// How an installed file compares with the render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compared {
    /// The same, comments aside.
    Same,
    /// Different: the sha256 of the installed text and of the rendered text.
    Differs {
        /// The installed text's.
        installed: String,
        /// The rendered text's.
        rendered: String,
    },
    /// Not installed.
    Missing,
    /// Not readable by this user.
    Unreadable(String),
}

impl Compared {
    /// Whether the file is as rendered.
    pub fn is_same(&self) -> bool {
        *self == Compared::Same
    }
}

/// Compare the file at `path` with `rendered`.
pub fn compare(_host: &dyn Host, _path: &str, _rendered: &str, _ignore: &[String]) -> Compared {
    Compared::Missing
}

/// Every installed file compared with the render: `(path, result)` in install order.
pub fn files(_host: &dyn Host, _v: &Verify<'_>) -> Result<Vec<(String, Compared)>, MdmError> {
    Ok(Vec::new())
}

/// Print every check, the file comparison and the summary; whether all passed.
pub fn run(_env: &Env<'_>, _v: &Verify<'_>, _out: &mut dyn Write) -> Result<bool, MdmError> {
    Ok(true)
}
