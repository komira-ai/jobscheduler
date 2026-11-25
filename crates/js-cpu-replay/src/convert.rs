//! The trace of an export: the job records the job service's `GetUsage` returned (one JSON
//! object per line), joined by job id with the agent's result lines (`job: result run=...`,
//! written by `result_line` in the agent's `worker/replies.rs`), and made anonymous.
//!
//! A job's store row (`crate::rows`), when the export holds it, gives its times (submitted,
//! placed, started, ended, by the store's clock), its estimator keys, its end, its CPU, peak and
//! contention, and these win over the record's and the result line's.
//!
//! A record becomes a job of the trace when it ran (it has a run time and a usage), on a host
//! that holds at least a hundredth of the records (a host of another kind that ran a few test
//! jobs is left out), and started at or after the cut: before the agent stopped holding a
//! job's CPU at its booking (`cpu.max`), a run's CPU was a lower bound of what it asked for.
//!
//! The trace keeps no job id, host name, class name or absolute time. A job's class is the
//! size class its agent named, else its recorded booking (CPU and memory) as a stand-in; every
//! job has the one client of the export. Its end is taken as an exit 0: neither source records
//! the exit code.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::rows::Facts;
use crate::trace::{End, Job};

/// What the agent's result line of a job says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentResult {
    /// The size class, when the line names one.
    pub class: Option<String>,
    /// The CPU time, in microseconds.
    pub cpu_usec: Option<u64>,
    /// The `some` total of the job's `cpu.pressure`, in microseconds.
    pub pressure_usec: Option<u64>,
    /// The most CPU over about five seconds, in millicores.
    pub peak_milli: Option<u64>,
}

/// The result lines of `text` by job id: every line that holds `job: result ` and names a
/// run, whatever comes before (a journal's prefix); a job named twice keeps its last line.
/// Each `key=value` word is read by [`words`], so a class the agent quoted (a build target's
/// full name with its spaces) is read whole.
#[must_use]
pub fn results(text: &str) -> BTreeMap<String, AgentResult> {
    let mut by_job = BTreeMap::new();
    for line in text.lines() {
        let Some((_, words)) = line.split_once("job: result ") else {
            continue;
        };
        let words = self::words(words);
        let Some(run) = words.get("run") else {
            continue;
        };
        let number = |key: &str| words.get(key).and_then(|value| value.parse().ok());
        by_job.insert(
            run.clone(),
            AgentResult {
                class: words.get("class").cloned(),
                cpu_usec: number("cpu_usage_usec"),
                pressure_usec: number("cpu_pressure_some_usec"),
                peak_milli: number("cpu_peak_millicores"),
            },
        );
    }
    by_job
}

/// The `key=value` words of `text`, split at whitespace: a value runs to the next whitespace,
/// or, when it opens with `"`, to the next `"` that no `\` escapes, with each `\` and the
/// character after it read as that character (the agent's `class_word` writes a class that
/// is not one word so). A value `-` that is not quoted is not known and is left out; a word
/// without `=` is skipped; a quoted value the line ends inside runs to the end.
fn words(text: &str) -> BTreeMap<String, String> {
    let mut words = BTreeMap::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return words;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_whitespace() && *c != '=') {
            key.push(c);
        }
        if chars.next_if_eq(&'=').is_none() {
            continue;
        }
        let mut value = String::new();
        let quoted = chars.next_if_eq(&'"').is_some();
        if quoted {
            while let Some(c) = chars.next().filter(|c| *c != '"') {
                value.extend(if c == '\\' { chars.next() } else { Some(c) });
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                value.push(c);
            }
        }
        if quoted || value != "-" {
            words.insert(key, value);
        }
    }
}

/// Why an export does not convert.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("line {line} of the records: {what}")]
pub struct ConvertError {
    /// The 1-based line number.
    pub line: usize,
    /// What is wrong with it.
    pub what: &'static str,
}

/// One record of the export.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    job: String,
    host: String,
    ended_ms: u64,
    run_ms: Option<u64>,
    wait_ms: u64,
    booked_milli: u64,
    booked_bytes: u64,
    cpu_usec: Option<u64>,
    peak_bytes: Option<u64>,
}

