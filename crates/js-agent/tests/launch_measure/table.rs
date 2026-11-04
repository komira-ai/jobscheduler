//! The pure part of `js-launch-measure`: the statistics of the samples and the text of the
//! result. No I/O and no clock: a sample comes in as whole microseconds, measured elsewhere.
//! Included with `#[path]` by the program (tests/bin/launch_measure.rs) and by its unit test
//! target (tests/launch_table.rs).
//!
//! The rules, each in one place:
//!
//! - A failed sample (its check, stage or start returned an error, or its job never
//!   reported) is counted in
//!   `failed`, listed with its error under the table, and in no statistic. `runs` counts the
//!   samples that are measured.
//! - The median of an even count is the mean of the middle two; p90 is the nearest rank,
//!   the value at rank ceil(0.9 n) of the sorted samples, counted from 1.
//! - A value is printed in milliseconds with one decimal place, rounded half away from zero
//!   at the tenth of a millisecond: 12.25 ms prints `12.3`, -6.55 ms prints `-6.6`.
//! - A variant with more than a quarter of its samples failed is a broken measurement, and a
//!   variant with no measured sample has nothing to show: either is an error, never a row.

use std::fmt;

/// A duration in half-microseconds: whole microseconds doubled, so that the median of an even
/// count (the mean of two whole microseconds) is exact. Signed, for a difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stat(i128);

impl Stat {
    /// `micros` whole microseconds.
    pub fn from_micros(micros: u64) -> Self {
        Self(i128::from(micros) * 2)
    }

    /// The mean of `a` and `b` microseconds.
    fn mean_of(a: u64, b: u64) -> Self {
        Self(i128::from(a) + i128::from(b))
    }

    /// `self` minus `other`.
    pub fn minus(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

/// Milliseconds with one decimal place, rounded half away from zero (see the module docs).
impl fmt::Display for Stat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A tenth of a millisecond is 100 microseconds, 200 half-microseconds.
        let tenths = (self.0.unsigned_abs() + 100) / 200;
        let sign = if self.0 < 0 && tenths > 0 { "-" } else { "" };
        write!(f, "{sign}{}.{}", tenths / 10, tenths % 10)
    }
}

/// `samples`, sorted.
fn sorted(samples: &[u64]) -> Vec<u64> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted
}

/// The median of `samples` (whole microseconds): the middle value, or the mean of the middle
/// two of an even count; `None` for no sample.
pub fn median(samples: &[u64]) -> Option<Stat> {
    let sorted = sorted(samples);
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(Stat::from_micros(sorted[n / 2])),
        _ => Some(Stat::mean_of(sorted[n / 2 - 1], sorted[n / 2])),
    }
}

/// The 90th percentile of `samples` by nearest rank: the value at rank ceil(0.9 n) of the
/// sorted samples, counted from 1; `None` for no sample.
pub fn p90(samples: &[u64]) -> Option<Stat> {
    let sorted = sorted(samples);
    let rank = (sorted.len() * 9).div_ceil(10);
    let index = rank.checked_sub(1)?;
    sorted.get(index).copied().map(Stat::from_micros)
}

/// Whether `failed` of `total` samples is more than a quarter: a broken measurement.
pub fn broken(failed: usize, total: usize) -> bool {
    failed * 4 > total
}

/// What a sample is measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Egress mode none: loopback only, nothing wired.
    None,
    /// Egress mode allow-list: the veth, the /30, the firewall's set member, the resolver
    /// file and the init's wait for the route.
    Wired,
}

impl Variant {
    /// The variant's name in the table.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Wired => "wired",
        }
    }
}

/// One sample: how long from the call of `start_staged` to the job's report, in whole
/// microseconds, and the error when it failed. A failed sample keeps the time to its error,
/// or 0 when the check or the stage failed before `start_staged` was called; no statistic
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The interval, in whole microseconds.
    pub micros: u64,
    /// Why it failed; `None` for a measured sample.
    pub failed: Option<String>,
}

/// The samples of one variant, in the order they were taken; warm-up samples are not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Samples {
    /// The variant.
    pub variant: Variant,
    /// The samples.
    pub samples: Vec<Sample>,
}

/// One row of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// The variant.
    pub variant: Variant,
    /// The measured samples.
    pub runs: usize,
    /// The failed samples.
    pub failed: usize,
    /// The smallest measured sample.
    pub min: Stat,
    /// The median of the measured samples.
    pub median: Stat,
    /// Their 90th percentile, by nearest rank.
    pub p90: Stat,
    /// The largest measured sample.
    pub max: Stat,
}

/// Why there is no table: what the variant's samples came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The variant has no measured sample.
    NoSample {
        /// The variant.
        variant: Variant,
        /// How many of its samples failed.
        failed: usize,
    },
    /// More than a quarter of the variant's samples failed.
    Broken {
        /// The variant.
        variant: Variant,
        /// How many of its samples failed.
        failed: usize,
        /// How many samples it has.
        total: usize,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSample { variant, failed } => write!(
                f,
                "variant {}: no measured sample ({failed} failed): nothing to show",
                variant.name()
            ),
            Self::Broken {
                variant,
                failed,
                total,
            } => write!(
                f,
                "variant {}: {failed} of {total} samples failed, more than a quarter: a broken \
                 measurement, no table",
                variant.name()
            ),
        }
    }
}

