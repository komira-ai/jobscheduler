//! `--cpu-ceiling`: an optional ceiling on each job's CPU as a multiple of its booking. It is
//! read as a multiple from 1 to 1000, reaches the runtime's settings, is off by default, and
//! is refused by name on an agent that runs no job. (Linux only: only Linux runs containers.)

use clap::Parser;
use js_agent::Cli;
use js_contain::CpuCeiling;

use js_agent::cli_runtime::Settings;
use js_agent::lifecycle::Local;

use crate::crun_host::Fixture;
use crate::support::{CountingSleeper, Lowest, Script, ScriptConnector, code, flags, linux_host};
use crate::support::{run_defaulted, welcome};

/// The agent's flags with `--cpu-ceiling` `value`, parsed.
fn parse(value: &str) -> Result<Cli, clap::Error> {
    Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "p",
        "--cpu-ceiling",
        value,
    ])
}

/// Proves the flag is read as a multiple of the booking and refused, naming it, when it is
/// below 1 (a job would be held below what it is guaranteed) or not a plain decimal; and that
/// with no flag there is no ceiling. Catches a ceiling read wrongly, one below the booking
/// taken, and a ceiling on by default.
#[test]
fn the_ceiling_is_a_multiple_of_the_booking_and_off_by_default() {
    for (value, permille) in [("1", 1000), ("1.5", 1500), ("2", 2000)] {
        let cli = parse(value).expect("parses");
        assert_eq!(cli.os.cpu_ceiling.map(CpuCeiling::permille), Some(permille));
    }
    for value in ["0.5", "0", "1.2345", "two"] {
        let said = parse(value).map(|_| ()).unwrap_err().to_string();
        assert!(said.contains("--cpu-ceiling <MULTIPLE>"), "{said}");
    }
    let none = Cli::try_parse_from(["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"]);
    assert_eq!(none.expect("parses").os.cpu_ceiling, None);
}

/// Proves the ceiling reaches the plan a crun runtime is opened from, and that without the
/// flag it holds none. Catches a flag that is read and dropped.
#[test]
fn the_ceiling_reaches_the_crun_plan() {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let fixture = Fixture::with_ids("cpu-ceiling", uid, gid);
    let plan_of = |extra: &[&str]| {
        let (state, scratch) = (fixture.state.display(), fixture.scratch.display());
        let given = [
            format!("--state-root={state}"),
            format!("--scratch-root={scratch}"),
        ];
        let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
        args.extend(["--job-user", Fixture::USER]);
        args.extend(given.iter().map(String::as_str));
        args.extend(extra);
        let cli = Cli::try_parse_from(args).expect("a command line");
        let settings =
            Settings::for_crun(fixture.crun.clone(), Fixture::USER, (uid, gid), Vec::new());
        let euid = rustix::process::geteuid().as_raw();
        let local = Local::from_cli_crun(&cli, euid, settings).expect("the flags pass");
        let backend = local.backend.expect("containers are enabled");
        let plan = backend
            .settings()
            .crun()
            .expect("crun settings")
            .cpu_ceiling;
        (plan, backend.settings().cpu_ceiling())
    };
    let ceiling: CpuCeiling = "1.5".parse().expect("a ceiling");
    assert_eq!(
        plan_of(&["--cpu-ceiling", "1.5"]),
        (Some(ceiling), Some(ceiling))
    );
    assert_eq!(plan_of(&[]), (None, None));
}

/// Proves an agent that runs no job refuses the flag by name before it sends anything.
/// Catches a ceiling accepted and ignored.
#[test]
fn an_agent_with_no_jobs_refuses_the_ceiling() {
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run_defaulted(
        &flags(&["--no-jobs", "--cpu-ceiling", "2"]),
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("js-agent: --cpu-ceiling: not with --no-jobs (an agent that runs no job)"),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty());
}
