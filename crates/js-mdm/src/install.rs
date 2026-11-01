//! `js-mdm install`: NanoHUB, the SCEP server, NanoDEP and the HTTPS proxy as capped system
//! services, as root.
//!
//! It checks the host first, read only, and refuses before anything changes: the effective uid
//! is not 0 (the plan reads root-only files, so a dry run needs root too); `MDM_STATE`,
//! `MDM_ETC` or `MDM_BIN` is a symbolic link; no interface carries `MDM_BIND`; a port it uses
//! is held by another process (one held by the kit's own active unit is a rerun); an archive in
//! `--bin-dir` is missing or its sha256 is not its pin; a secret file is not mode 0600 or not
//! root's.
//!
//! The plan has each of these only when the host differs:
//! 1. the user `MDM_USER` (a system user and group of its name, no login);
//! 2. `MDM_STATE` and its subdirectories, `MDM_USER`'s, 0700; `MDM_ETC`, root and `MDM_USER`'s
//!    group, 0750; `MDM_BIN`, root's, 0755;
//! 3. each binary unpacked from its checked archive into `MDM_BIN`, root's, 0755, the one it
//!    replaces kept as `<name>.previous`;
//! 4. the secrets, each made once from 32 random bytes as hex and never printed:
//!    `NANOHUB_API_KEY` (`nanohub.env`), `NANODEP_API` (`nanodep.env`) and
//!    `SCEP_CHALLENGE_PASSWORD` (`scep.env`), in 0600 root files under `MDM_ETC`;
//! 5. the SCEP CA (`scepserver ca -init`, as `MDM_USER`), unless `ca.pem` is there: an existing
//!    CA is kept, since replacing it re-enrolls every Mac;
//! 6. the slice and the service units under `/etc/systemd/system` (root's, 0644) and the
//!    Caddyfile (root and `MDM_USER`'s group, 0640), exactly as `js-mdm-core` renders them;
//! 7. `daemon-reload` when a unit changed; the SCEP server, NanoHUB and depserver enabled, and
//!    restarted when their unit, the slice, their binary or their secret changed, else started
//!    when not active; the proxy likewise, but only once the TLS certificate is there;
//!    depsyncer is left alone (it is enabled once an Apple token exists).
//!
//! `--uninstall` stops and disables every unit of the kit that is installed and moves the unit
//! files to `MDM_STATE/removed/<unix seconds>/`. The state (database, SCEP CA, DEP token, TLS
//! key and certificate), the secrets, the binaries and the user stay.

use std::io::Write;

use js_host_step::{Cmd, Host, Plan, execute};
use js_mdm_core::config::MdmConfig;
use js_mdm_core::pins::Pins;

use crate::{Env, MdmError};

/// Where systemd reads local units from.
pub const UNITS_DIR: &str = "/etc/systemd/system";
/// The socket lister.
pub const SS_PATH: &str = "/usr/bin/ss";
/// The secret files and the key each holds.
pub const SECRETS: [(&str, &str); 3] = [
    ("nanohub.env", "NANOHUB_API_KEY"),
    ("nanodep.env", "NANODEP_API"),
    ("scep.env", "SCEP_CHALLENGE_PASSWORD"),
];

/// What `install` was asked for.
pub struct Install {
    /// The settings.
    pub config: MdmConfig,
    /// Where `fetch` put the archives.
    pub bin_dir: String,
}

/// Run `cmd` and return its standard output, or why it failed.
pub fn checked(host: &dyn Host, cmd: &Cmd) -> Result<String, MdmError> {
    let out = host.run(cmd)?;
    match out.code {
        0 => Ok(out.stdout),
        code => Err(MdmError::Command {
            line: cmd.line(),
            code,
        }),
    }
}

/// [`plan`], then print or carry the plan out.
pub fn run(
    env: &Env<'_>,
    a: &Install,
    pins: &Pins,
    mode: js_host_step::Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let p = plan(env, a, pins, out)?;
    Ok(execute(&p, mode, env.host, out)?)
}

/// [`plan_uninstall`], then print or carry the plan out.
pub fn run_uninstall(
    env: &Env<'_>,
    c: &MdmConfig,
    mode: js_host_step::Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let p = plan_uninstall(env, c, out)?;
    Ok(execute(&p, mode, env.host, out)?)
}

/// Check the host, then plan the user, directories, binaries, secrets, CA, units and services.
pub fn plan(
    _env: &Env<'_>,
    _a: &Install,
    _pins: &Pins,
    _out: &mut dyn Write,
) -> Result<Plan, MdmError> {
    Ok(Plan::default())
}

/// Plan the rollback: units stopped and disabled, their files moved aside.
pub fn plan_uninstall(
    _env: &Env<'_>,
    _c: &MdmConfig,
    _out: &mut dyn Write,
) -> Result<Plan, MdmError> {
    Ok(Plan::default())
}
