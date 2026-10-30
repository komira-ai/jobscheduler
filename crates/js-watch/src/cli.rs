//! The command line: `run` watches, `abort` stops everything of one scheduler.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use js_alert::{AlertSink, DELIVERY_LIMIT, WebhookSink};
use js_elastic::SchedulerId;
use js_provider_lambdalabs::kind::{API_TIMEOUT, DEFAULT_API_URL};
use js_provider_lambdalabs::{ApiKey, HttpApi, LambdaRead, Reader, SystemClock};

use crate::abort::{LambdaTarget, Target, abort};
use crate::config::{Config, LambdaTable};
use crate::status::{HttpStatus, StatusSource};
use crate::view::{CloudView, LambdaView};
use crate::watcher::Watcher;

/// The command line.
#[derive(Debug, Parser)]
#[command(
    name = "js-watch",
    version,
    about = "Watches a scheduler's cloud from outside the lease server, and aborts it"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compares the cloud with the lease server's status and alerts. Read-only.
    Run {
        /// The configuration file.
        #[arg(long)]
        config: PathBuf,
        /// Stop after this many ticks (a cron job uses 1); without it, run until killed.
        #[arg(long)]
        ticks: Option<u64>,
    },
    /// Terminates everything that carries one scheduler's marks. A dry run unless
    /// `--apply` is given.
    Abort {
        /// The configuration file (its `[lambda]` table holds the credentials).
        #[arg(long)]
        config: PathBuf,
        /// The cloud.
        #[arg(long, value_enum)]
        cloud: CloudName,
        /// The id of the scheduler whose instances are stopped.
        #[arg(long)]
        scheduler_id: String,
        /// Really stop them. Without it nothing is stopped.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CloudName {
    Lambda,
}

/// The program: runs with the process's arguments and standard streams.
pub fn main() -> ExitCode {
    run(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &std::thread::sleep,
    )
}

/// Runs the command line `args` (the program name first). Output goes to `out`, a
/// refusal or failure to `err`; `sleep` is how the `run` command waits between ticks.
///
/// Exit status: 0 on success (and for `--help`), 2 for a usage error or a refused
/// configuration, 1 when an abort stopped nothing it should have, or its plan failed.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    sleep: &dyn Fn(Duration),
) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(usage) => {
            let _ = match usage.use_stderr() {
                true => write!(err, "{usage}"),
                false => write!(out, "{usage}"),
            };
            return ExitCode::from(u8::try_from(usage.exit_code()).unwrap_or(2));
        }
    };
    let result = match cli.command {
        Command::Run { config, ticks } => watch(&config, ticks, sleep),
        Command::Abort {
            config,
            cloud: CloudName::Lambda,
            scheduler_id,
            apply,
        } => stop_all(&config, &scheduler_id, apply, out),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            let _ = writeln!(err, "js-watch: {message}");
            ExitCode::from(code)
        }
    }
}

/// An exit code and what to say.
type Failure = (u8, String);

fn refused(why: impl std::fmt::Display) -> Failure {
    (2, why.to_string())
}

/// The Lambda API client the table describes.
fn lambda_api(table: &LambdaTable) -> Result<HttpApi, Failure> {
    let key = ApiKey::from_file(&table.api_key_file).map_err(refused)?;
    let url = table.api_url.as_deref().unwrap_or(DEFAULT_API_URL);
    HttpApi::connect(url, &key, None, API_TIMEOUT).map_err(refused)
}

/// The webhook sink the configuration names.
fn sink(config: &Config) -> Result<WebhookSink, Failure> {
    let alerts = &config.alerts;
    let authorization = match &alerts.webhook_token_file {
        Some(path) => Some(read_token(path)?),
        None => None,
    };
    WebhookSink::new(
        &alerts.webhook_url,
        authorization,
        "js-watch",
        DELIVERY_LIMIT,
    )
    .map_err(refused)
}

fn read_token(path: &Path) -> Result<String, Failure> {
    let key = ApiKey::from_file(path).map_err(refused)?;
    Ok(key.authorization())
}

/// `run`: builds the watcher from the file and drives it.
fn watch(config: &Path, ticks: Option<u64>, sleep: &dyn Fn(Duration)) -> Result<(), Failure> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let config = Config::load(config).map_err(refused)?;
    let scheduler = SchedulerId::new(&config.scheduler_id).map_err(refused)?;
    let api = Arc::new(lambda_api(config.lambda().map_err(refused)?)?);
    // From here the watcher holds a reader of the API: the client itself goes no further.
    let reads: Arc<dyn LambdaRead> = api;
    let cloud: Arc<dyn CloudView> = Arc::new(LambdaView::new(Reader::new(reads, scheduler)));
    let status = HttpStatus::new(&config.status_url, API_TIMEOUT).map_err(refused)?;
    let sink: Arc<dyn AlertSink> = Arc::new(sink(&config)?);
    let status: Arc<dyn StatusSource> = Arc::new(status);
    let mut watcher = Watcher::new((cloud, status), sink, config.settings());
    let origin = Instant::now();
    let interval = config.interval();
    let mut left = ticks;
    watcher.drive(
        || origin.elapsed(),
        || {
            left = left.map(|n| n.saturating_sub(1));
            if left == Some(0) {
                return false;
            }
            sleep(interval);
            true
        },
    );
    Ok(())
}

/// `abort`: builds the target from the file and prints what the abort did.
fn stop_all(
    config: &Path,
    scheduler: &str,
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let config = Config::load(config).map_err(refused)?;
    let id = SchedulerId::new(scheduler).map_err(refused)?;
    let api = Arc::new(lambda_api(config.lambda().map_err(refused)?)?);
    let target = LambdaTarget::new(api, id, Arc::new(SystemClock::new()));
    report(&target, scheduler, apply, out)
}

/// Runs the abort on `target` and prints it.
fn report(
    target: &dyn Target,
    scheduler: &str,
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let outcome = abort(target, scheduler, apply).map_err(|why| (1, why.to_string()))?;
    for line in &outcome.lines {
        let _ = writeln!(out, "{line}");
    }
    match outcome.failed {
        0 => Ok(()),
        count => Err((
            1,
            format!("{count} instance(s) were not stopped; run it again"),
        )),
    }
}
