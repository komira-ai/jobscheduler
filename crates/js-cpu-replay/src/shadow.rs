//! `shadow`: the table of the job service's shadow checks (`--placement-curves shadow`,
//! `docs/design/cpu-over-time.md` step 2), per class, from the server's own log.
//!
//! The server logs one line per run it checks against its class's profiles ([`MESSAGE`]),
//! with the run's mean and peak, the profile's level and predicted peak, and the error of the
//! curve and of the flat level over the run. [`parse`] reads those lines out of a log (any
//! other line is skipped) and [`table`] adds them up per class:
//!
//! - **mean error**: the sum of |run mean - profile level|, as a share of the sum of the run
//!   means, over the class's runs whose line gives both;
//! - **peak error**: the sum of |run peak - predicted peak|, as a share of the sum of the run
//!   peaks, over the runs whose line gives both;
//! - **curve / flat**: the curve's error as a share of the flat level's, over the scored runs
//!   (a profile is shaped when the share is at most 80% over its self-check window);
//! - **shaped**: whether the class's profile was shaped after its last run whose line says
//!   so, as the server said;
//! - **CPU error removed**: the flat level's CPU error minus the curve's, in core-seconds, over
//!   the scored runs. The line holds no booking, so this is what the curve would have predicted
//!   better than the flat line, not CPU a booking would have given back.
//!
//! Each figure is computed from the lines alone; the run counts are lines, not jobs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The message of the server's check line.
pub const MESSAGE: &str = "a run was checked against its class's profile";

/// A check line that does not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShadowError {
    /// The line names no class, or no CPU status after it.
    #[error("line {line}: no class and CPU status")]
    NoClass {
        /// The line, counted from 1 in the log.
        line: usize,
    },
    /// A field is not the number or the `true`/`false` it should be.
    #[error("line {line}: {field} is {value:?}")]
    Value {
        /// The line, counted from 1 in the log.
        line: usize,
        /// The field's name.
        field: String,
        /// Its text.
        value: String,
    },
}

/// What one dimension (CPU in millicores, memory in MiB) of one check said; a value is `None`
/// when the line leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dim {
    /// `learned`, `not-learned`, `no-series`, `partial`, `disagrees` or `not-a-success`.
    pub status: String,
    /// Whether the profile was shaped after the run.
    pub shaped: Option<bool>,
    /// The run's mean.
    pub mean: Option<u64>,
    /// The profile's level before the run.
    pub level: Option<u64>,
    /// The run's peak.
    pub peak: Option<u64>,
    /// The profile's predicted peak.
    pub predicted_peak: Option<u64>,
    /// The curve's error over the run, in unit-seconds.
    pub curve_error: Option<u64>,
    /// The flat level's error over the run, in unit-seconds.
    pub flat_error: Option<u64>,
}

/// One check line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The class's name, as the server logged it.
    pub class: String,
    /// The CPU check.
    pub cpu: Dim,
    /// The memory check.
    pub memory: Dim,
}

/// The `key=value` fields of `text`: a value in quotes runs to the next quote, any other to
/// the next space.
fn fields(text: &str) -> BTreeMap<&str, &str> {
    let mut out = BTreeMap::new();
    let mut rest = text;
    while let Some((key, after)) = rest.split_once('=') {
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => after.split_once(' ').unwrap_or((after, "")),
        };
        out.insert(key.trim(), value);
        rest = next;
    }
    out
}

