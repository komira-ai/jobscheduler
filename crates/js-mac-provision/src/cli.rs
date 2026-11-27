//! The program's verbs. [`execute`] runs one over a [`Pfctl`] and a writer, so the whole
//! of it is tested against a fake; [`main_with`] is the thin shell that builds the real
//! runner and the exit code.

use std::io::{self, Write};
use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::agent_flags::{self, AgentPf, FlagsError};
use crate::daemon::{Daemon, pf_pool_plist};
use crate::launchd::render_plist;
use crate::pfctl::{Pfctl, SYSTEM_PFCTL, SystemPfctl};
use crate::policy::{AnchorName, Endpoint, PolicyError, UidRange};
use crate::probe::{Probe, probe};
use crate::provision::{ProvisionError, Provisioner};
use crate::render::render;

/// The anchor when none is named: a child of `com.apple`, which the stock `/etc/pf.conf`
/// evaluates with `anchor "com.apple/*"`, so no edit of the main ruleset is needed.
pub const DEFAULT_ANCHOR: &str = "com.apple/com.komira.js.pool";
/// The state directory when none is named.
pub const DEFAULT_STATE_DIR: &str = "/var/db/com.komira.js-pf";

/// Limits the egress of a Mac pool's users with a pf anchor.
#[derive(Debug, Parser)]
#[command(name = "js-mac-provision", version)]
pub struct Cli {
    /// The `pfctl` to run.
    #[arg(long, global = true, default_value = SYSTEM_PFCTL)]
    pub pfctl: PathBuf,
    /// The pf anchor the rules are loaded under.
    #[arg(long, global = true, default_value = DEFAULT_ANCHOR)]
    pub anchor: String,
    /// The directory (root's, mode 0700) the rules file and the pf enable token are kept in.
    #[arg(long, global = true, default_value = DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
    /// What to do.
    #[command(subcommand)]
    pub verb: Verb,
}

/// The policy a verb renders: the pool's uids and what they may reach.
#[derive(Debug, Args)]
pub struct PolicyArgs {
    /// The first uid of the pool (500 or more).
    #[arg(long)]
    pub uid_start: u32,
    /// How many uids the pool holds.
    #[arg(long)]
    pub uid_count: u32,
    /// An endpoint the pool may connect to over TCP, as 192.0.2.10:443; repeat for more.
    /// An IPv4 literal only. None leaves the pool with loopback alone.
    #[arg(long = "endpoint")]
    pub endpoints: Vec<String>,
}

/// The policy `install` loads: given as flags, or read from the macOS agent's flags file.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// The macOS agent's flags file (`/etc/js-agent/flags`): load the anchor its
    /// `--native-network` and `--native-pf-*` flags name, unless that anchor is in force already.
    /// A file that is absent, or sets no `--native-network=on`, loads nothing.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["uid_start", "uid_count", "endpoints"]
    )]
    pub flags_file: Option<PathBuf>,
    /// The first uid of the pool (500 or more); with `--uid-count`, unless `--flags-file`.
    #[arg(long)]
    pub uid_start: Option<u32>,
    /// How many uids the pool holds.
    #[arg(long)]
    pub uid_count: Option<u32>,
    /// An endpoint the pool may connect to over TCP, as 192.0.2.10:443; repeat for more.
    /// An IPv4 literal only. None leaves the pool with loopback alone.
    #[arg(long = "endpoint")]
    pub endpoints: Vec<String>,
}

