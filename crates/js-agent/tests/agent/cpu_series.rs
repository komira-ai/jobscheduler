//! `--cpu-series`: record each job's CPU second by second and send it with the job's result.
//! It is a plain switch, off by default, reaches the crun runtime's settings, and is refused by
//! name on an agent that runs no job. (Linux only: only Linux runs containers.)

use clap::Parser;
use js_agent::Cli;

use js_agent::cli_runtime::Settings;
use js_agent::lifecycle::Local;

use crate::crun_host::Fixture;
use crate::support::{CountingSleeper, Lowest, Script, ScriptConnector, code, flags, linux_host};
use crate::support::{run_defaulted, welcome};

/// The agent's flags with `extra` after the required ones, parsed.
fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend(extra);
    Cli::try_parse_from(args)
}

/// Proves the flag is a plain switch that is off by default and takes no value. Catches a
/// recording that is on by default, and a flag that wants an argument.
#[test]
fn the_flag_is_a_switch_and_off_by_default() {
    assert!(!parse(&[]).expect("parses").samples.cpu_series);
    assert!(parse(&["--cpu-series"]).expect("parses").samples.cpu_series);
    assert!(parse(&["--cpu-series", "yes"]).is_err());
}

/// Proves the flag reaches the settings the crun runtime is opened from, and that without it
/// the runtime is opened with none. Catches a flag that is read and dropped.
#[test]
fn the_flag_reaches_the_crun_settings() {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let fixture = Fixture::with_ids("cpu-series", uid, gid);
    let plan_of = |extra: &[&str]| {
        let (state, scratch) = (fixture.state.display(), fixture.scratch.display());
        let given = [
            format!("--state-root={state}"),
            format!("--scratch-root={scratch}"),
        ];
        let mut args = vec!["--job-user", Fixture::USER];
        args.extend(given.iter().map(String::as_str));
        args.extend(extra);
        let cli = parse(&args).expect("a command line");
        let settings =
            Settings::for_crun(fixture.crun.clone(), Fixture::USER, (uid, gid), Vec::new());
        let euid = rustix::process::geteuid().as_raw();
        let local = Local::from_cli_crun(&cli, euid, settings).expect("the flags pass");
        let backend = local.backend.expect("containers are enabled");
        backend.settings().crun().expect("crun settings").cpu_series
    };
    assert!(plan_of(&["--cpu-series"]));
    assert!(!plan_of(&[]));
}

/// Proves an agent that runs no job refuses the flag by name before it sends anything.
/// Catches a flag accepted and ignored.
#[test]
fn an_agent_with_no_jobs_refuses_the_flag() {
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run_defaulted(
        &flags(&["--no-jobs", "--cpu-series"]),
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("js-agent: --cpu-series: not with --no-jobs (an agent that runs no job)"),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty());
}
