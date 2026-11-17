//! `js job wait`, written by hand: reads a job until it has ended
//! (`js_client::jobs::wait_for_terminal`) and exits with a status that says how it ended.
//! This file reads the flag, prints the end, and holds [`OUTCOMES`], the one table from a
//! job's end to an exit status, which `job run` uses too.

use std::future::pending;
use std::io::Write;
use std::pin::pin;

use clap::{Arg, ArgMatches};
use js_client::jobs::{Outcome, WaitError, wait_for_terminal};

use super::{Connection, FLAG_ORDER, runtime, show};
use crate::Services;
use crate::failure::{Failure, exit};
use crate::render::Render;

/// One row of the table: an end, its exit status, its name as printed, and what the help
/// says of it.
pub(crate) struct Row {
    outcome: Outcome,
    code: u8,
    name: &'static str,
    meaning: &'static str,
}

/// How a job's end is an exit status of `job wait` and `job run`. A job that succeeded is
/// 0; every other end has a status of its own, apart from the statuses `js` gives its own
/// failures (1 to 9), and [`UNDETERMINED`] is what is left when the end cannot be told.
pub(crate) const OUTCOMES: [Row; 6] = [
    Row {
        outcome: Outcome::Succeeded,
        code: exit::OK,
        name: "succeeded",
        meaning: "the job succeeded: it exited with code 0",
    },
    Row {
        outcome: Outcome::Failed,
        code: exit::JOB_FAILED,
        name: "failed",
        meaning: "the job failed: it exited with another code, or was stopped for its memory",
    },
    Row {
        outcome: Outcome::Cancelled,
        code: exit::JOB_CANCELLED,
        name: "cancelled",
        meaning: "the job was cancelled",
    },
    Row {
        outcome: Outcome::TimedOut,
        code: exit::JOB_TIMED_OUT,
        name: "timed-out",
        meaning: "the job timed out: it was stopped at its wall limit",
    },
    Row {
        outcome: Outcome::Lost,
        code: exit::JOB_LOST,
        name: "lost",
        meaning: "the job was lost: its container is gone and no result exists",
    },
    Row {
        outcome: Outcome::FailedToStart,
        code: exit::JOB_NOT_STARTED,
        name: "failed-to-start",
        meaning: "the job failed to start",
    },
];

/// What the help says of [`exit::UNDETERMINED`].
const UNDETERMINED: &str = "how the job ended could not be told: a call failed (js does not retry it), or the job is in a state this js does not know";

/// The exit status and the printed name of `outcome`, from [`OUTCOMES`].
pub(crate) fn status(outcome: Outcome) -> (u8, &'static str) {
    let row = OUTCOMES.iter().find(|row| row.outcome == outcome);
    row.map_or((exit::UNDETERMINED, "undetermined"), |row| {
        (row.code, row.name)
    })
}

/// The help's table of exit statuses: [`OUTCOMES`], [`exit::UNDETERMINED`], then `more`
/// (statuses and what each means) and a wrong flag.
pub(crate) fn statuses(more: &[(&str, &str)]) -> String {
    let rows = OUTCOMES
        .iter()
        .map(|row| (row.code.to_string(), row.meaning))
        .chain([(exit::UNDETERMINED.to_string(), UNDETERMINED)])
        .chain(
            more.iter()
                .map(|(code, meaning)| ((*code).to_owned(), *meaning)),
        )
        .chain([(exit::USAGE.to_string(), "a flag is wrong")]);
    let lines: String = rows
        .map(|(code, meaning)| format!("  {code:<13} {meaning}\n"))
        .collect();
    format!("Exit status:\n{lines}")
}

/// The command line of `job wait`.
pub(crate) fn command() -> clap::Command {
    clap::Command::new("wait")
        .about("Waits for a job to end, and exits with a status that says how it ended.")
        .long_about(
            "Waits for a job to end, and exits with a status that says how it ended. Reads the job (GetJob) every two seconds until it has ended, and a failed job's result (GetResult) to tell a job stopped at its wall limit from one that failed otherwise; then prints `job <id> <end>` (with --json, `{\"id\": .., \"outcome\": ..}`). A call that fails is not retried: the status is then 15. Written by hand, not generated from the job service.",
        )
        .arg(job_id("The id of the job to wait for."))
        .after_help(statuses(&[]))
}

/// The required `--job-id` flag, with `help`.
pub(crate) fn job_id(help: &'static str) -> Arg {
    Arg::new("job-id")
        .long("job-id")
        .value_name("TEXT")
        .help(help)
        .required(true)
        .display_order(FLAG_ORDER)
}

/// The value of `--job-id`, which clap requires.
pub(crate) fn job_of(args: &ArgMatches) -> String {
    args.get_one::<String>("job-id")
        .cloned()
        .unwrap_or_default()
}

/// Runs `job wait`.
pub(crate) fn run(
    globals: &ArgMatches,
    args: &ArgMatches,
    render: Render,
    out: &mut dyn Write,
    services: &Services,
) -> Result<u8, Failure> {
    let job = job_of(args);
    let connection = Connection::of(globals, &services.dialer)?;
    let waiter = &*services.job_waiter;
    runtime()?.block_on(async {
        let mut client = connection.client().await?;
        let waited = wait_for_terminal(&mut client, waiter, &job, pin!(pending())).await;
        ended(&job, waited, render, out)
    })
}

/// Prints how job `job` ended on `to` and gives its exit status, or the failure of a wait
/// that could not tell. The status is the answer, so a line that cannot be printed does not
/// change it.
pub(crate) fn ended(
    job: &str,
    waited: Result<Outcome, WaitError>,
    render: Render,
    to: &mut dyn Write,
) -> Result<u8, Failure> {
    let outcome = waited
        .map_err(|why| Failure::Undetermined(format!("cannot tell how job {job} ended: {why}")))?;
    let (code, name) = status(outcome);
    let _ = show(to, &render.ended(job, name));
    Ok(code)
}
