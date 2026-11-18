//! The segments of an action's start, read from the agent's `Started` journal lines.
//!
//! The agent logs one line per start, `js-agent: job: Started { run_id: .., timeline:
//! StartTimeline { .. } }`, after whatever prefix the journal adds. [`Timelines::add_text`]
//! reads such lines from a text and skips every other line. A `Started` line that does not
//! parse is counted and reported as malformed. The fields `placed_at_ms` and `received_at_ms`
//! are optional, because the lines of older agents do not have them.
//!
//! Each start gives these segments, in this order, each a signed difference in milliseconds:
//!
//! - `placed->received`: `received_at_ms - placed_at_ms`, when both are present. It spans the
//!   server's clock and the agent's clock, so a skew between the hosts shows in it.
//! - `received->X` for each lifecycle step `X`: the step's value, the time after the reply
//!   arrived.
//! - `A->B` for each pair of consecutive lifecycle steps, as they stand in the line.
//! - `A->B` for each pair of consecutive runtime steps, as they stand in the line. These are
//!   Unix times of one agent.
//! - `received->running`: the runtime step `running` minus `received_at_ms`, one agent clock.
//! - `placed->running`: the runtime step `running` minus `placed_at_ms`, two clocks.
//!
//! The rows of the table are the segments in the order they first appear in the input. A
//! percentile is the nearest-rank one: the p-th percentile of `n` sorted samples is the
//! sample at rank `ceil(p / 100 * n)`, counting from 1. Differences keep their sign.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::error::Error;

/// The text that opens the part of a line this module reads.
const MARKER: &str = "job: Started { run_id: ";

/// The samples of every segment, in the order the segments first appeared, and the counts of
/// the lines read.
#[derive(Clone, Debug, Default)]
pub struct Timelines {
    /// The index of each segment in `rows`.
    index: BTreeMap<String, usize>,
    /// Each segment with its samples, in order of first appearance.
    rows: Vec<(String, Vec<i64>)>,
    /// The `Started` lines that parsed.
    starts: usize,
    /// The `Started` lines that did not.
    malformed: usize,
}

/// One start as the line gives it.
struct Start {
    /// The server's Unix milliseconds when it placed the job.
    placed_at: Option<i64>,
    /// The agent's Unix milliseconds when the reply carrying the start arrived.
    received_at: Option<i64>,
    /// The lifecycle steps, each in milliseconds after the reply arrived.
    steps: Vec<(String, i64)>,
    /// The runtime steps, each at Unix milliseconds.
    runtime: Vec<(String, i64)>,
}

/// A position in the text of a line.
struct Cursor<'a>(&'a str);

impl<'a> Cursor<'a> {
    /// Moves past `text` if the rest starts with it.
    fn eat(&mut self, text: &str) -> Option<()> {
        self.0 = self.0.strip_prefix(text)?;
        Some(())
    }

    /// Reads the digits that follow as a number.
    fn number(&mut self) -> Option<i64> {
        let end = self
            .0
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(self.0.len());
        let (digits, rest) = self.0.split_at(end);
        let value = digits.parse().ok()?;
        self.0 = rest;
        Some(value)
    }

    /// Reads the text up to the next double quote, and moves past that quote.
    fn quoted(&mut self) -> Option<&'a str> {
        let (text, rest) = self.0.split_once('"')?;
        self.0 = rest;
        Some(text)
    }

    /// Reads `name: None, ` or `name: Some(N), `. An absent field, as in the line of an older
    /// agent, is `None` too and moves nothing.
    fn optional(&mut self, name: &str) -> Option<Option<i64>> {
        if self.eat(name).is_none() {
            return Some(None);
        }
        let value = if self.eat("None").is_some() {
            None
        } else {
            self.eat("Some(")?;
            let value = self.number()?;
            self.eat(")")?;
            Some(value)
        };
        self.eat(", ")?;
        Some(value)
    }

    /// Reads a list of `("name", N)` pairs.
    fn pairs(&mut self) -> Option<Vec<(String, i64)>> {
        self.eat("[")?;
        let mut pairs: Vec<(String, i64)> = Vec::new();
        while self.eat("]").is_none() {
            if !pairs.is_empty() {
                self.eat(", ")?;
            }
            self.eat("(\"")?;
            let name = self.quoted()?.to_owned();
            self.eat(", ")?;
            let value = self.number()?;
            self.eat(")")?;
            pairs.push((name, value));
        }
        Some(pairs)
    }
}

/// Reads the part of a `Started` line that follows the marker.
fn parse_start(rest: &str) -> Option<Start> {
    let mut cursor = Cursor(rest);
    cursor.eat("\"")?;
    cursor.quoted()?;
    cursor.eat(", timeline: StartTimeline { received_ms: ")?;
    cursor.number()?;
    cursor.eat(", ")?;
    let placed_at = cursor.optional("placed_at_ms: ")?;
    let received_at = cursor.optional("received_at_ms: ")?;
    cursor.eat("steps: ")?;
    let steps = cursor.pairs()?;
    cursor.eat(", runtime: ")?;
    let runtime = cursor.pairs()?;
    cursor.eat(" } }")?;
    Some(Start {
        placed_at,
        received_at,
        steps,
        runtime,
    })
}

