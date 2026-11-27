//! The agent hand-over check (docs/runbooks/agent-handover-check.md): watches named hosts through
//! the job service while a js-agent release rolls onto them, and says whether each one handed
//! its running jobs to the new agent instead of draining. It is read-only: it runs only
//! `js host list`, `js job list` and `js job get`, and changes nothing.
//!
//! It runs as the program `js-handover-check` ([`main`]), which the build farm builds on its own,
//! and as the verb `cargo xtask agent-handover-check` (xtask's `handover`, this crate).
//!
//! It reads the hosts and the jobs running on them once, then reads the hosts every
//! `--interval-secs`, at most `--samples` times, until every named host lists an agent version
//! other than the one it listed first. Then it reads the running jobs again and asks the service
//! about each job that ran before and does not run now on the same host in the same attempt.
//!
//! A host passes when it moved to a new version, was never seen cordoned or tainted while it
//! moved (an update that drains cordons the host first; one that hands over does not), and every
//! job that ran on it before runs on it still in the same attempt or ended succeeded or
//! cancelled. A run in which every host passed but one ran no job before is inconclusive.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use std::process::ExitCode;

use clap::{Args as ClapArgs, Parser};
use serde::Deserialize;

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The job service's address, as `js --server` takes it.
    #[arg(long)]
    pub server: String,
    /// A client token file of the job service, as `js --token-file` takes it.
    #[arg(long)]
    pub token_file: PathBuf,
    /// The CA certificate the service's certificate must chain to, as `js --ca-cert` takes it.
    #[arg(long)]
    pub ca_cert: Option<PathBuf>,
    /// An existing directory (the workspace scratch directory) the readings are written to.
    #[arg(long)]
    pub out_dir: PathBuf,
    /// A host to watch, as `js host list` names it; give the flag once per host.
    #[arg(long = "host", required = true)]
    pub hosts: Vec<String>,
    /// The most readings of the hosts while the release rolls.
    #[arg(long, default_value_t = 360, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    pub samples: u32,
    /// Seconds between two readings of the hosts.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub interval_secs: u64,
    /// The `js` program.
    #[arg(long, default_value = "js")]
    pub js: PathBuf,
}

/// The program `js-handover-check`: the check's flags and nothing else.
#[derive(Debug, Parser)]
#[command(
    name = "js-handover-check",
    about = "Watch hosts, read-only, while a js-agent release rolls onto them, and fail one that did not hand its running jobs to the new agent"
)]
struct Cli {
    #[command(flatten)]
    args: Args,
}

/// The program `js-handover-check`: [`run`] with the flags given, its outcome reported by
/// [`report`].
#[must_use]
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    report(
        &run(&cli.args),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Reports `result`: a pass is its line on `out` and status 0; a usage error its reason on `err`
/// and status 2; any other end its reason on `err` and status 1. Each line on `err` begins with
/// `js-handover-check: `.
pub fn report(
    result: &Result<String, Error>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    // A stream that cannot be written leaves the status to say how the check ended.
    match result {
        Ok(line) => {
            let _ = writeln!(out, "{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(err, "js-handover-check: {e}");
            if matches!(e, Error::Usage(_)) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// Why the check did not pass.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Flags that do not fit together.
    #[error("{0}")]
    Usage(String),
    /// A reading the check needs failed.
    #[error("{what} cannot be read: {why}")]
    Read {
        /// What was read.
        what: String,
        /// Why it failed.
        why: String,
    },
    /// A named host is not listed.
    #[error("host {0} is not listed by the job service")]
    Unknown(String),
    /// A named host lists no agent version.
    #[error(
        "host {0} lists no agent version, so whether it moves cannot be told; read the hosts again once its agent has said hello"
    )]
    NoVersion(String),
    /// A named host is cordoned before the roll.
    #[error(
        "host {0} is cordoned already, so a drain could not be told from it; start the check once the cordon is gone"
    )]
    Cordoned(String),
    /// A file of `--out-dir` could not be written.
    #[error("{path} cannot be written: {why}")]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        why: String,
    },
    /// The verdict: the check found this many failures, each printed above.
    #[error("FAIL: {0} finding(s), each named above")]
    Failed(usize),
    /// Every host passed, but these ran no job before.
    #[error(
        "INCONCLUSIVE: every host moved and lost nothing, but {0} ran no job before, so its hand-over of running jobs was not shown"
    )]
    Inconclusive(String),
}

