//! Running commands, with or without a profile, and building the profile.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use crate::report::Probe;

/// What a finished command did.
pub struct Outcome {
    /// Exit code, if it exited.
    pub code: Option<i32>,
    /// Exited 0.
    pub ok: bool,
    /// Standard output.
    pub stdout: String,
    /// Standard error (a spawn failure is reported here).
    pub stderr: String,
}

impl Outcome {
    /// The child modes exit 1 when the action was refused or failed.
    pub fn refused(&self) -> bool {
        self.code == Some(1)
    }

    /// The last part of stderr, one line, for a report.
    pub fn stderr_tail(&self) -> String {
        let text = self.stderr.trim();
        let start = text.char_indices().rev().nth(300).map_or(0, |(i, _)| i);
        text[start..].replace('\n', " | ")
    }
}

/// Runs `cmd` to completion.
pub fn run(mut cmd: Command) -> Outcome {
    match cmd.output() {
        Ok(out) => Outcome {
            code: out.status.code(),
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(err) => Outcome {
            code: None,
            ok: false,
            stdout: String::new(),
            stderr: format!("cannot start: {err}"),
        },
    }
}

/// Adds `prefix`status, and `prefix`stderr for a command that did not succeed.
pub fn describe(probe: Probe, prefix: &str, outcome: &Outcome) -> Probe {
    let probe = probe.fact(
        &format!("{prefix}exit"),
        outcome.code.map_or("signal".to_owned(), |c| c.to_string()),
    );
    if outcome.ok {
        probe
    } else {
        probe.fact(&format!("{prefix}stderr"), outcome.stderr_tail())
    }
}

/// The first line of a command's stdout, or why there is none.
pub fn first_line(program: &str, args: &[&str]) -> String {
    let outcome = run({
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    });
    let text = if outcome.ok {
        &outcome.stdout
    } else {
        &outcome.stderr
    };
    text.lines().next().unwrap_or("").to_owned()
}

/// How the network is cut in a profile.
pub enum Net {
    /// Everything denied; no loopback and no class-wide unix-socket allow.
    Off,
    /// `Off` plus `localhost:*`.
    Loopback,
    /// `Off` plus a loopback port range (`localhost:lo-hi`).
    Range(u16, u16),
    /// `Off` plus single loopback ports (`localhost:port`).
    Ports(Vec<u16>),
}

/// The places a profile is built around; all canonical.
pub struct Places {
    /// The probe binary, started as the child.
    pub exe: PathBuf,
    /// The run directory: writable.
    pub run: PathBuf,
    /// Read-only inputs, under the run directory: denied after the grant.
    pub input: PathBuf,
    /// The user's `/var/folders/<xx>/<hash>` entry: writable.
    pub uid_entry: PathBuf,
}

/// The profile text, built the design's way: allow default, deny write, narrow allows,
/// input protection, network, optional extra rules, the floor last. Paths are
/// `-D` parameters (`RUN`, `UIDENT`, `IN`), never interpolated.
pub fn profile(net: &Net, extra: &str) -> String {
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        "(deny job-creation)".to_owned(),
        "(deny lsopen)".to_owned(),
        "(deny user-preference-write)".to_owned(),
        "(allow file-write* (subpath (param \"RUN\")))".to_owned(),
        "(allow file-write* (subpath (param \"UIDENT\")))".to_owned(),
        "(allow file-write* (subpath \"/dev\"))".to_owned(),
        "(deny file-write* (subpath (param \"IN\")))".to_owned(),
        "(deny network*)".to_owned(),
    ];
    match net {
        Net::Off => {}
        Net::Loopback => lines.push("(allow network* (remote ip \"localhost:*\"))".to_owned()),
        Net::Ports(ports) => {
            for port in ports {
                lines.push(format!("(allow network* (remote ip \"localhost:{port}\"))"));
            }
        }
        Net::Range(lo, hi) => {
            lines.push(format!(
                "(allow network* (remote ip \"localhost:{lo}-{hi}\"))"
            ));
        }
    }
    if !extra.is_empty() {
        lines.push(extra.to_owned());
    }
    // The floor, last.
    for floor in [
        "(deny mach-register)",
        "(deny file-mount)",
        "(deny job-creation)",
        "(deny lsopen)",
    ] {
        lines.push(floor.to_owned());
    }
    lines.join("\n")
}

/// With a profile, or the no-sandbox control.
pub enum Mode<'a> {
    /// No `sandbox-exec`.
    Control,
    /// Under the profile for this network level; the text is added before the floor.
    Sandboxed(&'a Net, &'a str),
}

impl Places {
    /// `program args`, under `sandbox-exec -D ... -p <profile>` or directly.
    pub fn command(&self, mode: &Mode<'_>, program: &OsStr, args: &[&OsStr]) -> Command {
        let mut cmd = match mode {
            Mode::Control => Command::new(program),
            Mode::Sandboxed(net, extra) => {
                let mut cmd = Command::new("/usr/bin/sandbox-exec");
                for (name, path) in [
                    ("RUN", &self.run),
                    ("UIDENT", &self.uid_entry),
                    ("IN", &self.input),
                ] {
                    cmd.arg("-D").arg(format!("{name}={}", path.display()));
                }
                cmd.arg("-p").arg(profile(net, extra)).arg(program);
                cmd
            }
        };
        cmd.args(args);
        cmd
    }

    /// Runs this binary's `child <name> <arg>` mode.
    pub fn child(&self, mode: &Mode<'_>, name: &str, arg: &OsStr) -> Outcome {
        let args = [OsStr::new("child"), OsStr::new(name), arg];
        run(self.command(mode, self.exe.as_os_str(), &args))
    }
}
