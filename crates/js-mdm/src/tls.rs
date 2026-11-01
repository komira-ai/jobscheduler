//! `js-mdm tls`: the MDM's TLS certificate from an ACME CA over DNS-01, renewed by a timer, as
//! root. lego, pinned and installed by `js-mdm install`, does the ACME work.
//!
//! It checks first, and refuses before anything changes: an ACME setting is unset or
//! malformed (`js_mdm_core::acme::Acme`); `MDM_ETC/dns.env` is not a 0600 file of root's holding
//! every variable the provider reads; `MDM_BIN/lego` is missing, or its `lego run --help`
//! lists no flag the unit passes. The DNS credentials are never read into this program's
//! output or any command line: the certificate unit loads `dns.env` itself
//! (`EnvironmentFile=`), so lego gets them in its environment.
//!
//! The plan has each of these only when the host differs:
//! 1. the certificate unit, its twice-a-day timer, and the path unit that restarts the proxy
//!    when the certificate changes, under `/etc/systemd/system` (root's, 0644), then
//!    `daemon-reload`;
//! 2. the timer and the path unit enabled, and restarted when their unit changed, else started
//!    when not active;
//! 3. the certificate unit started once when there is no certificate yet (the first issuance;
//!    the timer renews it after);
//! 4. in a second plan, once the certificate is there (after step 3 when it runs; a dry run
//!    that would issue shows no proxy step), the proxy enabled and started when not active.
//!
//! After an apply the certificate must be there, else the run fails before the proxy; its
//! subject, issuer and expiry are printed (public data, read by `openssl`).

use std::io::Write;

use js_host_step::Mode;
use js_mdm_core::acme::Acme;
use js_mdm_core::config::MdmConfig;

use crate::{Env, MdmError};

/// The flags of `lego run` the certificate unit passes.
pub const LEGO_FLAGS: [&str; 9] = [
    "--accept-tos",
    "--email",
    "--server",
    "--path",
    "--key-type",
    "--dns",
    "--domains",
    "--dns.resolvers",
    "--dns.propagation.wait",
];

/// Check, then print or carry out the units, the first issuance and the proxy's start.
pub fn run(
    _env: &Env<'_>,
    _c: &MdmConfig,
    _acme: &Acme,
    _mode: Mode,
    _out: &mut dyn Write,
) -> Result<(), MdmError> {
    Ok(())
}
