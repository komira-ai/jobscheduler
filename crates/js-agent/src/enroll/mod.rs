//! `js-agent enroll`: every one-time step that makes a host a member of a pool, in one verb
//! run as root on the host.
//!
//! ```text
//! js lease join-token mint --provider P --instance I | js-agent enroll --provider P --instance I [--apply]
//! ```
//!
//! The verb does three things, in this order, and checks each before it goes on:
//!
//! 1. **The host setup.** It runs the script the deb installs,
//!    `/usr/share/js-agent/crun_host_setup.sh` (crun, the job user, subordinate ids, the
//!    scratch quota, the directories, AppArmor, the unit), with the flags below. The script is
//!    reviewed and pinned; the verb reads its exit status and does not repeat its work. A
//!    script that is missing, is not a regular file, is not root's or is writable by group or
//!    others is not run.
//! 2. **The instance file**, `/etc/js-agent/instance`: the line `PROVIDER:INSTANCE\n`, owned by
//!    root, mode 0644, which the agent reads with `--instance-file` and names in its hello.
//! 3. **The token file**, `/etc/js-agent/token`: the join token from standard input and a line
//!    break, owned by root, mode 0600, which the agent reads with `--token-file`.
//!
//! The token is read from standard input and nowhere else: a terminal is refused, and no flag
//! takes it, so it is on no command line. It must be what the server mints (64 lower-case hex
//! digits, one line). It is printed nowhere; the verb prints its sha256, which names it for
//! the operator and the server's log without being usable as it.
//!
//! # Dry run and apply
//!
//! Without `--apply` nothing is changed. The script is run without `--apply`, which is its
//! own read-only dry run, so the refusals an apply would meet are met here; the two files
//! are only looked at. With `--apply` every refusal comes before the first change: the root
//! check, the token, the ids, the script, and the two files that are already there. Then the
//! setup is applied, then the instance file is written, then the token file, and each is read
//! back. A file is made whole in a staged file beside it and linked to its name (js_client's
//! `create_new`), so a crash leaves nothing but the staged file, and nothing is ever
//! replaced.
//!
//! # Files that are already there
//!
//! A file that is there is left alone if it is what the verb would write: the instance file
//! must hold exactly the line, the token file must hold the same token (compared by sha256).
//! Any other file is refused, with exit 2, before the setup runs: `remove it by hand and run
//! again`. So is a link, a file that is not a regular file of root's, a mode the kind does
//! not allow (the token file must be 0600 or 0400; the instance file must not be writable
//! by group or others), and one longer than 4096 bytes. A second run on an enrolled host
//! changes nothing and says so. It needs the token again, to compare.
//!
//! # Flags
//!
//! | flag | meaning |
//! |---|---|
//! | `--provider <id>`, `--instance <id>` | required: the host's identity, which the join token is bound to. Both are ids the server accepts (`[A-Za-z0-9._-]`, at most 128 bytes, leading letter or digit) |
//! | `--apply` | do it; without it the run is a dry run |
//! | `--runtime crun` | the runtime to set the host up for; `crun` is the only one (the default) |
//! | `--server <url>`, `--pool <name>` | written into the unit by the setup script, which wants both or neither for an apply. `--pool` defaults to the provider: the server mints a token only for the pool's own name |
//! | `--job-user`, `--subid-start`, `--unit`, `--unit-sha256`, `--tmpfiles`, `--tmpfiles-sha256`, `--scratch-mount-unit`, `--dependents-limit`, `--skip-scratch-quota` | passed to the setup script as they are; see docs/runbooks/crun-host-setup.md |
//!
//! # Output and exit status
//!
//! Standard output has one line per step and a last line for the host; the setup script's own
//! words go to standard error.
//!
//! ```text
//! enroll: setup status=done|nothing-to-do|planned
//! enroll: instance status=written|present|planned path=/etc/js-agent/instance
//! enroll: token status=written|present|planned path=/etc/js-agent/token sha256=<hex>
//! enroll: status=enrolled|already-enrolled|planned provider=<id> instance=<id>
//! ```
//!
//! | status | meaning |
//! |---|---|
//! | 0 | enrolled; or nothing to do (`already-enrolled`); or, without `--apply`, a plan (`planned`) |
//! | 1 | a step failed after the checks: the setup script failed (its manifest says what it did), or a file could not be written. What was done stays; a rerun carries on |
//! | 2 | refused, nothing changed: not root, a terminal on standard input, a token or ids that are not valid, a script that is missing or unfit, a file that is there and differs, or the setup script refused (its exit 2); also what clap refuses |
//!
//! The verb reads no environment variable. It runs the script by `/usr/bin/bash`, with an empty
//! environment but `PATH`, and with no standard input.

