//! `js ci run`: runs one CI step as a job of the job service, for a CI runner that is not
//! the machine the step needs (a GitHub-hosted Linux runner whose step must run on a Mac).
//! The GitHub Action `actions/run` of this repository calls it; `docs/ci-runner.md` says
//! how it is used and why it is built this way.
//!
//! The command builds the job from its flags, then submits it, writes its stdout and stderr
//! as they come, waits for its end and cancels it at a signal, as `job run` does
//! (`crate::jobs::Run`). The job is `/bin/sh` running the script of [`script`]: it fetches
//! the one commit of the step's repository, runs the step's commands with bash, and writes
//! the declared outputs to stdout as a framed tar archive, which [`frame`] takes out of the
//! stream and [`outputs`] unpacks into `--outputs-dir`.
//!
//! The job is sized from history: its `size-class` label is `--step` written as a class
//! (`js_types::SizeClass::of_target`), so every run of one step of one workflow reads and
//! teaches one history, and its `size-fallback` is `--size-fallback`, by default the step's
//! kind, `ci-macos` or `ci-linux`, whose history sizes a step with too few runs of its own.
//! The job names no CPU or memory.

mod frame;
mod outputs;
mod script;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use clap::{ArgMatches, Args, FromArgMatches, ValueEnum};
use js_client::jobs::Outcome;
use js_proto::v1::{Constraints, ExecMode, JobFile, JobSpec, NetworkLevel, SubmitJobRequest};
use js_types::{SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL, SizeClass};

use crate::Services;
use crate::cli::parse_duration;
use crate::failure::{Failure, exit};
use crate::jobs::{Connection, Run, job_key, report, runtime, show, watch_signals};
use crate::render::Render;
use frame::{Demux, Frame};

/// Where the flags of `ci run` stand in its help: after the global flags, as the job
/// commands' do.
const FLAG_ORDER: usize = 100;

/// The program every CI job runs; the script is its argument.
const SHELL: &str = "/bin/sh";

/// The beginning of a run's outputs marker; the job's id follows.
const MARKER_PREFIX: &str = "js-ci-outputs-";

/// The exit statuses of `ci run`, for its help.
const STATUSES: &str = "Exit status:
  0             the job succeeded, and its declared outputs were unpacked
  1 to 255      the job's step exited with this code (outcome failed); a job that failed without an exit code of its own, stopped for its memory, is 10
  11            the job was cancelled
  12            the job timed out: it was stopped at its wall limit
  13            the job was lost: its container is gone and no result exists
  14            the job failed to start
  15            how the job ended could not be told: a call failed, or the job is in a state this js does not know
  16            the job succeeded, but its declared outputs could not be read back
  1, 3, 4, 9    the submit failed, as for js job submit; with 9 the job may exist, and the same command with the same --id is safe to run again
  129, 130, 143 a second SIGHUP, SIGINT or SIGTERM ended js before the job ended; the job may still be running
  2             a flag is wrong
A step's own exit code can equal one of js's statuses; the outcome js prints (and writes to --github-output) tells them apart.";

/// The machine a step runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Os {
    /// macOS on Apple silicon, natively under a sandbox-exec profile.
    Macos,
    /// Linux, in a container.
    Linux,
}

/// What the job may reach over the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Network {
    /// Nothing: the job cannot fetch its source, so only for a source the host holds.
    None,
    /// The endpoints the host's operator allows.
    AllowList,
    /// Any address but the host's own and link-local ones.
    Open,
}

