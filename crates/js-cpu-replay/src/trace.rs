//! The trace: the recorded jobs a replay runs on, one line each.
//!
//! A trace holds no job id, host name, label, command or absolute time: times are
//! milliseconds since the first arrival, hosts are indices `0..` and classes are dense
//! indices in order of first appearance ([`crate::convert`] makes them so).
//!
//! The file is the line [`HEADER`], then one line per job in arrival order, its fields
//! separated by tabs, in the order of the fields of [`Job`]: a number in decimal, an absent
//! value as `-`, the end as `ok`, `failed` or `memory`.

use std::fmt::Write as _;
use std::str::FromStr;

/// The first line of a trace.
pub const HEADER: &str = "# js-cpu-replay trace 1";

/// How a job's run ended, as far as its class's history is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// It exited with code 0: its CPU and memory teach its class.
    Ok,
    /// Any other end but a memory kill (a non-zero exit, its wall limit, a cancel, a failed
    /// input): it teaches its class nothing.
    Failed,
    /// It was stopped for memory: it raises its class's memory.
    Memory,
}

impl End {
    /// The word of the end in a trace line.
    fn word(self) -> &'static str {
        match self {
            End::Ok => "ok",
            End::Failed => "failed",
            End::Memory => "memory",
        }
    }

    /// The end a trace line's word names.
    fn of(word: &str) -> Option<End> {
        match word {
            "ok" => Some(End::Ok),
            "failed" => Some(End::Failed),
            "memory" => Some(End::Memory),
            _ => None,
        }
    }
}

/// One recorded job that ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// When it was submitted, in milliseconds since the trace's first arrival.
    pub arrive_ms: u64,
    /// How long it waited before it was placed, as recorded.
    pub wait_ms: u64,
    /// How long it took from its placement to its start (its image and its files), as
    /// recorded; it held its booking meanwhile.
    pub start_ms: u64,
    /// How long it ran, as recorded (the store's clock, from its start to its end).
    pub wall_ms: u64,
    /// The CPU time it used, in milliseconds.
    pub cpu_ms: u64,
    /// The most CPU it used over a window of about five seconds, in millicores, when known.
    pub peak_milli: Option<u32>,
    /// How long some task of it waited for a CPU (the `some` total of its `cpu.pressure`), in
    /// milliseconds, when known.
    pub pressure_ms: Option<u64>,
    /// The most memory it held, in MiB.
    pub memory_mib: u32,
    /// The CPU it was booked, as recorded, in millicores.
    pub booked_milli: u32,
    /// The memory it was booked, as recorded, in MiB.
    pub booked_mib: u32,
    /// The host it ran on, as an index.
    pub host: u8,
    /// How it ended.
    pub end: End,
    /// Its class: the estimator key it is sized by, as an index.
    pub class: u32,
    /// Its fallback class, as an index, when it names one.
    pub fallback: Option<u32>,
    /// The key of its client's whole history, as an index.
    pub client: u32,
}

/// Why a trace does not read.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    /// The first line is not [`HEADER`].
    #[error("the first line is not {HEADER:?}")]
    Header,
    /// A line does not hold a job.
    #[error("line {line}: {what}")]
    Line {
        /// The 1-based line number.
        line: usize,
        /// What is wrong with it.
        what: &'static str,
    },
}

/// The jobs of a trace, in arrival order.
///
/// # Errors
/// [`TraceError`] for a wrong header, a line that is not a job, or a job that arrives
/// before the one above it.
pub fn parse(text: &str) -> Result<Vec<Job>, TraceError> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(TraceError::Header);
    }
    let mut jobs: Vec<Job> = Vec::new();
    for (at, line) in lines.enumerate() {
        let number = at + 2;
        let job = job_of(line).ok_or(TraceError::Line {
            line: number,
            what: "not a job of 15 fields",
        })?;
        if jobs
            .last()
            .is_some_and(|last| last.arrive_ms > job.arrive_ms)
        {
            return Err(TraceError::Line {
                line: number,
                what: "it arrives before the line above it",
            });
        }
        jobs.push(job);
    }
    Ok(jobs)
}

/// The job of one line, or `None` when it is not one.
fn job_of(line: &str) -> Option<Job> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [
        arrive,
        wait,
        start,
        wall,
        cpu,
        peak,
        pressure,
        memory,
        booked_cpu,
        booked_memory,
        host,
        end,
        class,
        fallback,
        client,
    ] = fields.as_slice()
    else {
        return None;
    };
    Some(Job {
        arrive_ms: number(arrive)?,
        wait_ms: number(wait)?,
        start_ms: number(start)?,
        wall_ms: number(wall)?,
        cpu_ms: number(cpu)?,
        peak_milli: absent_or(peak)?,
        pressure_ms: absent_or(pressure)?,
        memory_mib: number(memory)?,
        booked_milli: number(booked_cpu)?,
        booked_mib: number(booked_memory)?,
        host: number(host)?,
        end: End::of(end)?,
        class: number(class)?,
        fallback: absent_or(fallback)?,
        client: number(client)?,
    })
}