/// One call of `js` with the arguments given: its stdout, or why it failed.
pub type Js<'a> = dyn FnMut(&[String]) -> Result<String, String> + 'a;

/// Runs `program` with `args` and no stdin: its stdout when it exits 0.
///
/// # Errors
/// Why it could not be run, or its exit status and stderr.
pub fn run_js(program: &Path, args: &[String]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{} cannot be run: {e}", program.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} ended with {}: {}",
            program.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The verb: `js` is `--js`, the clock the system's and sleeping real.
///
/// # Errors
/// [`Error`], for every end but a pass.
pub fn run(args: &Args) -> Result<String, Error> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let program = args.js.clone();
    let mut js = move |a: &[String]| run_js(&program, a);
    check(
        args,
        &mut js,
        &mut std::thread::sleep,
        u64::try_from(now_ms).unwrap_or(u64::MAX),
        &mut std::io::stdout().lock(),
    )
}

/// A host as `js host list --json` prints it (proto3 JSON: a default field is left out).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Host {
    id: String,
    state: String,
    agent_version: String,
    cordon_expires_at_ms: Option<serde_json::Value>,
    mark: String,
    taint_reason: String,
}

/// The answer of `js host list --json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Hosts {
    hosts: Vec<Host>,
}

/// A job's times.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Times {
    ended_at_ms: Option<serde_json::Value>,
}

/// A job as `js job list --json` and `js job get --json` print it.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Job {
    id: String,
    host_id: String,
    state: String,
    attempt: u32,
    times: Times,
}

impl Job {
    /// The job's attempt: 0, a job recorded before attempts were counted, is 1.
    fn attempt(&self) -> u32 {
        self.attempt.max(1)
    }

    /// When the job ended, in milliseconds (a uint64, which proto3 JSON prints as a string); 0
    /// when it has not.
    fn ended_ms(&self) -> u64 {
        match &self.times.ended_at_ms {
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
            Some(v) => v.as_u64().unwrap_or(0),
            None => 0,
        }
    }

    /// The job as one line of evidence.
    fn line(&self) -> String {
        serde_json::json!({"id": self.id, "host": self.host_id, "state": self.state}).to_string()
    }
}

/// The answer of `js job list --json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Jobs {
    jobs: Vec<Job>,
    next_page_token: String,
}

/// The answer of `js job get --json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GotJob {
    job: Option<Job>,
}

/// What the check saw of one host while the release rolled.
#[derive(Debug, Default)]
struct Seen {
    /// The version it listed first.
    before: String,
    /// What it was last listed as: a version, `-` (none) or `absent`; empty until read.
    last: String,
    /// The first reading in which it was cordoned.
    cordoned: Option<u32>,
    /// The first reading in which it was tainted, with the reason.
    tainted: Option<(u32, String)>,
    /// The marks it showed.
    marks: BTreeSet<String>,
    /// The readings in which it was not heard as live.
    unheard: u32,
}

impl Seen {
    /// Whether it is listed now with a version other than its first.
    fn moved(&self) -> bool {
        !["", "-", "absent"].contains(&self.last.as_str()) && self.last != self.before
    }
}

/// `text` or `-` when it is empty.
fn or_dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

/// The calls of the check: `js` with the base flags.
struct Reader<'a, 'b> {
    js: &'a mut Js<'b>,
    flags: Vec<String>,
    names: &'a [String],
}