/// Reads the fields of one dimension, named `<prefix>`, `<prefix>_runs` and so on, with the
/// unit `unit` on its values (`millicores`, `mib`).
fn dim(
    fields: &BTreeMap<&str, &str>,
    prefix: &str,
    unit: &str,
    line: usize,
) -> Result<Dim, ShadowError> {
    let text = |name: String| fields.get(name.as_str()).map(|value| (name, *value));
    let bad = |(field, value): (String, &str)| ShadowError::Value {
        line,
        field,
        value: value.to_owned(),
    };
    let number = |name: String| {
        text(name)
            .map(|found| found.1.parse::<u64>().map_err(|_| bad(found)))
            .transpose()
    };
    let shaped = text(format!("{prefix}_shaped"))
        .map(|found| match found.1 {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(bad(found)),
        })
        .transpose()?;
    Ok(Dim {
        status: fields.get(prefix).copied().unwrap_or_default().to_owned(),
        shaped,
        mean: number(format!("{prefix}_mean_{unit}"))?,
        level: number(format!("{prefix}_level_{unit}"))?,
        peak: number(format!("{prefix}_peak_{unit}"))?,
        predicted_peak: number(format!("{prefix}_predicted_peak_{unit}"))?,
        curve_error: number(format!("{prefix}_curve_error"))?,
        flat_error: number(format!("{prefix}_flat_error"))?,
    })
}

/// The check lines of `log`, in its order; every other line is skipped.
///
/// # Errors
/// A check line with no class, or with a value that is not a number (or `true`/`false`).
pub fn parse(log: &str) -> Result<Vec<Check>, ShadowError> {
    let mut checks = Vec::new();
    for (index, text) in log.lines().enumerate() {
        let line = index + 1;
        let Some((_, after)) = text.split_once(MESSAGE) else {
            continue;
        };
        // The class is the server's text as it stands, spaces and all: it runs to the CPU
        // status, which follows it.
        let (head, tail) = after
            .split_once(" cpu=")
            .ok_or(ShadowError::NoClass { line })?;
        let (_, class) = head
            .split_once("class=")
            .ok_or(ShadowError::NoClass { line })?;
        let tail = format!("cpu={tail}");
        let fields = fields(&tail);
        checks.push(Check {
            class: class.to_owned(),
            cpu: dim(&fields, "cpu", "millicores", line)?,
            memory: dim(&fields, "memory", "mib", line)?,
        });
    }
    Ok(checks)
}

/// The sums of one dimension of one class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Sum {
    mean_error: u64,
    means: u64,
    peak_error: u64,
    peaks: u64,
    scored: usize,
    curve: u64,
    flat: u64,
    shaped: Option<bool>,
}

impl Sum {
    fn add(&mut self, dim: &Dim) {
        if let (Some(mean), Some(level)) = (dim.mean, dim.level) {
            self.mean_error += mean.abs_diff(level);
            self.means += mean;
        }
        if let (Some(peak), Some(predicted)) = (dim.peak, dim.predicted_peak) {
            self.peak_error += peak.abs_diff(predicted);
            self.peaks += peak;
        }
        if let (Some(curve), Some(flat)) = (dim.curve_error, dim.flat_error) {
            self.scored += 1;
            self.curve += curve;
            self.flat += flat;
        }
        self.shaped = dim.shaped.or(self.shaped);
    }

    /// The flat level's error minus the curve's.
    fn removed(self) -> i128 {
        i128::from(self.flat) - i128::from(self.curve)
    }
}

/// One class's sums.
#[derive(Debug, Clone, Copy, Default)]
struct Class {
    lines: usize,
    cpu: Sum,
    memory: Sum,
}

/// `part` as a share of `whole`, to a tenth of a percent; `-` when `whole` is 0.
fn share(part: impl Into<i128>, whole: u64) -> String {
    (part.into() * 1000)
        .checked_div(i128::from(whole))
        .map_or_else(|| "-".to_owned(), tenths)
}

/// A count of tenths (of a percent, of a core-second) as text with one decimal.
fn tenths(value: i128) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let value = value.unsigned_abs();
    format!("{sign}{}.{}", value / 10, value % 10)
}

/// A CPU error in millicore-seconds as core-seconds, to a tenth.
fn core_seconds(millicore_seconds: i128) -> String {
    tenths(millicore_seconds / 100)
}

/// `yes`, `no` or `-`.
fn said(shaped: Option<bool>) -> &'static str {
    match shaped {
        Some(true) => "yes",
        Some(false) => "no",
        None => "-",
    }
}