/// The flags of `ci run`.
#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// The machine the step runs on: macos (Apple silicon, natively under sandbox-exec) or
    /// linux (in a container).
    #[arg(long, value_enum, value_name = "OS")]
    os: Os,
    /// The image, pinned by digest (NAME@sha256:HEX): on Linux the container's, which must
    /// hold sh, mktemp, git, bash and tar; on macOS the bundle the job is named by.
    #[arg(long, value_name = "REF")]
    image: String,
    /// The step's name, which its size class is made of, such as
    /// `owner/repo/workflow/job`: every run of one name reads and teaches one history of
    /// what the step used.
    #[arg(long, value_name = "NAME")]
    step: String,
    /// The class whose history sizes the step while its own has fewer than five runs.
    /// Default: the step's kind, ci-macos or ci-linux.
    #[arg(long, value_name = "NAME")]
    size_fallback: Option<String>,
    /// The step's commands, run as GitHub runs a `run:` step (bash -e -o pipefail) in the
    /// checkout.
    #[arg(long, value_name = "SCRIPT")]
    run: String,
    /// The repository the job fetches its source from: an `https://` URL with no user or
    /// password.
    #[arg(long, value_name = "URL")]
    source_url: String,
    /// The commit the job checks out: its full id, 40 or 64 lower-case hex digits.
    #[arg(long, value_name = "SHA")]
    source_commit: String,
    /// A file holding a token the fetch authenticates with (as the user x-access-token), for
    /// a repository that is not public: a regular file owned by you, mode 0600 or 0400. It
    /// reaches the job as a job file, which no read of the job returns.
    #[arg(long, value_name = "PATH")]
    source_token_file: Option<PathBuf>,
    /// The step's outputs: paths relative to the checkout, one per line. Each that the step
    /// made is brought back into --outputs-dir, whether or not the step succeeded.
    #[arg(long, value_name = "PATHS", requires = "outputs_dir")]
    outputs: Option<String>,
    /// The directory the outputs are unpacked into, made when missing.
    #[arg(long, value_name = "DIR")]
    outputs_dir: Option<PathBuf>,
    /// The most the step may run, such as 90s, 15m or 2h.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration, default_value = "1h")]
    wall_limit: Duration,
    /// What the job may reach over the network: none, allow-list or open. The fetch needs
    /// the repository.
    #[arg(long, value_enum, value_name = "LEVEL", default_value = "open")]
    network: Network,
    /// A variable of the step's environment, KEY=VALUE; may be repeated. Not for secrets:
    /// every read of the job returns its environment.
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// The job's id: 1 to 63 bytes of letters, digits, dot, underscore and dash. Not given:
    /// js draws one and prints it on stderr before it submits.
    #[arg(long, value_name = "ID")]
    id: Option<String>,
    /// A file to append the step's outputs for GitHub Actions to, `job-id=`, `outcome=` and
    /// `exit-code=` lines: the file `$GITHUB_OUTPUT` names.
    #[arg(long, value_name = "FILE")]
    github_output: Option<PathBuf>,
}

/// The command group `ci`.
pub(crate) fn command() -> clap::Command {
    let run = RunArgs::augment_args(clap::Command::new("run").next_display_order(FLAG_ORDER))
        .about("Runs a CI step as a job: fetches its commit, runs its commands, brings back its outputs, and exits with its status.")
        .long_about(
            "Runs a CI step as a job: fetches its commit, runs its commands, brings back its outputs, and exits with its status. The job runs /bin/sh with a script that fetches the one commit of --source-url, runs --run with bash in the checkout, and then writes the paths of --outputs that exist to its stdout as a tar archive. js submits it (as js job run does: a lost answer is sent again with the same id), writes its stdout and stderr as they come, with the archive taken out of stdout and unpacked into --outputs-dir, and waits for its end; the first SIGINT, SIGTERM or SIGHUP cancels the job and waits, a second ends js. The job names no CPU or memory: the scheduler sizes it from the history of --step, or of --size-fallback while the step has fewer than five runs.",
        )
        .after_help(STATUSES);
    clap::Command::new("ci")
        .about("Runs CI steps as jobs")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(run)
}

/// The matches of `ci run`, when the command line names it.
pub(crate) fn chosen(matches: &ArgMatches) -> Option<&ArgMatches> {
    matches.subcommand_matches("ci")?.subcommand_matches("run")
}

/// `KEY=VALUE`, with a key that is not empty.
fn parse_env(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("{text:?} is not KEY=VALUE with a key")),
    }
}