/// A verb.
#[derive(Debug, Subcommand)]
pub enum Verb {
    /// Print the anchor's rules. Reads and changes nothing.
    Render(PolicyArgs),
    /// Load the anchor, enable pf, and read the rules back (root). Safe to repeat.
    Install(InstallArgs),
    /// Compare the loaded rules with the rendered ones; exits 1 on a difference.
    Check(PolicyArgs),
    /// Flush this anchor's rules, release the pf enable token and delete the state (root).
    Remove,
    /// Print the LaunchDaemon property list that runs `install` at boot and on an interval.
    LaunchdPlist {
        /// The policy `install` is given.
        #[command(flatten)]
        policy: PolicyArgs,
        /// The absolute path of this program on the Mac.
        #[arg(long)]
        program: String,
        /// Seconds between runs after the one at boot.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..))]
        interval: u32,
    },
    /// Print the property list of the js-agent LaunchDaemon: fixed, the host's own flags are
    /// in the flags file.
    AgentPlist,
    /// Print the property list of the js-agent.pf-pool LaunchDaemon: fixed, it runs
    /// `install --flags-file` with the agent's flags file at load and every 30 seconds.
    PfPoolPlist,
    /// Connect once and print the outcome: connected, refused, timed out or failed.
    /// Run it as a pool user. Exits 1 unless it connected.
    Probe {
        /// The address and port.
        addr: SocketAddrV4,
        /// How long to wait, in milliseconds.
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
}

/// Why a verb failed.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The policy was refused.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// A step of install, check or remove failed.
    #[error(transparent)]
    Provision(#[from] ProvisionError),
    /// The output could not be written.
    #[error("cannot write the output: {0}")]
    Write(#[from] io::Error),
    /// launchd needs an absolute program path.
    #[error("--program {0:?} is not an absolute path")]
    RelativeProgram(String),
    /// `install` was given neither a flags file nor the pool's uids.
    #[error("install needs --flags-file, or --uid-start and --uid-count")]
    NoPolicy,
    /// The flags file was refused.
    #[error(transparent)]
    Flags(#[from] FlagsError),
}

/// The anchor name and the policy of a verb, checked.
fn policy(cli: &Cli, args: &PolicyArgs) -> Result<(AnchorName, UidRange, Vec<Endpoint>), CliError> {
    policy_of(cli, args.uid_start, args.uid_count, &args.endpoints)
}

/// The anchor name and a policy given as flags, checked.
fn policy_of(
    cli: &Cli,
    uid_start: u32,
    uid_count: u32,
    endpoints: &[String],
) -> Result<(AnchorName, UidRange, Vec<Endpoint>), CliError> {
    let anchor = AnchorName::new(&cli.anchor)?;
    let uids = UidRange::new(uid_start, uid_count)?;
    let endpoints = endpoints
        .iter()
        .map(|e| e.parse())
        .collect::<Result<Vec<Endpoint>, _>>()?;
    Ok((anchor, uids, endpoints))
}

/// The arguments the LaunchDaemon gives `install`: everything the policy needs, so the
/// daemon reads no file of its own.
fn install_args(cli: &Cli, uids: UidRange, endpoints: &[Endpoint]) -> Vec<String> {
    let mut args = vec![
        "install".to_owned(),
        "--pfctl".to_owned(),
        cli.pfctl.to_string_lossy().into_owned(),
        "--anchor".to_owned(),
        cli.anchor.clone(),
        "--state-dir".to_owned(),
        cli.state_dir.to_string_lossy().into_owned(),
        "--uid-start".to_owned(),
        uids.first().to_string(),
        "--uid-count".to_owned(),
        (uids.last() - uids.first() + 1).to_string(),
    ];
    for e in endpoints {
        args.push("--endpoint".to_owned());
        args.push(e.to_string());
    }
    args
}

/// `install --flags-file`: loads the anchor the agent's flags file names, unless `check` says it
/// is in force already, so that a run every 30 seconds reloads nothing under the agent's own
/// checks. The anchor, the state directory and the `pfctl` are the file's (the agent's defaults
/// when it names none), never this run's `--anchor`, `--state-dir` or `--pfctl`: they are the
/// ones the agent checks with.
fn install_from(
    cli: &Cli,
    pf: &dyn Pfctl,
    file: &Path,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let euid = rustix::process::geteuid().as_raw();
    let policy = match agent_flags::read(file, euid, &agent_flags::read_system)? {
        AgentPf::NoFile => {
            writeln!(out, "no flags file {}: nothing to install", file.display())?;
            return Ok(());
        }
        AgentPf::Off { network } => {
            let said = format!(
                "{}: {}={network}: the agent checks no anchor, nothing to install",
                file.display(),
                agent_flags::NETWORK
            );
            writeln!(out, "{said}")?;
            return Ok(());
        }
        AgentPf::On(policy) => policy,
    };
    // The agent runs the `pfctl` its flags name; so does this, unless it is the one this run has.
    let named;
    let pf: &dyn Pfctl = if policy.pfctl == cli.pfctl {
        pf
    } else {
        named = SystemPfctl::new(policy.pfctl.clone());
        &named
    };
    let provisioner = Provisioner::new(pf, &policy.anchor, &policy.state_dir);
    let (uids, anchor) = (policy.uids, &policy.anchor);
    if provisioner.check(uids, &policy.endpoints).is_ok() {
        writeln!(out, "in force: {anchor} holds the rules for uids {uids}")?;
    } else {
        provisioner.install(uids, &policy.endpoints)?;
        writeln!(out, "installed {anchor} for uids {uids}")?;
    }
    Ok(())
}

/// Runs the verb of `cli` over `pf`, printing to `out`. `Ok(false)` is a verb that ran
/// and answered no: a probe that did not connect.
///
/// # Errors
/// [`CliError`].
pub fn execute(cli: &Cli, pf: &dyn Pfctl, out: &mut dyn Write) -> Result<bool, CliError> {
    match &cli.verb {
        Verb::Render(args) => {
            let (anchor, uids, endpoints) = policy(cli, args)?;
            out.write_all(render(uids, &endpoints, &anchor).as_bytes())?;
        }
        Verb::Install(args) => match (&args.flags_file, args.uid_start, args.uid_count) {
            (Some(file), _, _) => install_from(cli, pf, file, out)?,
            (None, Some(uid_start), Some(uid_count)) => {
                let (anchor, uids, endpoints) =
                    policy_of(cli, uid_start, uid_count, &args.endpoints)?;
                Provisioner::new(pf, &anchor, &cli.state_dir).install(uids, &endpoints)?;
                writeln!(out, "installed {anchor} for uids {uids}")?;
            }
            (None, _, _) => return Err(CliError::NoPolicy),
        },
        Verb::Check(args) => {
            let (anchor, uids, endpoints) = policy(cli, args)?;
            Provisioner::new(pf, &anchor, &cli.state_dir).check(uids, &endpoints)?;
            writeln!(out, "ok: {anchor} holds the rules for uids {uids}")?;
        }
        Verb::Remove => {
            let anchor = AnchorName::new(&cli.anchor)?;
            Provisioner::new(pf, &anchor, &cli.state_dir).remove()?;
            writeln!(out, "removed {anchor}")?;
        }
        Verb::LaunchdPlist {
            policy: args,
            program,
            interval,
        } => {
            let (_, uids, endpoints) = policy(cli, args)?;
            if !std::path::Path::new(program).is_absolute() {
                return Err(CliError::RelativeProgram(program.clone()));
            }
            let args = install_args(cli, uids, &endpoints);
            out.write_all(render_plist(program, &args, *interval).as_bytes())?;
        }
        Verb::AgentPlist => {
            out.write_all(Daemon::agent().render().as_bytes())?;
        }
        Verb::PfPoolPlist => {
            out.write_all(pf_pool_plist().as_bytes())?;
        }
        Verb::Probe { addr, timeout_ms } => {
            let outcome = probe(*addr, Duration::from_millis(*timeout_ms));
            writeln!(out, "{outcome}")?;
            return Ok(outcome == Probe::Connected);
        }
    }
    Ok(true)
}

/// Parses `args` (the program name first), runs the verb against the real `pfctl` and
/// prints to standard output. A usage error exits through clap; any other failure prints
/// `error: ...` and exits 1.
#[must_use]
pub fn main_with(args: impl IntoIterator<Item = std::ffi::OsString>) -> ExitCode {
    let cli = Cli::parse_from(args);
    let pf = SystemPfctl::new(&cli.pfctl);
    match execute(&cli, &pf, &mut io::stdout()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use js_testkit::dir::TestDir;

    use super::*;
    use crate::testing::Fake;

    fn cli(dir: &std::path::Path, extra: &[&str]) -> Cli {
        let mut args = vec!["js-mac-provision", "--state-dir", dir.to_str().unwrap()];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).unwrap()
    }

    fn run(cli: &Cli, pf: &dyn Pfctl) -> (Result<bool, CliError>, String) {
        let mut out = Vec::new();
        let r = execute(cli, pf, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    const POLICY: [&str; 6] = [
        "--uid-start",
        "600",
        "--uid-count",
        "100",
        "--endpoint",
        "192.0.2.10:443",
    ];

    fn with(verb: &str, rest: &[&str]) -> Vec<String> {
        let mut v = vec![verb.to_owned()];
        v.extend(rest.iter().chain(POLICY.iter()).map(ToString::to_string));
        v
    }

    fn parse(dir: &std::path::Path, args: &[String]) -> Cli {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        cli(dir, &refs)
    }

    #[test]
    fn agent_plist_prints_the_fixed_agent_daemon() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-agent-plist");
        let pf = Fake::new(dir.path());
        let c = parse(dir.path(), &["agent-plist".to_owned()]);
        let (r, text) = run(&c, &pf);
        assert!(r.unwrap());
        assert_eq!(text, Daemon::agent().render());
        assert!(text.contains("<string>/etc/js-agent/flags</string>"));
    }

    #[test]
    fn agent_plist_takes_no_flags() {
        assert!(
            Cli::try_parse_from(["js-mac-provision", "agent-plist", "--server", "https://x:1"])
                .is_err()
        );
    }

    #[test]
    fn pf_pool_plist_prints_the_fixed_pf_daemon() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-pf-pool-plist");
        let pf = Fake::new(dir.path());
        let (r, text) = run(&parse(dir.path(), &["pf-pool-plist".to_owned()]), &pf);
        assert!(r.unwrap());
        assert_eq!(text, pf_pool_plist());
        assert!(pf.calls().is_empty());
        assert!(
            Cli::try_parse_from(["js-mac-provision", "pf-pool-plist", "--uid-start", "600"])
                .is_err()
        );
    }

    /// The agent's flags for the pool 600-699 and one endpoint, with the state directory `dir`.
    fn agent_flags(dir: &std::path::Path) -> String {
        format!(
            "--server=https://leases.example:7300\n--native-network=on\n\
             --native-pf-uid-start=600\n--native-pf-uid-count=100\n\
             --native-pf-endpoint=192.0.2.10:443\n--native-pf-state-dir={}\n",
            dir.display()
        )
    }

    /// Writes a flags file as the agent's own: not writable by group or others, whatever the
    /// umask.
    fn write_flags(path: &std::path::Path, text: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn install_from_file(dir: &std::path::Path, file: &std::path::Path) -> Cli {
        cli(dir, &["install", "--flags-file", file.to_str().unwrap()])
    }

    #[test]
    fn install_from_a_flags_file_loads_the_agents_anchor_and_then_reloads_nothing() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-flags-file");
        let file = dir.join("flags");
        write_flags(&file, &agent_flags(dir.path()));
        let pf = Fake::new(dir.path());
        let (r, text) = run(&install_from_file(dir.path(), &file), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            "installed com.apple/com.komira.js.pool for uids 600-699\n"
        );
        let load = format!(
            "-a com.apple/com.komira.js.pool -f {}",
            dir.join("pool.pf").display()
        );
        assert_eq!(pf.calls().iter().filter(|c| **c == load).count(), 1);
        let rules = std::fs::read_to_string(dir.join("pool.pf")).unwrap();
        assert!(
            rules.contains("to 192.0.2.10 port 443 user { 600, 601, "),
            "{rules}"
        );
        let (r, text) = run(&install_from_file(dir.path(), &file), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            "in force: com.apple/com.komira.js.pool holds the rules for uids 600-699\n"
        );
        assert_eq!(pf.calls().iter().filter(|c| **c == load).count(), 1);
    }

    #[test]
    fn install_from_a_flags_file_whose_endpoints_changed_loads_the_new_rules() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-flags-change");
        let file = dir.join("flags");
        write_flags(&file, &agent_flags(dir.path()));
        let pf = Fake::new(dir.path());
        assert!(run(&install_from_file(dir.path(), &file), &pf).0.unwrap());
        let changed = agent_flags(dir.path()).replace("192.0.2.10:443", "198.51.100.7:8443");
        write_flags(&file, &changed);
        let (r, text) = run(&install_from_file(dir.path(), &file), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            "installed com.apple/com.komira.js.pool for uids 600-699\n"
        );
        let rules = std::fs::read_to_string(dir.join("pool.pf")).unwrap();
        assert!(rules.contains("to 198.51.100.7 port 8443 user"), "{rules}");
        assert!(!rules.contains("192.0.2.10"), "{rules}");
    }

    #[test]
    fn install_from_a_flags_file_that_is_absent_or_not_on_runs_no_pfctl() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-flags-nothing");
        let pf = Fake::new(dir.path());
        let absent = dir.join("absent");
        let (r, text) = run(&install_from_file(dir.path(), &absent), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            format!("no flags file {}: nothing to install\n", absent.display())
        );
        let off = dir.join("off");
        write_flags(
            &off,
            "--server=https://leases.example:7300\n--native-network=open\n",
        );
        let (r, text) = run(&install_from_file(dir.path(), &off), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            format!(
                "{}: --native-network=open: the agent checks no anchor, nothing to install\n",
                off.display()
            )
        );
        assert!(pf.calls().is_empty());
        assert!(!dir.join("pool.pf").exists());
    }

    /// The anchor, the state directory and the `pfctl` are the ones the flags file names, which
    /// the agent checks with, whatever this run's `--anchor` and `--pfctl` say. Catches a daemon
    /// (which runs with no such flag) that could never load a non-default anchor the agent accepts.
    #[test]
    fn install_from_a_flags_file_uses_the_files_anchor_and_pfctl_not_this_runs() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-flags-named");
        let pf = Fake::new(dir.path());
        let file = dir.join("flags");
        let other = format!(
            "{}--native-pf-anchor=com.apple/other\n",
            agent_flags(dir.path())
        );
        write_flags(&file, &other);
        let (r, text) = run(&install_from_file(dir.path(), &file), &pf);
        assert!(r.unwrap());
        assert_eq!(text, "installed com.apple/other for uids 600-699\n");
        let load = format!("-a com.apple/other -f {}", dir.join("pool.pf").display());
        assert!(pf.calls().contains(&load), "{:?}", pf.calls());
        let named = format!("{other}--native-pfctl=/nonexistent/pfctl\n");
        write_flags(&file, &named);
        let calls = pf.calls().len();
        let (r, _) = run(&install_from_file(dir.path(), &file), &pf);
        let e = r.unwrap_err();
        assert!(matches!(e, CliError::Provision(_)), "{e:?}");
        assert!(e.to_string().contains("could not run pfctl"), "{e}");
        assert_eq!(pf.calls().len(), calls);
    }