/// The count of each status of one dimension, in name order.
fn statuses<'a>(checks: impl Iterator<Item = &'a Dim>) -> String {
    let mut counts = BTreeMap::<&str, usize>::new();
    checks.for_each(|dim| *counts.entry(dim.status.as_str()).or_default() += 1);
    let said: Vec<String> = counts
        .iter()
        .map(|(status, count)| format!("{status} {count}"))
        .collect();
    said.join(", ")
}

/// The report of `checks`: what the lines hold, then one table row per class of at least
/// `min_runs` lines, most lines first, named as logged or, with `hide_classes`, `class 1`,
/// `class 2` and so on; then the shaped share and the CPU error removed.
#[must_use]
pub fn table(checks: &[Check], min_runs: usize, hide_classes: bool) -> String {
    let mut classes = BTreeMap::<&str, Class>::new();
    for check in checks {
        let class = classes.entry(check.class.as_str()).or_default();
        class.lines += 1;
        class.cpu.add(&check.cpu);
        class.memory.add(&check.memory);
    }
    let mut kept: Vec<(&str, Class)> = classes
        .iter()
        .filter(|(_, class)| class.lines >= min_runs)
        .map(|(name, class)| (*name, *class))
        .collect();
    kept.sort_by_key(|(_, class)| std::cmp::Reverse(class.lines));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} check lines, {} classes, {} with at least {min_runs} lines",
        checks.len(),
        classes.len(),
        kept.len(),
    );
    let _ = writeln!(out, "CPU: {}", statuses(checks.iter().map(|c| &c.cpu)));
    let _ = writeln!(
        out,
        "memory: {}",
        statuses(checks.iter().map(|c| &c.memory))
    );
    out.push_str(
        "\n| class | lines | CPU scored | CPU mean error % | CPU peak error % | CPU curve/flat % \
         | CPU shaped | CPU error removed (core-s) | memory scored | memory mean error % \
         | memory peak error % | memory curve/flat % | memory shaped |\n\
         |---|--:|--:|--:|--:|--:|---|--:|--:|--:|--:|--:|---|\n",
    );
    for (number, (name, class)) in kept.iter().enumerate() {
        let name = if hide_classes {
            format!("class {}", number + 1)
        } else {
            name.replace('|', "\\|")
        };
        let (cpu, memory) = (class.cpu, class.memory);
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            class.lines,
            cpu.scored,
            share(cpu.mean_error, cpu.means),
            share(cpu.peak_error, cpu.peaks),
            share(cpu.curve, cpu.flat),
            said(cpu.shaped),
            core_seconds(cpu.removed()),
            memory.scored,
            share(memory.mean_error, memory.means),
            share(memory.peak_error, memory.peaks),
            share(memory.curve, memory.flat),
            said(memory.shaped),
        );
    }
    let count = |pick: fn(&Class) -> Sum| {
        kept.iter()
            .filter(|(_, class)| pick(class).shaped == Some(true))
            .count()
    };
    let (cpu_shaped, memory_shaped) = (count(|class| class.cpu), count(|class| class.memory));
    let whole = kept.len() as u64;
    let removed = |shaped_only: bool| -> i128 {
        kept.iter()
            .filter(|(_, class)| !shaped_only | (class.cpu.shaped == Some(true)))
            .map(|(_, class)| class.cpu.removed())
            .sum()
    };
    let flat: u64 = kept.iter().map(|(_, class)| class.cpu.flat).sum();
    let _ = writeln!(
        out,
        "\nClasses in the table: {}; shaped by CPU: {cpu_shaped} ({}%), by memory: \
         {memory_shaped} ({}%).\nCPU error the curve removes against the flat line: {} core-s over the \
         table's classes ({}% of the flat line's {} core-s), {} core-s over its CPU-shaped \
         classes.",
        kept.len(),
        share(cpu_shaped as u64, whole),
        share(memory_shaped as u64, whole),
        core_seconds(removed(false)),
        share(removed(false), flat),
        core_seconds(i128::from(flat)),
        core_seconds(removed(true)),
    );
    out
}

#[cfg(test)]
#[path = "shadow/tests.rs"]
mod tests;