/// The row of `samples`.
///
/// # Errors
///
/// No measured sample, or more than a quarter failed.
pub fn row(samples: &Samples) -> Result<Row, Refusal> {
    let variant = samples.variant;
    let measured: Vec<u64> = samples
        .samples
        .iter()
        .filter(|sample| sample.failed.is_none())
        .map(|sample| sample.micros)
        .collect();
    let total = samples.samples.len();
    let failed = total - measured.len();
    let no_sample = Refusal::NoSample { variant, failed };
    let sorted = sorted(&measured);
    let (Some(&min), Some(&max)) = (sorted.first(), sorted.last()) else {
        return Err(no_sample);
    };
    if broken(failed, total) {
        return Err(Refusal::Broken {
            variant,
            failed,
            total,
        });
    }
    let (Some(median), Some(p90)) = (median(&measured), p90(&measured)) else {
        return Err(no_sample);
    };
    Ok(Row {
        variant,
        runs: measured.len(),
        failed,
        min: Stat::from_micros(min),
        median,
        p90,
        max: Stat::from_micros(max),
    })
}

/// The table's header and its separator.
pub const HEADER: [&str; 2] = [
    "| variant | runs | failed | min | median | p90 | max |",
    "|---|---:|---:|---:|---:|---:|---:|",
];

/// The table of `rows`: the header, its separator and one line for each row, in order, each
/// line ended by a newline.
pub fn table(rows: &[Row]) -> String {
    let mut text = String::new();
    for line in HEADER {
        text.push_str(line);
        text.push('\n');
    }
    for row in rows {
        text.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            row.variant.name(),
            row.runs,
            row.failed,
            row.min,
            row.median,
            row.p90,
            row.max
        ));
    }
    text
}

/// `wired - none (median): X ms`, from the rows of the two variants; `None` unless `rows`
/// holds both.
pub fn difference_line(rows: &[Row]) -> Option<String> {
    let median_of = |variant: Variant| {
        rows.iter()
            .find(|row| row.variant == variant)
            .map(|row| row.median)
    };
    let (none, wired) = (median_of(Variant::None)?, median_of(Variant::Wired)?);
    Some(format!("wired - none (median): {} ms", wired.minus(none)))
}

/// How the samples were taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Method {
    /// The measured samples of each variant.
    pub runs: u32,
    /// The samples of each variant taken first and discarded.
    pub warmup: u32,
}

/// What the program read of the machine it ran on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The runner image, as the `--image-name` flag gave it.
    pub image: String,
    /// The kernel release (`uname -r`).
    pub kernel: String,
    /// The first line of `crun --version`.
    pub crun: String,
    /// The CPUs this process may use (`std::thread::available_parallelism`).
    pub cpus: usize,
}

/// The whole result, in Markdown: the method, the table, the difference of the medians, the
/// failed samples (each with its error), and the facts of the machine.
///
/// # Errors
///
/// The first variant with no measured sample or with more than a quarter failed: then there
/// is no table at all.
pub fn report(variants: &[Samples], method: Method, facts: &Facts) -> Result<String, Refusal> {
    let rows = variants.iter().map(row).collect::<Result<Vec<_>, _>>()?;
    let mut text = String::from("### crun launch time\n\n");
    text.push_str(&format!(
        "One sample is the interval from the call of the agent's `CrunRuntime::start_staged` \
         (the job staged by `CrunRuntime::stage`: its lease directories, cgroup and bundle made, \
         its image in the store) to the first byte the job writes to `/scratch/up`, a FIFO in \
         the run's scratch directory (a bind mount; `/work` is an overlayfs, where a FIFO does \
         not share a pipe with the host) that the program holds open, read by a blocking read. \
         Outside it: the lifecycle's per-start firewall check (`check_egress`), the stage, \
         image pull and unpack, and the teardown between samples. The allow-list names one \
         endpoint and nothing listens there: the job opens no connection, so the wired \
         interval covers the wiring, not a connect. {} measured samples per variant after {} \
         discarded, the variants alternating sample by sample (none, wired, none, ...). A \
         failed sample is counted and listed, and is in no statistic.\n\n",
        method.runs, method.warmup
    ));
    text.push_str(
        "Milliseconds, one decimal place, rounded half away from zero; p90 by nearest rank.\n\n",
    );
    text.push_str(&table(&rows));
    if let Some(line) = difference_line(&rows) {
        text.push('\n');
        text.push_str(&line);
        text.push('\n');
    }
    let failures: Vec<String> = variants
        .iter()
        .flat_map(|variant| {
            variant
                .samples
                .iter()
                .enumerate()
                .filter_map(move |(index, sample)| {
                    sample.failed.as_ref().map(|why| {
                        format!("- {} sample {}: {why}\n", variant.variant.name(), index + 1)
                    })
                })
        })
        .collect();
    if !failures.is_empty() {
        text.push_str("\nFailed samples, left out of every statistic:\n\n");
        text.push_str(&failures.concat());
    }
    text.push_str(&format!(
        "\nRunner:\n\n- image: {}\n- kernel: {}\n- crun: {}\n- CPUs: {}\n",
        facts.image, facts.kernel, facts.crun, facts.cpus
    ));
    Ok(text)
}
