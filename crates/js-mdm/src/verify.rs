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

use js_host_step::{Account, Cmd, Host, Kind, SYSTEMCTL, ids};
use js_mdm_core::acme::Acme;
use js_mdm_core::config::MdmConfig;
use js_mdm_core::render::{self, UnitNames, render_tls, strip_comments};
use sha2::{Digest, Sha256};

use crate::install::{SS_PATH, UNITS_DIR};
use crate::{Env, MdmError};

/// How long the certificate must stay valid.
pub const VALID_FOR_DAYS: u64 = 21;
/// The secret files, all 0600 and root's.
pub const SECRET_FILES: [&str; 4] = ["nanohub.env", "nanodep.env", "scep.env", "dns.env"];
const OPENSSL: &str = "/usr/bin/openssl";

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

type Outcome = Result<String, String>;

fn text(e: impl fmt::Display) -> String {
    e.to_string()
}

/// Every health check, in the order they print.
pub fn checks(host: &dyn Host, v: &Verify<'_>) -> Vec<Check> {
    let c = v.config;
    let n = UnitNames::new(c);
    let mut rows = Vec::new();
    let mut row = |name: String, result: Outcome| rows.push(Check { name, result });
    for unit in [
        &n.scep,
        &n.nanohub,
        &n.depserver,
        &n.proxy,
        &n.cert_timer,
        &n.reload_path,
    ] {
        row(format!("{unit} is active"), active(host, unit));
    }
    let listening = host.run(&Cmd::new(SS_PATH).arg("-Hltn")).map_err(text);
    let listening = listening.and_then(|o| match o.code {
        0 => Ok(o.stdout),
        code => Err(format!("ss exited with status {code}")),
    });
    let bind = c.bind.to_string();
    for (what, port, addr) in [
        ("NanoHUB", c.nanohub_port, "127.0.0.1"),
        ("SCEP", c.scep_port, "127.0.0.1"),
        ("depserver", c.dep_port, "127.0.0.1"),
        ("the proxy", 443, bind.as_str()),
    ] {
        let only = listening.clone().and_then(|t| listens_only(&t, port, addr));
        row(format!("{what} listens on {addr}:{port} only"), only);
    }
    let none = listening.and_then(|t| match listeners(&t, 80).as_slice() {
        [] => Ok(String::new()),
        some => Err(format!("listening on {}", some.join(", "))),
    });
    row("nothing listens on port 80".to_owned(), none);
    let crt = format!("{}/lego/certificates/{}.crt", c.state, c.dns_name);
    row(
        format!("the certificate is valid for {VALID_FOR_DAYS} more days"),
        certificate(host, &crt),
    );
    row(
        format!("{} is a 0700 directory of {}", c.state, c.user),
        state_dir(host, c),
    );
    row(
        format!("{} is a 0750 directory of root", c.etc),
        mode_is(host, c.etc.as_str(), Kind::Dir, 0o750, 0),
    );
    for file in SECRET_FILES {
        let path = format!("{}/{file}", c.etc);
        row(
            format!("{path} is a 0600 file of root"),
            mode_is(host, &path, Kind::File, 0o600, 0),
        );
    }
    rows
}

fn active(host: &dyn Host, unit: &str) -> Outcome {
    let out = host
        .run(&Cmd::new(SYSTEMCTL).arg("is-active").arg(unit))
        .map_err(text)?;
    match out.code {
        0 => Ok(String::new()),
        _ => Err(format!("is {}", out.stdout.trim())),
    }
}

/// The local addresses (`addr:port`) of the listeners on `port` in `ss -Hltn` output.
fn listeners(ss: &str, port: u16) -> Vec<&str> {
    let tail = format!(":{port}");
    ss.lines()
        .filter_map(|l| l.split_whitespace().nth(3))
        .filter(|local| local.ends_with(&tail))
        .collect()
}

fn listens_only(ss: &str, port: u16, addr: &str) -> Outcome {
    let want = format!("{addr}:{port}");
    match listeners(ss, port).as_slice() {
        [one] if *one == want => Ok(String::new()),
        [] => Err("nothing listens there".to_owned()),
        some => Err(format!("listening on {}", some.join(", "))),
    }
}

