//! The program's verbs. [`execute`] runs one over a [`Pfctl`] and a writer, so the whole
//! of it is tested against a fake; [`main_with`] is the thin shell that builds the real
//! runner and the exit code.

use std::io::{self, Write};
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::daemon::Daemon;
use crate::launchd::render_plist;
use crate::pfctl::{Pfctl, SYSTEM_PFCTL, SystemPfctl};
use crate::policy::{AnchorName, Endpoint, PolicyError, UidRange};
use crate::postinstall::{self, LAUNCHCTL, Launchctl, SystemLaunchctl};
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

/// A verb.
#[derive(Debug, Subcommand)]
pub enum Verb {
    /// Print the anchor's rules. Reads and changes nothing.
    Render(PolicyArgs),
    /// Load the anchor, enable pf, and read the rules back (root). Safe to repeat.
    Install(PolicyArgs),
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
    /// Print the property list of the js-agent LaunchDaemon.
    AgentPlist {
        /// The lease server's address, the agent's `--server`.
        #[arg(long)]
        server: String,
        /// The pool the host asks to join, the agent's `--pool`.
        #[arg(long, default_value = "default")]
        pool: String,
    },
    /// Print the property list of the js-mac-session LaunchDaemon.
    SessionPlist {
        /// The code requirement the helper holds its caller to.
        #[arg(long)]
        daemon_requirement: String,
        /// The group that may reach the helper's socket.
        #[arg(long)]
        socket_group: String,
    },
    /// What the installer package does after it has laid its files down (root): make the
    /// release link when there is none, install the plists that are not installed, and
    /// load the daemons that are not loaded. Never stops or restarts anything.
    Postinstall {
        /// The release the package laid down.
        #[arg(long)]
        version: String,
        /// The volume the package was installed on; daemons are loaded only for `/`.
        #[arg(long, default_value = "/")]
        volume: PathBuf,
    },
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
    /// The postinstall stopped.
    #[error(transparent)]
    Postinstall(#[from] postinstall::Error),
    /// The output could not be written.
    #[error("cannot write the output: {0}")]
    Write(#[from] io::Error),
    /// launchd needs an absolute program path.
    #[error("--program {0:?} is not an absolute path")]
    RelativeProgram(String),
}

/// The anchor name and the policy of a verb, checked.
fn policy(cli: &Cli, args: &PolicyArgs) -> Result<(AnchorName, UidRange, Vec<Endpoint>), CliError> {
    let anchor = AnchorName::new(&cli.anchor)?;
    let uids = UidRange::new(args.uid_start, args.uid_count)?;
    let endpoints = args
        .endpoints
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

/// `launchctl` for the volume the package was installed on: only the running system, `/`,
/// has daemons to load.
fn launchctl_for<'a>(
    volume: &std::path::Path,
    real: &'a SystemLaunchctl,
) -> Option<&'a dyn Launchctl> {
    let _ = (volume, real);
    None
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
        Verb::Install(args) => {
            let (anchor, uids, endpoints) = policy(cli, args)?;
            Provisioner::new(pf, &anchor, &cli.state_dir).install(uids, &endpoints)?;
            writeln!(out, "installed {anchor} for uids {uids}")?;
        }
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
        Verb::AgentPlist { server, pool } => {
            out.write_all(Daemon::agent(server, pool).render().as_bytes())?;
        }
        Verb::SessionPlist {
            daemon_requirement,
            socket_group,
        } => {
            let daemon = Daemon::session(daemon_requirement, socket_group);
            out.write_all(daemon.render().as_bytes())?;
        }
        Verb::Postinstall { version, volume } => {
            let real = SystemLaunchctl::new(LAUNCHCTL);
            let lines = postinstall::run(volume, version, launchctl_for(volume, &real))?;
            for line in lines {
                writeln!(out, "{line}")?;
            }
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
    fn agent_plist_prints_the_agents_daemon_for_the_server_and_pool() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-agent-plist");
        let pf = Fake::new(dir.path());
        let c = parse(
            dir.path(),
            &[
                "agent-plist".to_owned(),
                "--server".to_owned(),
                "https://leases.example:7300".to_owned(),
                "--pool".to_owned(),
                "mac".to_owned(),
            ],
        );
        let (r, text) = run(&c, &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            Daemon::agent("https://leases.example:7300", "mac").render()
        );
        assert!(text.contains("<string>mac</string>"));
    }

    #[test]
    fn agent_plist_joins_the_default_pool_when_none_is_named() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-agent-pool");
        let pf = Fake::new(dir.path());
        let c = parse(
            dir.path(),
            &[
                "agent-plist".to_owned(),
                "--server".to_owned(),
                "https://x:1".to_owned(),
            ],
        );
        let (r, text) = run(&c, &pf);
        assert!(r.unwrap());
        assert_eq!(text, Daemon::agent("https://x:1", "default").render());
    }

    #[test]
    fn session_plist_prints_the_helpers_daemon() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-session-plist");
        let pf = Fake::new(dir.path());
        let c = parse(
            dir.path(),
            &[
                "session-plist".to_owned(),
                "--daemon-requirement".to_owned(),
                "cdhash H\"abc\"".to_owned(),
                "--socket-group".to_owned(),
                "_jsagent".to_owned(),
            ],
        );
        let (r, text) = run(&c, &pf);
        assert!(r.unwrap());
        assert_eq!(
            text,
            Daemon::session("cdhash H\"abc\"", "_jsagent").render()
        );
    }

    #[test]
    fn postinstall_on_a_volume_that_is_not_the_running_system_prints_what_it_did() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-postinstall");
        let root = dir.path().join("vol");
        std::fs::create_dir_all(root.join("usr/local/js-agent/releases/1.0.0")).unwrap();
        std::fs::create_dir_all(root.join("usr/local/js-agent/share")).unwrap();
        std::fs::create_dir_all(root.join("Library/LaunchDaemons")).unwrap();
        for label in [crate::daemon::AGENT_LABEL, crate::daemon::SESSION_LABEL] {
            std::fs::write(
                root.join(format!("usr/local/js-agent/share/{label}.plist")),
                "x",
            )
            .unwrap();
        }
        let pf = Fake::new(dir.path());
        let args = [
            "postinstall".to_owned(),
            "--version".to_owned(),
            "1.0.0".to_owned(),
            "--volume".to_owned(),
            root.to_str().unwrap().to_owned(),
        ];
        let (r, text) = run(&parse(dir.path(), &args), &pf);
        assert!(r.unwrap());
        assert!(
            text.starts_with("linked /usr/local/js-agent/link/current to "),
            "{text}"
        );
        assert!(
            text.ends_with("the volume is not the running system: nothing was loaded\n"),
            "{text}"
        );
    }

    #[test]
    fn postinstall_with_a_release_that_is_not_there_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-cli-postinstall-bad");
        let pf = Fake::new(dir.path());
        let args = [
            "postinstall".to_owned(),
            "--version".to_owned(),
            "1.0.0".to_owned(),
            "--volume".to_owned(),
            dir.path().to_str().unwrap().to_owned(),
        ];
        let (r, _) = run(&parse(dir.path(), &args), &pf);
        assert!(matches!(r, Err(CliError::Postinstall(_))));
    }

    #[test]
    fn only_the_running_system_has_a_launchctl() {
        let real = SystemLaunchctl::new("true");
        assert!(launchctl_for(std::path::Path::new("/"), &real).is_some());
        assert!(launchctl_for(std::path::Path::new("/Volumes/Other"), &real).is_none());
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
            "pass out quick inet proto tcp from any to 192.0.2.10 port 443 user 600:699"
        ));
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