/// Runs `ci run`: `globals` are the matches of the whole command line, `own` those of `ci
/// run`.
pub(crate) fn run(
    globals: &ArgMatches,
    own: &ArgMatches,
    render: Render,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: Services,
) -> Result<u8, Failure> {
    // clap has already checked `own` against these flags, so this does not fail; its error
    // is made up front.
    let args = RunArgs::from_arg_matches(own)
        .ok()
        .ok_or(Failure::Usage("not ci run".to_owned()))?;
    let declared = outputs_of(args.outputs.as_deref())?;
    let (submit, key) = job_key(args.id.as_deref())?;
    // The marker is the job id's, so the same command run again with the same --id (after a
    // lost answer) submits the same job.
    let marker = match declared.is_empty() {
        true => None,
        false => Some(format!("{MARKER_PREFIX}{}", key.1)),
    };
    let mut spec = spec(&args, &declared, marker.as_deref())?;
    spec.job_id = key.1.clone();
    let job = spec.job_id.clone();
    let connection = Connection::of(globals, &services.dialer)?;
    if args.id.is_none() {
        show(err, &render.drawn("job", &job)).map_err(|why| {
            Failure::Server(format!(
                "cannot print the drawn id, so nothing was sent: {why}"
            ))
        })?;
    }
    let signals = watch_signals(services.signals)?;
    let runner = Run {
        connection,
        submit,
        key: Some(key),
        job: job.clone(),
        waiter: &*services.job_waiter,
        render,
    };
    let sent = SubmitJobRequest { spec: Some(spec) };
    let mut demux = Demux::new(out, marker.as_deref());
    let ended = runtime()?.block_on(async {
        let outcome = runner.drive(sent, signals, &mut demux, err).await?;
        // A job that succeeded exited 0; only a failed one's code is read from its result.
        let code = match outcome {
            Outcome::Succeeded => Some(0),
            Outcome::Failed => runner.exit_code().await,
            _ => None,
        };
        Ok::<_, Failure>((outcome, code))
    });
    let frame = demux.finish();
    let (outcome, exit_code) = match ended {
        Ok(ended) => ended,
        Err(failure) => {
            let kind = failure.parts().1;
            github(&args, err, render, &outputs::github_lines(&job, kind, None));
            return Err(failure);
        }
    };
    let mut code = report(&job, outcome, render, err);
    let named = exit_code
        .and_then(|code| u8::try_from(code).ok())
        .filter(|code| *code != 0);
    if let (Outcome::Failed, Some(named)) = (outcome, named) {
        code = named;
    }
    let brought = bring_back(frame, marker.is_some(), args.outputs_dir.as_deref());
    if let Err(why) = brought {
        let _ = show(err, &render.failure(&Failure::Server(why)));
        if outcome == Outcome::Succeeded {
            code = exit::OUTPUTS;
        }
    }
    let name = crate::jobs::status(outcome).1;
    github(
        &args,
        err,
        render,
        &outputs::github_lines(&job, name, exit_code),
    );
    Ok(code)
}

/// Appends `lines` to the `--github-output` file, if one is named; a file that cannot be
/// written is said on `err` and does not change the status.
fn github(args: &RunArgs, err: &mut dyn Write, render: Render, lines: &str) {
    let Some(path) = args.github_output.as_deref() else {
        return;
    };
    if let Err(why) = outputs::append(path, lines) {
        let _ = show(err, &render.failure(&Failure::Server(why)));
    }
}

/// Unpacks the archive of `frame` into `dir`; with outputs declared (`wanted`), a stream
/// that held no frame or a broken one is an error.
fn bring_back(
    frame: std::io::Result<Frame>,
    wanted: bool,
    dir: Option<&Path>,
) -> Result<(), String> {
    let frame = frame.map_err(|why| format!("cannot write the job's stdout: {why}"))?;
    match (frame, dir) {
        (Frame::Read(archive), Some(dir)) => outputs::unpack(&archive, dir),
        (Frame::Broken(why), _) => Err(why),
        (Frame::Absent, _) if wanted => Err(
            "the job wrote no outputs: its script did not reach them (see the job's stderr)"
                .to_owned(),
        ),
        _ => Ok(()),
    }
}

/// The paths of `--outputs`: one per line, empty lines left out. Each must be relative to
/// the checkout, name a file in it (no `..`), and not begin with `-` (tar would read it as an
/// option).
fn outputs_of(text: Option<&str>) -> Result<Vec<String>, Failure> {
    let lines = text.unwrap_or_default().lines();
    let paths = lines.filter(|line| !line.trim().is_empty());
    paths
        .map(|path| {
            let inside = Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
            match inside && !path.starts_with('-') {
                true => Ok(path.to_owned()),
                false => Err(Failure::flag(
                    "--outputs",
                    format!("{path:?} is not a path inside the checkout: it must be relative, hold no `..` and not begin with -"),
                )),
            }
        })
        .collect()
}