fn certificate(host: &dyn Host, crt: &str) -> Outcome {
    host.stat(crt)
        .map_err(text)?
        .ok_or(format!("{crt} is not there"))?;
    let secs = VALID_FOR_DAYS * 24 * 3600;
    let cmd =
        Cmd::new(OPENSSL).args(&["x509", "-noout", "-checkend", &secs.to_string(), "-in", crt]);
    match host.run(&cmd).map_err(text)?.code {
        0 => Ok(String::new()),
        _ => Err("expires sooner, or is not a certificate".to_owned()),
    }
}

fn state_dir(host: &dyn Host, c: &MdmConfig) -> Outcome {
    let ids = ids(host, &Account::new(&c.user, &c.user)).map_err(text)?;
    let (uid, _) = ids.ok_or(format!("no user {}", c.user))?;
    mode_is(host, c.state.as_str(), Kind::Dir, 0o700, uid)
}

fn mode_is(host: &dyn Host, path: &str, kind: Kind, mode: u32, uid: u32) -> Outcome {
    let meta = host
        .stat(path)
        .map_err(text)?
        .ok_or(format!("{path} is not there"))?;
    let got = (meta.kind, meta.mode, meta.uid);
    match got == (kind, mode, uid) {
        true => Ok(String::new()),
        false => Err(format!(
            "is {:?} {:04o} uid {}, not {kind:?} {mode:04o} uid {uid}",
            got.0, got.1, got.2
        )),
    }
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

fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Compare the file at `path` with `rendered`, leaving out comment lines and, from the
/// installed text, every `ignore` text.
pub fn compare(host: &dyn Host, path: &str, rendered: &str, ignore: &[String]) -> Compared {
    match host.read(path) {
        Ok(Some(bytes)) => {
            let plain = strip_comments(&String::from_utf8_lossy(&bytes));
            let installed = sha(&ignore.iter().fold(plain, |t, i| t.replace(i.as_str(), "")));
            let rendered = sha(&strip_comments(rendered));
            match installed == rendered {
                true => Compared::Same,
                false => Compared::Differs {
                    installed,
                    rendered,
                },
            }
        }
        Ok(None) => Compared::Missing,
        Err(e) => Compared::Unreadable(e.to_string()),
    }
}

/// Every installed file compared with the render: `(path, result)` in install order.
pub fn files(host: &dyn Host, v: &Verify<'_>) -> Result<Vec<(String, Compared)>, MdmError> {
    let c = v.config;
    let rendered = render::render(c)?;
    let mut wanted: Vec<(String, String)> = Vec::new();
    for unit in &rendered.units {
        wanted.push((format!("{UNITS_DIR}/{}", unit.name), unit.text.clone()));
    }
    wanted.push((
        format!("{}/{}", c.etc, render::CADDYFILE_NAME),
        rendered.caddyfile,
    ));
    for unit in v
        .acme
        .map(|a| render_tls(c, a))
        .transpose()?
        .unwrap_or_default()
    {
        wanted.push((format!("{UNITS_DIR}/{}", unit.name), unit.text));
    }
    Ok(wanted
        .into_iter()
        .map(|(path, text)| {
            let how = compare(host, &path, &text, v.ignore);
            (path, how)
        })
        .collect())
}

/// Print every check, the file comparison and the summary; whether all passed.
pub fn run(env: &Env<'_>, v: &Verify<'_>, out: &mut dyn Write) -> Result<bool, MdmError> {
    let host = env.host;
    if host.euid() != 0 {
        writeln!(
            out,
            "NOTE: not root: the checks of root-only files fail; run it as root"
        )?;
    }
    let mut fails = 0;
    for row in checks(host, v) {
        fails += usize::from(row.result.is_err());
        writeln!(out, "{row}")?;
    }
    if v.acme.is_none() {
        writeln!(
            out,
            "skipped  the certificate job's units: the settings have no ACME keys"
        )?;
    }
    for (path, how) in files(host, v)? {
        fails += usize::from(!how.is_same());
        match how {
            Compared::Same => writeln!(out, "same       {path}")?,
            Compared::Differs {
                installed,
                rendered,
            } => writeln!(
                out,
                "differs    {path}: installed sha256 {installed}, rendered sha256 {rendered}"
            )?,
            Compared::Missing => writeln!(out, "missing    {path}")?,
            Compared::Unreadable(why) => writeln!(out, "unreadable {path}: {why}")?,
        }
    }
    match fails {
        0 => writeln!(out, "verify: all checks passed")?,
        n => writeln!(out, "verify: {n} check(s) failed")?,
    }
    Ok(fails == 0)
}