    #[test]
    fn install_from_a_refused_flags_file_runs_no_pfctl() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-flags-refused");
        let pf = Fake::new(dir.path());
        let file = dir.join("flags");
        write_flags(&file, "--native-network=on\n--native-network=off\n");
        let (r, _) = run(&install_from_file(dir.path(), &file), &pf);
        let e = r.unwrap_err();
        assert!(matches!(e, CliError::Flags(_)), "{e:?}");
        assert!(
            e.to_string()
                .ends_with("--native-network: given more than once"),
            "{e}"
        );
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn install_needs_a_flags_file_or_both_uid_flags_and_not_both_kinds() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-install-args");
        let pf = Fake::new(dir.path());
        for args in [
            vec!["install"],
            vec!["install", "--uid-start", "600"],
            vec!["install", "--uid-count", "100"],
        ] {
            let (r, _) = run(&cli(dir.path(), &args), &pf);
            let e = r.unwrap_err();
            assert!(matches!(e, CliError::NoPolicy), "{args:?}: {e:?}");
            assert_eq!(
                e.to_string(),
                "install needs --flags-file, or --uid-start and --uid-count"
            );
        }
        for extra in [
            ["--uid-start", "600"],
            ["--uid-count", "100"],
            ["--endpoint", "192.0.2.10:443"],
        ] {
            let mut args = vec![
                "js-mac-provision",
                "install",
                "--flags-file",
                "/etc/js-agent/flags",
            ];
            args.extend(extra);
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn the_session_plist_verb_is_gone() {
        assert!(
            Cli::try_parse_from([
                "js-mac-provision",
                "session-plist",
                "--daemon-requirement",
                "x",
            ])
            .is_err()
        );
    }

    #[test]
    fn the_postinstall_verb_is_gone() {
        assert!(
            Cli::try_parse_from(["js-mac-provision", "postinstall", "--version", "1"]).is_err()
        );
    }

    #[test]
    fn render_prints_the_anchor_and_touches_no_pfctl() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-render");
        let pf = Fake::new(dir.path());
        let c = parse(
            dir.path(),
            &with("render", &["--endpoint", "198.51.100.7:8443"]),
        );
        let (r, text) = run(&c, &pf);
        assert!(r.unwrap());
        assert!(text.contains(
            "pass out quick inet proto tcp from any to 192.0.2.10 port 443 user { 600, 601, 602, "
        ));
        assert!(text.contains(", 698, 699 } keep state"));
        assert!(text.contains("to 198.51.100.7 port 8443"));
        assert!(pf.calls().is_empty());
        assert!(!dir.join("pool.pf").exists());
    }