mod files;
mod setup;
mod token;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use js_agent_wire::{InstanceIdentity, ValidationError};

use crate::exit;
use files::Kind;
pub use token::{Token, TokenError};

/// The first argument that selects the verb.
pub const VERB: &str = "enroll";

/// The instance file the agent is started with (`--instance-file`).
pub const INSTANCE_FILE: &str = "/etc/js-agent/instance";

/// The token file the agent is started with (`--token-file`).
pub const TOKEN_FILE: &str = "/etc/js-agent/token";

/// The host setup script the deb installs.
pub const SETUP_SCRIPT: &str = "/usr/share/js-agent/crun_host_setup.sh";

/// The shell the setup script is run with, by its absolute path.
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

/// The runtime the host is set up for.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Runtime {
    /// `crun`, run as an unprivileged job user (`crun_host_setup.sh`).
    Crun,
}

/// The flags of the verb.
#[derive(Parser)]
#[command(
    name = "js-agent enroll",
    about = "Sets a host up to join a pool: the host setup, the instance file and the join token",
    long_about = "Sets a host up to join a pool, as root: runs the packaged host setup, writes \
        /etc/js-agent/instance, and writes the join token read from standard input to \
        /etc/js-agent/token (mode 0600). Without --apply it changes nothing. A file that is \
        already there is left alone if it is what would be written and refused if not.\n\n\
        The token is read from standard input only, for example:\n  \
        js lease join-token mint --provider P --instance I | js-agent enroll --provider P --instance I --apply\n\n\
        Exit status: 0 enrolled, nothing to do, or a plan; 1 a step failed; 2 refused, \
        nothing changed.",
    disable_version_flag = true
)]
struct Args {
    /// The provider of the host: the name of the pool it joins.
    #[arg(long, value_name = "ID")]
    provider: String,
    /// The instance the join token is bound to: the id of this host.
    #[arg(long, value_name = "ID")]
    instance: String,
    /// The runtime to set the host up for.
    #[arg(long, value_enum, default_value_t = Runtime::Crun)]
    runtime: Runtime,
    /// Change the host. Without it the run only shows what it would do.
    #[arg(long)]
    apply: bool,
    /// The lease server, written into the unit (the setup script needs it to apply).
    #[arg(long, value_name = "URL")]
    server: Option<String>,
    /// The pool, written into the unit with --server (the provider when absent).
    #[arg(long, value_name = "NAME")]
    pool: Option<String>,
    /// The job user (the setup script's default is jsjob).
    #[arg(long, value_name = "NAME")]
    job_user: Option<String>,
    /// The first id of the subordinate id block (the first free block when absent).
    #[arg(long, value_name = "N")]
    subid_start: Option<u64>,
    /// js-agent-crun.service of the release (an apply needs it with --unit-sha256).
    #[arg(long, value_name = "FILE")]
    unit: Option<PathBuf>,
    /// The sha256 of --unit.
    #[arg(long, value_name = "HEX")]
    unit_sha256: Option<String>,
    /// tmpfiles-crun.conf of the release (an apply needs it with --tmpfiles-sha256).
    #[arg(long, value_name = "FILE")]
    tmpfiles: Option<PathBuf>,
    /// The sha256 of --tmpfiles.
    #[arg(long, value_name = "HEX")]
    tmpfiles_sha256: Option<String>,
    /// The mount unit of the scratch directory.
    #[arg(long, value_name = "NAME")]
    scratch_mount_unit: Option<String>,
    /// How many units the walk of the mount's dependents may look at.
    #[arg(long, value_name = "N")]
    dependents_limit: Option<u64>,
    /// Leave the scratch mount as it is.
    #[arg(long)]
    skip_scratch_quota: bool,
}

impl Args {
    /// The flags of the setup script: `--apply` when applying, then each setup flag that
    /// was given, in the script's spelling. `--pool` goes with `--server`.
    fn setup_flags(&self) -> Vec<OsString> {
        let pool = self.pool.as_ref().unwrap_or(&self.provider);
        let valued: [(&str, Option<OsString>); 10] = [
            ("--job-user", self.job_user.as_ref().map(OsString::from)),
            (
                "--subid-start",
                self.subid_start.map(|n| n.to_string().into()),
            ),
            ("--unit", self.unit.as_ref().map(|path| path.into())),
            (
                "--unit-sha256",
                self.unit_sha256.as_ref().map(OsString::from),
            ),
            ("--tmpfiles", self.tmpfiles.as_ref().map(|path| path.into())),
            (
                "--tmpfiles-sha256",
                self.tmpfiles_sha256.as_ref().map(OsString::from),
            ),
            ("--server", self.server.as_ref().map(OsString::from)),
            ("--pool", self.server.as_ref().map(|_| OsString::from(pool))),
            (
                "--scratch-mount-unit",
                self.scratch_mount_unit.as_ref().map(OsString::from),
            ),
            (
                "--dependents-limit",
                self.dependents_limit.map(|n| n.to_string().into()),
            ),
        ];
        self.apply
            .then_some(OsString::from("--apply"))
            .into_iter()
            .chain(
                valued
                    .into_iter()
                    .filter_map(|(flag, value)| value.map(|v| [OsString::from(flag), v]))
                    .flatten(),
            )
            .chain(
                self.skip_scratch_quota
                    .then_some(OsString::from("--skip-scratch-quota")),
            )
            .collect()
    }
}