/// The number at `path` in `value`: a JSON number, or a string of one (as proto3 JSON writes
/// a 64-bit number).
fn number(value: &Value, path: &[&str]) -> Option<u64> {
    let value = path.iter().try_fold(value, |value, key| value.get(key))?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// The record of one line.
fn record(line: &str) -> Result<Record, &'static str> {
    let value: Value = serde_json::from_str(line).map_err(|_| "not a JSON object")?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(Record {
        job: text("jobId").ok_or("no jobId")?,
        host: text("hostId").ok_or("no hostId")?,
        ended_ms: number(&value, &["endedAtMs"]).ok_or("no endedAtMs")?,
        run_ms: number(&value, &["runMs"]),
        wait_ms: number(&value, &["queueWaitMs"]).unwrap_or(0),
        booked_milli: number(&value, &["booked", "cpuMillis"]).unwrap_or(0),
        booked_bytes: number(&value, &["booked", "memoryBytes"]).unwrap_or(0),
        cpu_usec: number(&value, &["used", "cpuUsec"]),
        peak_bytes: number(&value, &["used", "peakMemoryBytes"]),
    })
}

/// A trace and how its records were counted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Converted {
    /// The trace's jobs, in arrival order.
    pub jobs: Vec<Job>,
    /// Records read.
    pub read: usize,
    /// Records that did not run (no run time or no usage).
    pub not_run: usize,
    /// Records of a host that holds under a hundredth of them.
    pub other_host: usize,
    /// Records that started before the cut.
    pub before_cut: usize,
    /// Jobs of the trace that had a result line.
    pub with_result: usize,
    /// Jobs of the trace that had a store row.
    pub with_row: usize,
}

/// When a job was submitted, how long it waited for a host and to start, how long it ran,
/// and when it started, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Times {
    arrive: u64,
    wait: u64,
    start: u64,
    wall: u64,
    started: u64,
}

impl Times {
    /// The times of a job's store row, when it was placed, started and ended.
    fn of_row(facts: &Facts) -> Option<Times> {
        let (placed, started, ended) = (facts.placed_ms?, facts.started_ms?, facts.ended_ms?);
        Some(Times {
            arrive: facts.queued_ms,
            wait: placed.saturating_sub(facts.queued_ms),
            start: started.saturating_sub(placed),
            wall: ended.saturating_sub(started),
            started,
        })
    }

    /// The times of a usage record that ran `run_ms`: its start is its end less its run, and
    /// its arrival its start less its wait (the time from its placement to its start is not
    /// in the record).
    fn of_record(record: &Record, run_ms: u64) -> Times {
        let started = record.ended_ms.saturating_sub(run_ms);
        Times {
            arrive: started.saturating_sub(record.wait_ms),
            wait: record.wait_ms,
            start: 0,
            wall: run_ms,
            started,
        }
    }
}