/// A decimal number.
fn number<T: FromStr>(text: &str) -> Option<T> {
    text.parse().ok()
}

/// `Some(None)` for `-`, `Some(Some(n))` for a decimal number, `None` for anything else.
fn absent_or<T: FromStr>(text: &str) -> Option<Option<T>> {
    if text == "-" {
        Some(None)
    } else {
        number(text).map(Some)
    }
}

/// The text of a trace of `jobs`, which [`parse`] reads back.
#[must_use]
pub fn write(jobs: &[Job]) -> String {
    let mut text = format!("{HEADER}\n");
    for job in jobs {
        let fields = [
            job.arrive_ms.to_string(),
            job.wait_ms.to_string(),
            job.start_ms.to_string(),
            job.wall_ms.to_string(),
            job.cpu_ms.to_string(),
            shown(job.peak_milli),
            shown(job.pressure_ms),
            job.memory_mib.to_string(),
            job.booked_milli.to_string(),
            job.booked_mib.to_string(),
            job.host.to_string(),
            job.end.word().to_owned(),
            job.class.to_string(),
            shown(job.fallback),
            job.client.to_string(),
        ];
        let _ = writeln!(text, "{}", fields.join("\t"));
    }
    text
}

/// A value as a trace writes it: the number, or `-` when absent.
fn shown<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A job that arrives at `arrive_ms`, ran `wall_ms` using `cpu_ms` of CPU, of class
    /// `class`, with nothing else known: exit 0, 1 GiB, booked a core and 4 GiB, on host 0.
    pub(crate) fn job(arrive_ms: u64, wall_ms: u64, cpu_ms: u64, class: u32) -> Job {
        Job {
            arrive_ms,
            wait_ms: 0,
            start_ms: 0,
            wall_ms,
            cpu_ms,
            peak_milli: None,
            pressure_ms: None,
            memory_mib: 1024,
            booked_milli: 1000,
            booked_mib: 4096,
            host: 0,
            end: End::Ok,
            class,
            fallback: None,
            client: 0,
        }
    }

    #[test]
    fn a_trace_reads_back_what_was_written() {
        let jobs = vec![
            job(0, 10, 5, 1),
            Job {
                peak_milli: Some(20_000),
                pressure_ms: Some(7),
                fallback: Some(3),
                end: End::Failed,
                host: 5,
                start_ms: 30,
                ..job(4, 1000, 900, 2)
            },
            Job {
                end: End::Memory,
                ..job(4, 1, 1, 2)
            },
        ];
        let text = write(&jobs);
        assert_eq!(
            text,
            "# js-cpu-replay trace 1\n\
             0\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0\n\
             4\t0\t30\t1000\t900\t20000\t7\t1024\t1000\t4096\t5\tfailed\t2\t3\t0\n\
             4\t0\t0\t1\t1\t-\t-\t1024\t1000\t4096\t0\tmemory\t2\t-\t0\n"
        );
        assert_eq!(parse(&text), Ok(jobs));
    }

    #[test]
    fn a_trace_without_its_header_is_refused() {
        assert_eq!(parse(""), Err(TraceError::Header));
        assert_eq!(parse("# another 1\n"), Err(TraceError::Header));
    }

    #[test]
    fn each_malformed_line_is_refused_by_number() {
        let good = "0\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0";
        let cases = [
            // Fourteen fields, then sixteen.
            "0\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-",
            "0\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0\t0",
            // A word where a number goes, in a plain field and in an optional one.
            "x\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0",
            "0\t0\t0\t10\t5\tx\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0",
            // A host past an index's range, and an end no trace writes.
            "0\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t256\tok\t1\t-\t0",
            "0\t0\t0\t10\t5\t-\t-\t1024\t1000\t4096\t0\tlost\t1\t-\t0",
        ];
        for bad in cases {
            let text = format!("{HEADER}\n{good}\n{bad}\n");
            assert_eq!(
                parse(&text),
                Err(TraceError::Line {
                    line: 3,
                    what: "not a job of 15 fields"
                }),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_job_that_arrives_before_the_one_above_it_is_refused() {
        let text = write(&[job(5, 1, 1, 0), job(4, 1, 1, 0)]);
        assert_eq!(
            parse(&text),
            Err(TraceError::Line {
                line: 3,
                what: "it arrives before the line above it"
            })
        );
        assert_eq!(
            TraceError::Line {
                line: 3,
                what: "it arrives before the line above it"
            }
            .to_string(),
            "line 3: it arrives before the line above it"
        );
        assert_eq!(
            TraceError::Header.to_string(),
            "the first line is not \"# js-cpu-replay trace 1\""
        );
    }
}