impl Reader<'_, '_> {
    /// `js <flags> <verb...>`.
    fn call(&mut self, verb: &[&str]) -> Result<String, String> {
        let mut args = self.flags.clone();
        args.extend(verb.iter().map(|s| (*s).to_owned()));
        (self.js)(&args)
    }

    /// `js host list`, parsed.
    fn hosts(&mut self) -> Result<(String, Hosts), String> {
        let text = self.call(&["host", "list"])?;
        let hosts = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok((text, hosts))
    }

    /// Every job in one of `states` on a named host, every page.
    fn jobs_in(&mut self, states: &[&str]) -> Result<Vec<Job>, Error> {
        let what = format!("the jobs {}", states.join(", "));
        let mut found = Vec::new();
        let mut token = String::new();
        loop {
            let mut verb = vec!["job", "list"];
            for state in states {
                verb.extend(["--states", *state]);
            }
            verb.extend(["--page-size", "500"]);
            if !token.is_empty() {
                verb.extend(["--page-token", token.as_str()]);
            }
            let read = |why: String| Error::Read {
                what: what.clone(),
                why,
            };
            let text = self.call(&verb).map_err(read)?;
            let page: Jobs = serde_json::from_str(&text).map_err(|e| read(e.to_string()))?;
            found.extend(
                page.jobs
                    .into_iter()
                    .filter(|j| self.names.contains(&j.host_id)),
            );
            if page.next_page_token.is_empty() {
                return Ok(found);
            }
            token = page.next_page_token;
        }
    }

    /// What became of `job`, which ran before and does not run now as it did.
    fn outcome(&mut self, job: &Job) -> String {
        let got = self
            .call(&["job", "get", "--job-id", job.id.as_str()])
            .ok()
            .and_then(|t| serde_json::from_str::<GotJob>(&t).ok())
            .and_then(|g| g.job);
        let Some(now) = got else {
            return "unreadable".to_owned();
        };
        match now.state.as_str() {
            "JOB_STATE_SUCCEEDED" | "JOB_STATE_CANCELLED" => "ended".to_owned(),
            "JOB_STATE_LOST" => "lost".to_owned(),
            "JOB_STATE_FAILED" => "failed".to_owned(),
            "JOB_STATE_FAILED_TO_START" => "failed-to-start".to_owned(),
            _ if now.attempt() != job.attempt() || now.host_id != job.host_id => {
                "started-again".to_owned()
            }
            state => format!("not-running:{}", or_dash(state)),
        }
    }
}

/// Writes `text` to `name` in `dir`.
fn put(dir: &Path, name: &str, text: &str) -> Result<(), Error> {
    let path = dir.join(name);
    std::fs::write(&path, text).map_err(|e| Error::Write {
        path,
        why: e.to_string(),
    })
}

/// The ids of `jobs` on `host`, sorted and space-separated, `none` when there are none.
fn ids(jobs: &[Job], host: &str) -> String {
    let set: BTreeSet<&str> = jobs
        .iter()
        .filter(|j| j.host_id == host)
        .map(|j| j.id.as_str())
        .collect();
    if set.is_empty() {
        "none".to_owned()
    } else {
        set.into_iter().collect::<Vec<_>>().join(" ")
    }
}