/// The trace of the records of `usage` (one JSON object per line) that started at or after
/// `since_ms` (milliseconds since the Unix epoch), with what each job's store row says
/// (`rows`, by job id) or, failing that, its agent's result line (`results`).
///
/// # Errors
/// [`ConvertError`] for a line that is not a record.
pub fn convert(
    usage: &str,
    results: &BTreeMap<String, AgentResult>,
    rows: &BTreeMap<String, Facts>,
    since_ms: u64,
) -> Result<Converted, ConvertError> {
    let records = usage
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(at, line)| record(line).map_err(|what| ConvertError { line: at + 1, what }))
        .collect::<Result<Vec<Record>, ConvertError>>()?;
    let mut converted = Converted {
        read: records.len(),
        ..Converted::default()
    };
    let mut per_host: BTreeMap<&str, usize> = BTreeMap::new();
    for record in &records {
        *per_host.entry(&record.host).or_default() += 1;
    }
    let hosts: BTreeMap<&str, u8> = per_host
        .iter()
        .filter(|(_, count)| **count * 100 >= records.len())
        .enumerate()
        .map(|(index, (host, _))| (*host, u8::try_from(index).unwrap_or(u8::MAX)))
        .collect();
    let mut ran = Vec::new();
    for record in &records {
        let (Some(run_ms), Some(cpu_usec)) = (record.run_ms, record.cpu_usec) else {
            converted.not_run += 1;
            continue;
        };
        let Some(&host) = hosts.get(record.host.as_str()) else {
            converted.other_host += 1;
            continue;
        };
        let row = rows.get(&record.job);
        let times = row
            .and_then(Times::of_row)
            .unwrap_or_else(|| Times::of_record(record, run_ms));
        if times.started < since_ms {
            converted.before_cut += 1;
            continue;
        }
        ran.push((times, record, row, cpu_usec, host));
    }
    ran.sort_by(|a, b| {
        (a.0.arrive, a.1.ended_ms, &a.1.job).cmp(&(b.0.arrive, b.1.ended_ms, &b.1.job))
    });
    let first = ran.first().map_or(0, |(times, ..)| times.arrive);
    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    let mut index_of = |name: String| {
        let next = u32::try_from(names.len()).unwrap_or(u32::MAX);
        *names.entry(name).or_insert(next)
    };
    let client = index_of("client".to_owned());
    for (times, record, row, cpu_usec, host) in ran {
        let result = results.get(&record.job);
        converted.with_result += usize::from(result.is_some());
        converted.with_row += usize::from(row.is_some());
        let result = result.cloned().unwrap_or_default();
        let booked = || {
            format!(
                "class booked {} {}",
                record.booked_milli,
                record.booked_bytes >> 20
            )
        };
        let (class, fallback, client, end) = match row {
            Some(row) => (
                index_of(format!("key {}", row.class)),
                row.fallback
                    .as_ref()
                    .map(|fallback| index_of(format!("key {fallback}"))),
                index_of(format!("key {}", row.client)),
                row.end,
            ),
            None => (
                index_of(
                    result
                        .class
                        .as_ref()
                        .map_or_else(booked, |class| format!("class {class}")),
                ),
                None,
                client,
                End::Ok,
            ),
        };
        let cpu_usec = row
            .and_then(|row| row.cpu_usec)
            .or(result.cpu_usec)
            .unwrap_or(cpu_usec);
        let peak = row.and_then(|row| row.peak_milli).or(result.peak_milli);
        let pressure = row
            .and_then(|row| row.pressure_usec)
            .or(result.pressure_usec);
        converted.jobs.push(Job {
            arrive_ms: times.arrive - first,
            wait_ms: times.wait,
            start_ms: times.start,
            wall_ms: times.wall,
            cpu_ms: cpu_usec.div_ceil(1000),
            peak_milli: peak.map(saturated),
            pressure_ms: pressure.map(|usec| usec / 1000),
            memory_mib: saturated(record.peak_bytes.unwrap_or(0).div_ceil(1 << 20)),
            booked_milli: saturated(record.booked_milli),
            booked_mib: saturated(record.booked_bytes >> 20),
            host,
            end,
            class,
            fallback,
            client,
        });
    }
    Ok(converted)
}

