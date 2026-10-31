//! The command line.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use js_client::ClientError;

use crate::clock::{Clock, SystemClock};
use crate::exec::{CommandSetter, Experiment, ExperimentError, NoSetter, Setter, run_plan};
use crate::fake::{FakeClock, FakeFarm};
use crate::farm::{Farm, HostShape};
use crate::http::HttpFarm;
use crate::plan::{Plan, PlanError};
use crate::summary::{Listing, ReadError, Summarise, group, read_records};
use crate::workload::{Workload, WorkloadError};

/// The command line.
#[derive(Debug, Parser)]
#[command(
    name = "js-measure",
    version,
    about = "The jobscheduler measurement harness"
)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Prints the order a plan runs in.
    Schedule {
        /// The plan file (JSON).
        #[arg(long)]
        plan: PathBuf,
    },
    /// Runs a plan and appends its records to `records.jsonl`, `jobs.jsonl` and
    /// `minutes.jsonl` in `--out`.
    Run {
        /// The plan file (JSON).
        #[arg(long)]
        plan: PathBuf,
        /// The workload file (JSON).
        #[arg(long)]
        workload: PathBuf,
        /// The directory the three files go in; made if it is missing.
        #[arg(long)]
        out: PathBuf,
        /// The code commit under test; written to every record.
        #[arg(long)]
        commit: String,
        /// Milliseconds between polls of the farm.
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
        /// Seconds after which a run's open jobs are given up.
        #[arg(long, default_value_t = 3600)]
        deadline_secs: u64,
        /// A program run as `<program> <parameter> <setting> <arm> control|treatment` before each run.
        #[arg(long)]
        apply_command: Option<String>,
        /// Run against a lease server (needs `--server` and `--token-file`); without it
        /// the run is against a simulated farm and measures nothing real.
        #[arg(long, requires_all = ["server", "token_file"])]
        live: bool,
        /// The lease server, `https://host:port` (`http://` on the loopback only).
        #[arg(long, requires = "live")]
        server: Option<String>,
        /// The owner-only file holding the client token.
        #[arg(long, requires = "live")]
        token_file: Option<PathBuf>,
        /// A PEM file to trust for `https://`.
        #[arg(long, requires = "live")]
        ca_cert: Option<PathBuf>,
        /// Hosts of the simulated farm.
        #[arg(long, default_value_t = 2)]
        fake_hosts: usize,
    },
    /// Groups a records file by setting and arm and prints the groups.
    Summary {
        /// A `records.jsonl`.
        #[arg(long)]
        records: PathBuf,
    },
}

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A file could not be read or written.
    #[error("{path}: {why}")]
    File {
        /// The path.
        path: String,
        /// What went wrong.
        why: String,
    },
    /// The plan is not usable.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// The workload is not usable.
    #[error(transparent)]
    Workload(#[from] WorkloadError),
    /// The plan stopped.
    #[error(transparent)]
    Experiment(#[from] ExperimentError),
    /// The records file has a bad line.
    #[error(transparent)]
    Records(#[from] ReadError),
    /// A client flag is unusable.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// A flag is out of range.
    #[error("{0}")]
    Usage(String),
    /// The output could not be written.
    #[error("cannot write the output: {0}")]
    Output(#[from] io::Error),
}

fn file_error(path: &Path, why: impl ToString) -> CliError {
    CliError::File {
        path: path.display().to_string(),
        why: why.to_string(),
    }
}

fn read_text(path: &Path) -> Result<String, CliError> {
    fs::read_to_string(path).map_err(|err| file_error(path, err))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    serde_json::from_str(&read_text(path)?).map_err(|err| file_error(path, err))
}

/// Runs the command line: parses the arguments, prints results to standard output and a
/// failure to standard error.
#[must_use]
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli.command, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("js-measure: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one command, writing its results to `out`.
///
/// # Errors
///
/// [`CliError`]: the command's failure.
pub fn execute(command: Command, out: &mut dyn Write) -> Result<(), CliError> {
    match command {
        Command::Schedule { plan } => schedule(&plan, out),
        Command::Summary { records } => summary(&records, out),
        Command::Run {
            plan,
            workload,
            out: dir,
            commit,
            poll_ms,
            deadline_secs,
            apply_command,
            live,
            server,
            token_file,
            ca_cert,
            fake_hosts,
        } => {
            let deadline_ms = deadline_secs.checked_mul(1000).ok_or_else(|| {
                CliError::Usage(format!("--deadline-secs {deadline_secs} is too large"))
            })?;
            let job = RunArgs {
                plan: read_json(&plan)?,
                workload: read_json(&workload)?,
                dir,
                commit,
                poll_ms,
                deadline_ms,
            };
            let setter: Box<dyn Setter> = match apply_command {
                Some(program) => Box::new(CommandSetter { program }),
                None => Box::new(NoSetter),
            };
            match (live, server, token_file) {
                (true, Some(server), Some(token)) => {
                    let farm = HttpFarm::connect(&server, &token, ca_cert.as_deref())?;
                    job.run(&farm, &SystemClock, setter.as_ref(), out)
                }
                _ => {
                    let clock = FakeClock::at(600_000);
                    let shape = HostShape {
                        cpu_millis: 64_000,
                        memory_bytes: 256 << 30,
                    };
                    let hosts = (1..=fake_hosts).map(|n| (format!("fake-{n}"), shape));
                    let farm = FakeFarm::new(&clock, hosts.collect());
                    job.run(&farm, &clock, setter.as_ref(), out)
                }
            }
        }
    }
}

fn schedule(plan: &Path, out: &mut dyn Write) -> Result<(), CliError> {
    let plan: Plan = read_json(plan)?;
    for slot in plan.schedule()? {
        let kind = if slot.warmup { "warmup" } else { "run" };
        let (setting, arm) = (&plan.settings[slot.setting], &plan.arms[slot.arm].label);
        writeln!(out, "{} {kind} {setting} {arm}", slot.order)?;
    }
    Ok(())
}

fn summary(records: &Path, out: &mut dyn Write) -> Result<(), CliError> {
    let groups = group(read_records(&read_text(records)?)?);
    out.write_all(Listing.summarise(&groups).as_bytes())?;
    Ok(())
}

/// What `run` was asked.
struct RunArgs {
    plan: Plan,
    workload: Workload,
    dir: PathBuf,
    commit: String,
    poll_ms: u64,
    deadline_ms: u64,
}

impl RunArgs {
    fn run(
        &self,
        farm: &dyn Farm,
        clock: &dyn Clock,
        setter: &dyn Setter,
        out: &mut dyn Write,
    ) -> Result<(), CliError> {
        let jobs = self.workload.expand()?;
        let taken = self.dir.join("records.jsonl");
        if fs::metadata(&taken).is_ok_and(|meta| meta.len() > 0) {
            let why = "already holds records; a second run in it would repeat run_index, give --out a new directory";
            return Err(file_error(&taken, why));
        }
        fs::create_dir_all(&self.dir).map_err(|err| file_error(&self.dir, err))?;
        let open = |name: &str| -> Result<File, CliError> {
            let path = self.dir.join(name);
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            options.open(&path).map_err(|err| file_error(&path, err))
        };
        let (mut records, mut rows, mut minutes) = (
            open("records.jsonl")?,
            open("jobs.jsonl")?,
            open("minutes.jsonl")?,
        );
        let exp = Experiment {
            plan: &self.plan,
            jobs: &jobs,
            farm,
            clock,
            setter,
            commit: &self.commit,
            poll_ms: self.poll_ms,
            deadline_ms: self.deadline_ms,
        };
        let runs = run_plan(&exp, &mut |run| {
            write_lines(&mut records, std::slice::from_ref(&run.record))?;
            write_lines(&mut rows, &run.jobs)?;
            write_lines(&mut minutes, &run.minutes)
        })?;
        writeln!(out, "{runs} runs recorded in {}", self.dir.display())?;
        Ok(())
    }
}

fn write_lines<T: serde::Serialize>(file: &mut File, rows: &[T]) -> io::Result<()> {
    for row in rows {
        serde_json::to_writer(&mut *file, row)?;
        file.write_all(b"\n")?;
    }
    file.flush()
}