/// The job of the flags, without its id.
fn spec(args: &RunArgs, declared: &[String], marker: Option<&str>) -> Result<JobSpec, Failure> {
    check_url(&args.source_url)?;
    check_commit(&args.source_commit)?;
    let (kind, os, arch, exec) = match args.os {
        Os::Macos => ("ci-macos", "macos", "arm64", ExecMode::SandboxExec),
        Os::Linux => ("ci-linux", "linux", "", ExecMode::Container),
    };
    let class = class_of("--step", &args.step)?;
    let fallback = class_of(
        "--size-fallback",
        args.size_fallback.as_deref().unwrap_or(kind),
    )?;
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), class),
        (SIZE_FALLBACK_LABEL.to_owned(), fallback),
    ]);
    let files = match &args.source_token_file {
        Some(path) => vec![JobFile {
            name: script::TOKEN_FILE.to_owned(),
            contents: source_token(path)?,
        }],
        None => Vec::new(),
    };
    let network = match args.network {
        Network::None => NetworkLevel::None,
        Network::AllowList => NetworkLevel::AllowList,
        Network::Open => NetworkLevel::Open,
    };
    let run = &args.run;
    let constraints = Constraints {
        os: os.to_owned(),
        arch: arch.to_owned(),
        exec: exec.into(),
        ..Constraints::default()
    };
    Ok(JobSpec {
        image: args.image.clone(),
        command: SHELL.to_owned(),
        args: script::args(run, &args.source_url, &args.source_commit, marker, declared),
        env: args.env.iter().cloned().collect(),
        constraints: Some(constraints),
        wall_limit_seconds: Some(args.wall_limit.as_secs()),
        network: network.into(),
        files,
        labels,
        ..JobSpec::default()
    })
}

/// `name` written as a size class, or the usage failure of `flag` for a name with no letter
/// or digit.
fn class_of(flag: &str, name: &str) -> Result<String, Failure> {
    let class = SizeClass::of_target(name).map(|class| class.as_str().to_owned());
    class.ok_or_else(|| Failure::flag(flag, format!("{name:?} holds no letter or digit")))
}

/// `--source-url` must be `https://` with a host and no user or password: the URL is in
/// every read of the job, so it carries no credential.
fn check_url(url: &str) -> Result<(), Failure> {
    let authority = url
        .strip_prefix("https://")
        .map(|rest| rest.split('/').next().unwrap_or_default());
    match authority {
        Some(host) if !host.is_empty() && !host.contains('@') => Ok(()),
        _ => Err(Failure::flag(
            "--source-url",
            format!("{url:?} is not an https:// URL with a host and no user or password"),
        )),
    }
}

/// `--source-commit` must be a full commit id: a fetch by commit needs all of it.
fn check_commit(commit: &str) -> Result<(), Failure> {
    let hex = commit
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    match hex && matches!(commit.len(), 40 | 64) {
        true => Ok(()),
        false => Err(Failure::flag(
            "--source-commit",
            format!("{commit:?} is not a full commit id: 40 or 64 lower-case hex digits"),
        )),
    }
}

/// The token in the file at `path`, read under the rules of `--token-file`.
fn source_token(path: &Path) -> Result<String, Failure> {
    match js_client::bearer(path) {
        Ok(bearer) => Ok(bearer.trim_start_matches("Bearer ").to_owned()),
        Err(_) => Err(Failure::flag(
            "--source-token-file",
            format!(
                "{} must be a regular file owned by you and readable only by you (mode 0600 or 0400) that holds one token of printable ASCII",
                path.display()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a job stdout that could not be written is an outputs failure that says
    /// so, whether or not outputs were declared, and that a stream with no frame is one only
    /// when outputs were declared. It catches a failed write of the step's log taken for
    /// outputs read back.
    #[test]
    fn a_stdout_that_could_not_be_written_is_an_outputs_failure() {
        let closed = Err(std::io::Error::other("closed"));
        assert_eq!(
            bring_back(closed, false, None),
            Err("cannot write the job's stdout: closed".to_owned())
        );
        assert_eq!(bring_back(Ok(Frame::Absent), false, None), Ok(()));
        assert!(bring_back(Ok(Frame::Absent), true, None).is_err());
    }
}