/// `value` as a `u32`, held at its largest.
fn saturated(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record of `job` on `host` that ended at `ended`, ran `run` ms after waiting `wait`,
    /// booked a core and 4 GiB and used `cpu` µs and 1.5 GiB.
    fn line(job: &str, host: &str, ended: u64, run: u64, wait: u64, cpu: u64) -> String {
        format!(
            "{{\"booked\":{{\"cpuMillis\":\"1000\",\"jobId\":\"{job}\",\"memoryBytes\":\"4294967296\"}},\
             \"endedAtMs\":\"{ended}\",\"hostId\":\"{host}\",\"jobId\":\"{job}\",\
             \"queueWaitMs\":\"{wait}\",\"runMs\":\"{run}\",\
             \"used\":{{\"cpuUsec\":\"{cpu}\",\"peakMemoryBytes\":\"1610612736\"}}}}"
        )
    }

    #[test]
    fn result_lines_are_read_whatever_precedes_them() {
        let text = "Nov 22 host js-agent[1]: job: result run=k1-1-0 cpu_millis=250 \
                    memory_bytes=1 class=compile cpu_from=class memory_from=class \
                    cpu_usage_usec=2000000 peak_memory_bytes=1 cpu_periods=0 \
                    cpu_throttled_periods=0 throttled_permille=0 cpu_pressure_some_usec=1500 \
                    cpu_peak_millicores=20000\n\
                    job: result run=k1-2-0 class=- cpu_usage_usec=- cpu_peak_millicores=x\n\
                    job: result cpu_millis=250\n\
                    something else entirely\n\
                    job: result run=k1-2-0 class=test cpu_usage_usec=7\n";
        let read = results(text);
        assert_eq!(read.len(), 2);
        assert_eq!(
            read["k1-1-0"],
            AgentResult {
                class: Some("compile".to_owned()),
                cpu_usec: Some(2_000_000),
                pressure_usec: Some(1500),
                peak_milli: Some(20_000),
            }
        );
        // The later line of a job wins.
        assert_eq!(
            read["k1-2-0"],
            AgentResult {
                class: Some("test".to_owned()),
                cpu_usec: Some(7),
                ..AgentResult::default()
            }
        );
    }

    /// Asserts that a class the agent quoted is read whole, a build target's full name with
    /// its spaces, slashes and colon, and the words after it still read; that `\"` and `\\`
    /// in it read as `"` and `\`; that a quoted `-` is a class while a bare one is not; that a
    /// word without `=` is skipped; and that a line that ends inside a quote (or after a lone
    /// `\` in one) keeps what it read. Catches a full class cut at its first space, its words
    /// read as other fields, and a class that is `-` lost.
    #[test]
    fn a_quoted_class_is_read_whole() {
        let text = "job: result run=r1 class=\"komira//src/json:json mojo_compile build\" \
                    cpu_from=class cpu_usage_usec=9\n\
                    job: result run=r2 stray class=\"say \\\"hi\\\" a\\\\b\" cpu_usage_usec=3\n\
                    job: result run=r3 class=\"-\"\n\
                    job: result run=r4 cpu_usage_usec=4 class=\"open to the end\n\
                    job: result run=r5 class=\"lone\\";
        let read = results(text);
        let class = |run: &str| read[run].class.clone();
        assert_eq!(
            class("r1").as_deref(),
            Some("komira//src/json:json mojo_compile build")
        );
        assert_eq!(read["r1"].cpu_usec, Some(9));
        assert_eq!(class("r2").as_deref(), Some("say \"hi\" a\\b"));
        assert_eq!(read["r2"].cpu_usec, Some(3));
        assert_eq!(class("r3").as_deref(), Some("-"));
        assert_eq!(class("r4").as_deref(), Some("open to the end"));
        assert_eq!(read["r4"].cpu_usec, Some(4));
        assert_eq!(class("r5").as_deref(), Some("lone"));
    }

    #[test]
    fn records_become_an_anonymous_trace_in_arrival_order() {
        // b arrives first (ends at 9000 after 2000 ms of run and 3000 of wait: at 4000).
        let usage = [
            line("a", "node-2", 10_000, 4000, 0, 4_000_000),
            String::new(),
            line("b", "node-1", 9000, 2000, 3000, 1_000_000),
            line("c", "node-1", 9000, 2000, 3000, 1_000_000),
        ]
        .join("\n");
        let results = BTreeMap::from([(
            "a".to_owned(),
            AgentResult {
                class: Some("compile".to_owned()),
                cpu_usec: Some(4_000_400),
                pressure_usec: Some(2_500),
                peak_milli: Some(9000),
            },
        )]);
        let converted = convert(&usage, &results, &BTreeMap::new(), 0).unwrap();
        assert_eq!((converted.read, converted.with_result), (3, 1));
        let b = Job {
            arrive_ms: 0,
            wait_ms: 3000,
            start_ms: 0,
            wall_ms: 2000,
            cpu_ms: 1000,
            peak_milli: None,
            pressure_ms: None,
            memory_mib: 1536,
            booked_milli: 1000,
            booked_mib: 4096,
            host: 0,
            end: End::Ok,
            class: 1,
            fallback: None,
            client: 0,
        };
        let c = b.clone();
        let a = Job {
            arrive_ms: 2000,
            wait_ms: 0,
            wall_ms: 4000,
            cpu_ms: 4001,
            peak_milli: Some(9000),
            pressure_ms: Some(2),
            host: 1,
            class: 2,
            ..b.clone()
        };
        assert_eq!(converted.jobs, vec![b, c, a]);
    }

    #[test]
    fn records_that_did_not_run_ran_elsewhere_or_before_the_cut_are_counted_and_left_out() {
        let mut usage: Vec<String> = (0..99)
            .map(|n| line(&format!("j{n}"), "node-1", 100_000 + n, 1000, 0, 1000))
            .collect();
        usage.push(line("other", "mac-1", 100_000, 1000, 0, 1000));
        // Queued when it ended, so no run time and no usage.
        usage.push(
            "{\"booked\":{},\"endState\":\"JOB_STATE_QUEUED\",\"endedAtMs\":5,\"hostId\":\"node-1\",\
             \"jobId\":\"q\",\"queueWaitMs\":\"5\"}"
                .to_owned(),
        );
        // One record of 101 is under a hundredth: its host is left out.
        let all = convert(&usage.join("\n"), &BTreeMap::new(), &BTreeMap::new(), 0).unwrap();
        assert_eq!(
            (all.read, all.not_run, all.other_host, all.before_cut),
            (101, 1, 1, 0)
        );
        assert_eq!(all.jobs.len(), 99);
        // The first job started at 99 000 ms, before a cut at 99 001.
        let cut = convert(
            &usage.join("\n"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            99_001,
        )
        .unwrap();
        assert_eq!((cut.before_cut, cut.jobs.len()), (1, 98));
        // Jobs without a result line are classed by their booking: one class here.
        assert!(cut.jobs.iter().all(|job| (job.class, job.client) == (1, 0)));
        assert_eq!(saturated(u64::MAX), u32::MAX);
    }

    #[test]
    fn a_store_row_gives_a_job_its_times_keys_end_peak_and_contention() {
        // The record says it ran 4 s ending at 10 s after no wait; its row says it was
        // submitted at 1 s, placed at 2 s, started at 4.5 s and ended at 9 s.
        let usage = line("a", "node-1", 10_000, 4000, 0, 4_000_000);
        let row = Facts {
            class: "K".to_owned(),
            fallback: Some("F".to_owned()),
            client: "C".to_owned(),
            end: End::Memory,
            cpu_usec: Some(3_000_000),
            peak_milli: Some(5000),
            pressure_usec: Some(400_000),
            queued_ms: 1000,
            placed_ms: Some(2000),
            started_ms: Some(4500),
            ended_ms: Some(9000),
        };
        let results = BTreeMap::from([(
            "a".to_owned(),
            AgentResult {
                class: Some("ignored".to_owned()),
                peak_milli: Some(1),
                ..AgentResult::default()
            },
        )]);
        let rows = BTreeMap::from([("a".to_owned(), row.clone())]);
        let converted = convert(&usage, &results, &rows, 0).unwrap();
        assert_eq!((converted.with_row, converted.with_result), (1, 1));
        assert_eq!(
            converted.jobs,
            vec![Job {
                arrive_ms: 0,
                wait_ms: 1000,
                start_ms: 2500,
                wall_ms: 4500,
                cpu_ms: 3000,
                peak_milli: Some(5000),
                pressure_ms: Some(400),
                memory_mib: 1536,
                booked_milli: 1000,
                booked_mib: 4096,
                host: 0,
                end: End::Memory,
                // "client" is 0, then the row's class, fallback and client.
                class: 1,
                fallback: Some(2),
                client: 3,
            }]
        );
        // The cut is at the row's start; a row not yet placed falls back to the record.
        let late = convert(&usage, &results, &rows, 4501).unwrap();
        assert_eq!(late.before_cut, 1);
        let queued = BTreeMap::from([(
            "a".to_owned(),
            Facts {
                placed_ms: None,
                fallback: None,
                ..row
            },
        )]);
        let converted = convert(&usage, &BTreeMap::new(), &queued, 0).unwrap();
        assert_eq!(
            (
                converted.jobs[0].wait_ms,
                converted.jobs[0].start_ms,
                converted.jobs[0].wall_ms
            ),
            (0, 0, 4000)
        );
        assert_eq!(converted.jobs[0].fallback, None);
    }

    #[test]
    fn a_line_that_is_not_a_record_is_refused_by_number() {
        let good = line("a", "node-1", 10_000, 4000, 0, 1);
        for (bad, what) in [
            ("nope", "not a JSON object"),
            ("{\"hostId\":\"h\",\"endedAtMs\":1}", "no jobId"),
            ("{\"jobId\":\"j\",\"endedAtMs\":1}", "no hostId"),
            (
                "{\"jobId\":\"j\",\"hostId\":\"h\",\"endedAtMs\":\"x\"}",
                "no endedAtMs",
            ),
        ] {
            let usage = format!("{good}\n{bad}\n");
            let error = convert(&usage, &BTreeMap::new(), &BTreeMap::new(), 0).unwrap_err();
            assert_eq!(error, ConvertError { line: 2, what });
            assert_eq!(error.to_string(), format!("line 2 of the records: {what}"));
        }
    }
}