/// Whether `host` is a host id: 1 to 253 of ASCII letters, digits, `_`, `.`, `:` and `-`.
fn host_id_ok(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

/// Writes one line to `out`.
fn say(out: &mut dyn Write, line: &str) -> Result<(), Error> {
    writeln!(out, "{line}").map_err(|e| Error::Write {
        path: PathBuf::from("stdout"),
        why: e.to_string(),
    })
}

/// The check, with `js` called through `js`, `sleep` between readings and `now_ms` the time it
/// began: each finding on `out`, the PASS line returned.
///
/// # Errors
/// [`Error`]: a refusal before the watch, a reading after it that failed, the FAIL verdict, or
/// an inconclusive one.
pub fn check(
    args: &Args,
    js: &mut Js<'_>,
    sleep: &mut dyn FnMut(Duration),
    now_ms: u64,
    out: &mut dyn Write,
) -> Result<String, Error> {
    if let Some(bad) = args.hosts.iter().find(|h| !host_id_ok(h)) {
        return Err(Error::Usage(format!("--host {bad} is not a host id")));
    }
    let mut unique = BTreeSet::new();
    if let Some(twice) = args.hosts.iter().find(|h| !unique.insert(h.as_str())) {
        return Err(Error::Usage(format!("--host {twice} is given twice")));
    }
    if !args.out_dir.is_dir() {
        return Err(Error::Usage(format!(
            "--out-dir {} is not a directory",
            args.out_dir.display()
        )));
    }
    let mut flags = vec![
        "--server".to_owned(),
        args.server.clone(),
        "--token-file".to_owned(),
        args.token_file.display().to_string(),
        "--json".to_owned(),
    ];
    if let Some(ca) = &args.ca_cert {
        flags.extend(["--ca-cert".to_owned(), ca.display().to_string()]);
    }
    let mut reader = Reader {
        js,
        flags,
        names: &args.hosts,
    };
    let dir = &args.out_dir;

    let (text, listed) = reader.hosts().map_err(|why| Error::Read {
        what: "the hosts".to_owned(),
        why,
    })?;
    put(dir, "hosts-before.json", &text)?;
    let before = reader.jobs_in(&["running"])?;
    let mut seen = Vec::new();
    for name in &args.hosts {
        let host = listed
            .hosts
            .iter()
            .find(|h| &h.id == name)
            .ok_or_else(|| Error::Unknown(name.clone()))?;
        if host.agent_version.is_empty() {
            return Err(Error::NoVersion(name.clone()));
        }
        if host.cordon_expires_at_ms.is_some() {
            return Err(Error::Cordoned(name.clone()));
        }
        say(
            out,
            &format!(
                "{name}: agent {}; running before: {}",
                host.agent_version,
                ids(&before, name)
            ),
        )?;
        seen.push(Seen {
            before: host.agent_version.clone(),
            ..Seen::default()
        });
    }

    let mut timeline = String::from("reading\thost\tversion\tcordon\tmark\tstate\ttaint\n");
    let mut readings = 0;
    for reading in 1..=args.samples {
        readings = reading;
        match reader.hosts() {
            Err(_) => {
                let _ = writeln!(timeline, "{reading}\t*\tunreadable\t-\t-\t-\t-");
            }
            Ok((_, now)) => {
                for (name, s) in args.hosts.iter().zip(seen.iter_mut()) {
                    let Some(h) = now.hosts.iter().find(|h| &h.id == name) else {
                        s.last = "absent".to_owned();
                        s.unheard += 1;
                        let _ = writeln!(timeline, "{reading}\t{name}\tabsent");
                        continue;
                    };
                    s.last = or_dash(&h.agent_version).to_owned();
                    let cordon = if h.cordon_expires_at_ms.is_some() {
                        s.cordoned = s.cordoned.or(Some(reading));
                        "cordoned"
                    } else {
                        "-"
                    };
                    let mark = if h.mark.is_empty() {
                        "HOST_MARK_UNSPECIFIED"
                    } else {
                        h.mark.as_str()
                    };
                    if mark != "HOST_MARK_UNSPECIFIED" {
                        s.marks.insert(mark.to_owned());
                    }
                    if h.state != "HOST_STATE_LIVE" {
                        s.unheard += 1;
                    }
                    if !h.taint_reason.is_empty() && s.tainted.is_none() {
                        s.tainted = Some((reading, h.taint_reason.clone()));
                    }
                    let _ = writeln!(
                        timeline,
                        "{reading}\t{name}\t{}\t{cordon}\t{mark}\t{}\t{}",
                        s.last,
                        or_dash(&h.state),
                        or_dash(&h.taint_reason)
                    );
                }
                if seen.iter().all(Seen::moved) {
                    break;
                }
            }
        }
        if reading < args.samples {
            sleep(Duration::from_secs(args.interval_secs));
        }
    }
    put(dir, "timeline.tsv", &timeline)?;
    say(
        out,
        &format!(
            "readings of the hosts: {readings} (in {})",
            dir.join("timeline.tsv").display()
        ),
    )?;

    let after = reader.jobs_in(&["running"])?;
    let mut outcomes = Vec::new();
    for job in &before {
        let kept = after
            .iter()
            .any(|a| a.id == job.id && a.host_id == job.host_id && a.attempt() == job.attempt());
        let outcome = if kept {
            "kept".to_owned()
        } else {
            reader.outcome(job)
        };
        outcomes.push((job.host_id.clone(), job.id.clone(), outcome));
    }
    let tsv: String = outcomes
        .iter()
        .map(|(h, id, o)| format!("{h}\t{id}\t{o}\n"))
        .collect();
    put(dir, "outcomes.tsv", &tsv)?;
    let badly: Vec<Job> = reader
        .jobs_in(&["lost", "failed", "failed-to-start"])?
        .into_iter()
        .filter(|j| j.ended_ms() >= now_ms)
        .collect();
    let lines: String = badly.iter().map(|j| j.line() + "\n").collect();
    put(dir, "jobs-ended-badly.jsonl", &lines)?;

    let mut failures = 0;
    let mut idle = Vec::new();
    for (name, s) in args.hosts.iter().zip(&seen) {
        let mut fail = |out: &mut dyn Write, why: String| {
            failures += 1;
            say(out, &format!("{name}: FAIL: {why}"))
        };
        if s.moved() {
            say(out, &format!("{name}: moved {} -> {}", s.before, s.last))?;
        } else {
            let last = if s.last.is_empty() {
                "unread"
            } else {
                s.last.as_str()
            };
            fail(
                &mut *out,
                format!("did not move from {} (last listed: {last})", s.before),
            )?;
        }
        if let Some(r) = s.cordoned {
            fail(
                &mut *out,
                format!("cordoned at reading {r}: it drained instead of handing over"),
            )?;
        }
        if let Some((r, why)) = &s.tainted {
            fail(&mut *out, format!("tainted at reading {r} ({why})"))?;
        }
        if !s.marks.is_empty() {
            let marks: Vec<&str> = s.marks.iter().map(String::as_str).collect();
            say(out, &format!("{name}: marks seen: {}", marks.join(" ")))?;
        }
        if s.unheard > 0 {
            say(
                out,
                &format!("{name}: not heard as live in {} reading(s)", s.unheard),
            )?;
        }
        let mine: Vec<&(String, String, String)> =
            outcomes.iter().filter(|(h, _, _)| h == name).collect();
        let count = |o: &str| mine.iter().filter(|(_, _, x)| x == o).count();
        say(
            out,
            &format!(
                "{name}: running before: {}; kept: {}; ended succeeded or cancelled: {}; running after: {}",
                mine.len(),
                count("kept"),
                count("ended"),
                ids(&after, name)
            ),
        )?;
        if mine.is_empty() {
            idle.push(name.as_str());
        }
        for (_, id, outcome) in mine.iter().filter(|(_, _, o)| o != "kept" && o != "ended") {
            fail(&mut *out, format!("job {id} {outcome}"))?;
        }
    }
    if !badly.is_empty() {
        failures += 1;
        say(
            out,
            "FAIL: jobs of these hosts that ended lost, failed or failed to start since the check began:",
        )?;
        for j in &badly {
            say(out, &format!("  {}", j.line()))?;
        }
    }
    if failures > 0 {
        return Err(Error::Failed(failures));
    }
    if !idle.is_empty() {
        return Err(Error::Inconclusive(idle.join(", ")));
    }
    Ok(
        "PASS: every host moved and handed its running jobs over: none cordoned, tainted, lost, failed or started again"
            .to_owned(),
    )
}