/// The p-th percentile of `sorted` (ascending, not empty) by nearest rank: the sample at rank
/// `ceil(p / 100 * n)` counting from 1, for `p` from 1 to 100.
///
/// # Panics
/// If `sorted` is empty.
#[must_use]
pub fn nearest_rank(sorted: &[i64], p: usize) -> i64 {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

impl Timelines {
    /// An empty collection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of `Started` lines that parsed.
    #[must_use]
    pub fn starts(&self) -> usize {
        self.starts
    }

    /// The number of `Started` lines that did not parse.
    #[must_use]
    pub fn malformed(&self) -> usize {
        self.malformed
    }

    /// Reads every line of `text`: a `Started` line adds its segments, a line that is not one
    /// is skipped, and a `Started` line that does not parse is counted as malformed.
    pub fn add_text(&mut self, text: &str) {
        for line in text.lines() {
            let Some(at) = line.find(MARKER) else {
                continue;
            };
            match parse_start(&line[at + MARKER.len()..]) {
                Some(start) => {
                    self.starts += 1;
                    self.add_start(&start);
                }
                None => self.malformed += 1,
            }
        }
    }

    /// Adds one sample to the segment called `name`.
    fn sample(&mut self, name: String, value: i64) {
        if let Some(&at) = self.index.get(&name) {
            self.rows[at].1.push(value);
        } else {
            self.index.insert(name.clone(), self.rows.len());
            self.rows.push((name, vec![value]));
        }
    }

    /// Adds the segments of one start.
    fn add_start(&mut self, start: &Start) {
        if let (Some(placed), Some(received)) = (start.placed_at, start.received_at) {
            self.sample(
                "placed->received".to_owned(),
                received.saturating_sub(placed),
            );
        }
        for (name, value) in &start.steps {
            self.sample(format!("received->{name}"), *value);
        }
        for pair in start.steps.windows(2).chain(start.runtime.windows(2)) {
            let ((a, from), (b, to)) = (&pair[0], &pair[1]);
            self.sample(format!("{a}->{b}"), to.saturating_sub(*from));
        }
        let running = start.runtime.iter().find(|(name, _)| name == "running");
        if let Some(&(_, at)) = running {
            if let Some(received) = start.received_at {
                self.sample("received->running".to_owned(), at.saturating_sub(received));
            }
            if let Some(placed) = start.placed_at {
                self.sample("placed->running".to_owned(), at.saturating_sub(placed));
            }
        }
    }

    /// The report as markdown: the counts of lines, a note on the clocks, and one table row
    /// per segment with its count, p50, p90, p99 and max in milliseconds.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("# Start timeline\n\n");
        let _ = write!(
            out,
            "Starts: {}. Malformed Started lines: {}.\n\n",
            self.starts, self.malformed
        );
        out.push_str(
            "All values are milliseconds, signed. Percentiles are nearest-rank. \
             `placed->received` and `placed->running` span the server's clock and the agent's \
             clock, so a skew between the hosts shows in them.\n\n",
        );
        if self.rows.is_empty() {
            out.push_str("No starts.\n");
            return out;
        }
        out.push_str(
            "| segment | count | p50 | p90 | p99 | max |\n|---|---:|---:|---:|---:|---:|\n",
        );
        for (name, samples) in &self.rows {
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            let _ = writeln!(
                out,
                "| {name} | {} | {} | {} | {} | {} |",
                sorted.len(),
                nearest_rank(&sorted, 50),
                nearest_rank(&sorted, 90),
                nearest_rank(&sorted, 99),
                nearest_rank(&sorted, 100),
            );
        }
        out
    }
}

/// `js-start-timeline [FILE...]`: reads the agent's journal lines from the files, or from
/// standard input when there are none, and prints the table of the segments of the starts.
#[derive(Clone, Debug, Parser)]
#[command(name = "js-start-timeline", version, about)]
pub struct StartTimelineCli {
    /// The files of journal lines.
    pub inputs: Vec<PathBuf>,
}

/// Runs the command: the table to `out`, reading the files in order or `stdin` when there are
/// none.
///
/// # Errors
/// Whatever stops a read or the write.
pub fn run(cli: &StartTimelineCli, stdin: &mut dyn Read, out: &mut dyn Write) -> Result<(), Error> {
    let mut timelines = Timelines::new();
    if cli.inputs.is_empty() {
        let mut bytes = Vec::new();
        stdin.read_to_end(&mut bytes)?;
        timelines.add_text(&String::from_utf8_lossy(&bytes));
    }
    for path in &cli.inputs {
        let bytes = fs::read(path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        timelines.add_text(&String::from_utf8_lossy(&bytes));
    }
    out.write_all(timelines.render().as_bytes())?;
    Ok(())
}

/// Runs the command line: the table goes to standard output, a failure to standard error.
#[must_use]
pub fn main() -> ExitCode {
    let cli = StartTimelineCli::parse();
    let mut stdin = std::io::stdin().lock();
    match run(&cli, &mut stdin, &mut std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("js-start-timeline: {error}");
            ExitCode::FAILURE
        }
    }
}