    #[test]
    fn install_check_and_remove_run_the_provisioner() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-verbs");
        let pf = Fake::new(dir.path());
        let (r, text) = run(&parse(dir.path(), &with("install", &[])), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            "installed com.apple/com.komira.js.pool for uids 600-699\n"
        );
        let (r, text) = run(&parse(dir.path(), &with("check", &[])), &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            "ok: com.apple/com.komira.js.pool holds the rules for uids 600-699\n"
        );
        let (r, text) = run(&parse(dir.path(), &["remove".to_owned()]), &pf);
        assert!(r.unwrap());
        assert_eq!(text, "removed com.apple/com.komira.js.pool\n");
        assert!(
            pf.calls()
                .contains(&"-a com.apple/com.komira.js.pool -F rules".to_owned())
        );
    }

    #[test]
    fn a_named_anchor_and_pfctl_are_used() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-anchor");
        let pf = Fake::new(dir.path());
        let mut args = with("install", &[]);
        args.extend(["--anchor".to_owned(), "com.apple/other".to_owned()]);
        let (r, _) = run(&parse(dir.path(), &args), &pf);
        assert!(r.unwrap());
        assert!(pf.calls()[0].starts_with("-a com.apple/other -f "));
    }

    #[test]
    fn a_refused_policy_is_an_error_before_pfctl_runs() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-refuse");
        let pf = Fake::new(dir.path());
        for (args, why) in [
            (
                vec!["install", "--uid-start", "600", "--uid-count", "0"],
                "empty",
            ),
            (
                vec!["install", "--uid-start", "400", "--uid-count", "10"],
                "below 500",
            ),
            (
                vec![
                    "install",
                    "--uid-start",
                    "600",
                    "--uid-count",
                    "1",
                    "--endpoint",
                    "example.com:443",
                ],
                "not an IPv4",
            ),
            (
                vec![
                    "check",
                    "--uid-start",
                    "600",
                    "--uid-count",
                    "1",
                    "--endpoint",
                    "169.254.169.254:80",
                ],
                "link-local",
            ),
            (
                vec![
                    "render",
                    "--uid-start",
                    "600",
                    "--uid-count",
                    "1",
                    "--anchor",
                    "a/*",
                ],
                "anchor name",
            ),
        ] {
            let (r, _) = run(&cli(dir.path(), &args), &pf);
            let e = r.unwrap_err();
            assert!(matches!(e, CliError::Policy(_)), "{args:?}");
            assert!(e.to_string().contains(why), "{e}");
        }
        let (r, _) = run(&cli(dir.path(), &["remove", "--anchor", "a b"]), &pf);
        assert!(matches!(r, Err(CliError::Policy(_))));
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn a_failed_step_is_a_provision_error() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-fail");
        let mut pf = Fake::new(dir.path());
        pf.fail = Some((0, false));
        let (r, _) = run(&parse(dir.path(), &with("install", &[])), &pf);
        let e = r.unwrap_err();
        assert!(matches!(e, CliError::Provision(_)));
        assert!(e.to_string().contains("exited with 1"));
    }

    #[test]
    fn the_plist_carries_the_whole_install_command() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-plist");
        let pf = Fake::new(dir.path());
        let args = with(
            "launchd-plist",
            &["--program", "/opt/js/js-mac-provision", "--interval", "120"],
        );
        let (r, text) = run(&parse(dir.path(), &args), &pf);
        assert!(r.unwrap());
        let strings: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("<string>")?.strip_suffix("</string>"))
            .collect();
        let state = dir.path().to_str().unwrap();
        assert_eq!(
            strings[1..],
            [
                "/opt/js/js-mac-provision",
                "install",
                "--pfctl",
                "/sbin/pfctl",
                "--anchor",
                "com.apple/com.komira.js.pool",
                "--state-dir",
                state,
                "--uid-start",
                "600",
                "--uid-count",
                "100",
                "--endpoint",
                "192.0.2.10:443",
                "/var/log/js-mac-provision.log",
            ]
        );
        assert!(text.contains("<integer>120</integer>"));
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn the_plist_refuses_a_relative_program_and_a_zero_interval() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-plist2");
        let pf = Fake::new(dir.path());
        let args = with("launchd-plist", &["--program", "js-mac-provision"]);
        let (r, _) = run(&parse(dir.path(), &args), &pf);
        assert_eq!(
            r.unwrap_err().to_string(),
            "--program \"js-mac-provision\" is not an absolute path"
        );
        let mut zero = with("launchd-plist", &["--program", "/x", "--interval", "0"]);
        zero.insert(0, "--state-dir".to_owned());
        zero.insert(1, "/x".to_owned());
        let refs: Vec<&str> = std::iter::once("js-mac-provision")
            .chain(zero.iter().map(String::as_str))
            .collect();
        assert!(Cli::try_parse_from(refs).is_err());
    }

    #[test]
    fn probe_answers_yes_only_for_a_connect() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-probe");
        let pf = Fake::new(dir.path());
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let (r, text) = run(&cli(dir.path(), &["probe", &addr]), &pf);
        assert!(r.unwrap());
        assert_eq!(text, "connected\n");
        let dead = js_testkit::port::dead_addr().to_string();
        let (r, text) = run(
            &cli(dir.path(), &["probe", &dead, "--timeout-ms", "5000"]),
            &pf,
        );
        assert!(!r.unwrap());
        assert_eq!(text, "refused\n");
    }

    #[test]
    fn a_probe_of_a_non_ipv4_address_is_a_usage_error() {
        assert!(Cli::try_parse_from(["js-mac-provision", "probe", "example.com:80"]).is_err());
        assert!(Cli::try_parse_from(["js-mac-provision", "probe", "[::1]:80"]).is_err());
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_output_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-write");
        let pf = Fake::new(dir.path());
        let e = execute(&parse(dir.path(), &with("render", &[])), &pf, &mut Broken).unwrap_err();
        assert!(matches!(e, CliError::Write(_)));
        assert!(e.to_string().contains("closed"));
        assert!(Broken.flush().is_ok());
    }

    #[test]
    fn the_defaults_are_the_system_pfctl_and_the_stock_anchor() {
        let c = Cli::try_parse_from(["js-mac-provision", "remove"]).unwrap();
        assert_eq!(c.pfctl, PathBuf::from("/sbin/pfctl"));
        assert_eq!(c.anchor, DEFAULT_ANCHOR);
        assert_eq!(c.state_dir, PathBuf::from(DEFAULT_STATE_DIR));
    }
}