/// Why the verb stopped.
enum Failure {
    /// A flag, the token or a file is wrong, or the user may not run the verb; nothing was
    /// changed (the setup script's own refusal is this too).
    Usage(String),
    /// A step failed after the checks.
    Failed(String),
}

impl Failure {
    fn code(&self) -> u8 {
        match self {
            Self::Usage(_) => exit::USAGE,
            Self::Failed(_) => exit::FAILED,
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Usage(text) | Self::Failed(text) => text,
        }
    }
}

/// A refusal with these words.
fn usage(why: &str) -> Failure {
    Failure::Usage(why.to_owned())
}

/// The refusal for a standard input that is not a token.
fn bad_token(why: TokenError) -> Failure {
    Failure::Usage(why.to_string())
}

/// The refusal for ids the server would not admit.
fn bad_ids(why: ValidationError) -> Failure {
    Failure::Usage(format!(
        "--provider and --instance must make an instance id the server accepts: {why}"
    ))
}

/// Runs the verb on `args` (the program name and the verb first). The report goes to `out`,
/// the setup script's words and the refusals to `err`.
pub fn run(
    args: &[OsString],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host,
) -> ExitCode {
    match enroll(args, stdin, err, host) {
        Ok(text) => {
            let _ = write!(out, "{text}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            let _ = writeln!(err, "js-agent enroll: {}", failure.text());
            ExitCode::from(failure.code())
        }
    }
}

/// What the verb prints on success: the help text, or the report.
fn enroll(
    args: &[OsString],
    stdin: &mut dyn Read,
    err: &mut dyn Write,
    host: &Host,
) -> Result<String, Failure> {
    let args = match Args::try_parse_from(args.iter().skip(1)) {
        Ok(args) => args,
        Err(e) if e.use_stderr() => return Err(Failure::Usage(e.to_string())),
        Err(help) => return Ok(help.to_string()),
    };
    let Runtime::Crun = args.runtime;
    // Every refusal comes before the first change.
    if host.euid != 0 {
        return Err(usage(
            "must run as root: it runs the host setup and writes /etc/js-agent",
        ));
    }
    if host.stdin_is_terminal {
        return Err(usage(
            "standard input is a terminal; pipe the join token in: \
             js lease join-token mint --provider P --instance I | js-agent enroll ...",
        ));
    }
    let token = Token::read(stdin).map_err(bad_token)?;
    InstanceIdentity {
        provider: args.provider.clone(),
        instance_id: args.instance.clone(),
    }
    .validate()
    .map_err(bad_ids)?;
    setup::fit(&host.setup_script)?;
    let line = format!("{}:{}\n", args.provider, args.instance);
    let have_instance = files::inspect(&host.instance_file, Kind::Instance, line.as_bytes())?;
    let token_line = token.line();
    let have_token = files::inspect(&host.token_file, Kind::Token, token_line.as_bytes())?;

    let apply = args.apply;
    let setup = setup::run(
        &host.shell,
        &host.setup_script,
        &args.setup_flags(),
        apply,
        err,
    )?;
    if apply && !have_instance {
        files::write_new(&host.instance_file, Kind::Instance, line.as_bytes())?;
    }
    if apply && !have_token {
        files::write_new(&host.token_file, Kind::Token, token_line.as_bytes())?;
    }

    let word = |present: bool| match (present, apply) {
        (true, _) => "present",
        (false, true) => "written",
        (false, false) => "planned",
    };
    let pending = [setup != "nothing-to-do", !have_instance, !have_token]
        .into_iter()
        .any(|pending| pending);
    let status = match (pending, apply) {
        (false, _) => "already-enrolled",
        (true, true) => "enrolled",
        (true, false) => "planned",
    };
    Ok(format!(
        "enroll: setup status={setup}\n\
         enroll: instance status={} path={}\n\
         enroll: token status={} path={} sha256={}\n\
         enroll: status={status} provider={} instance={}\n",
        word(have_instance),
        host.instance_file.display(),
        word(have_token),
        host.token_file.display(),
        token.sha256(),
        args.provider,
        args.instance,
    ))
}
