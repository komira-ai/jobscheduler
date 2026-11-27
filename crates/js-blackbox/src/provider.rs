//! J5's judge: the calls the fake mount provider answered, as its control socket lists them, the
//! most of one kind in any window, and whether a job ended as its own fault.

use std::time::Duration;

use crate::JobSeen;

/// One call the fake mount provider answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCall {
    /// When it came, from the fake's start.
    pub at: Duration,
    /// `Prepare`, `Mount`, `Release`, `Sweep`, or `Restart` (the fake's own restart).
    pub method: String,
    /// `ok`, `unavailable`, `invalid` or `precondition`.
    pub answer: String,
}

/// The calls of the fake's `calls` reply: one line each, `<ms since start> <method> <answer>`.
///
/// # Errors
///
/// The first line that is not one, quoted.
pub fn provider_calls(listed: &str) -> Result<Vec<ProviderCall>, String> {
    listed
        .lines()
        .map(|line| {
            let mut words = line.split(' ');
            let (at, method, answer) = (words.next(), words.next(), words.next());
            match (
                at.and_then(|ms| ms.parse::<u64>().ok()),
                method,
                answer,
                words.next(),
            ) {
                (Some(ms), Some(method), Some(answer), None) => Ok(ProviderCall {
                    at: Duration::from_millis(ms),
                    method: method.to_owned(),
                    answer: answer.to_owned(),
                }),
                _ => Err(format!("not a call of the fake provider: {line:?}")),
            }
        })
        .collect()
}

/// The window that holds the most calls of one kind ([`busiest`]): how many, and where it
/// starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busiest {
    /// The calls in the window.
    pub count: usize,
    /// When the window's first call came, from the fake's start.
    pub from: Duration,
}

/// The window of `window` that holds the most calls of `method` in `calls` (in the order they
/// came): a call at `t` and one at `t + window` are in different windows.
#[must_use]
pub fn busiest(calls: &[ProviderCall], method: &str, window: Duration) -> Busiest {
    let times: Vec<Duration> = calls
        .iter()
        .filter(|call| call.method == method)
        .map(|call| call.at)
        .collect();
    let mut best = Busiest {
        count: 0,
        from: Duration::ZERO,
    };
    let mut start = 0;
    for (end, at) in times.iter().enumerate() {
        start += times[start..end]
            .iter()
            .take_while(|first| **first + window <= *at)
            .count();
        let count = end + 1 - start;
        best = match count > best.count {
            true => Busiest {
                count,
                from: times[start],
            },
            false => best,
        };
    }
    best
}

/// The calls of `method` per second, from the fake's start, as `second:count` pairs, for a
/// failure's report.
#[must_use]
pub fn per_second(calls: &[ProviderCall], method: &str) -> String {
    let mut seconds: Vec<(u64, usize)> = Vec::new();
    for call in calls.iter().filter(|call| call.method == method) {
        let second = call.at.as_secs();
        match seconds.last_mut() {
            Some((last, count)) if *last == second => *count += 1,
            _ => seconds.push((second, 1)),
        }
    }
    let pairs: Vec<String> = seconds
        .iter()
        .map(|(second, count)| format!("{second}:{count}"))
        .collect();
    pairs.join(" ")
}

/// Whether a job that ended, ended as its own fault: every end but a success, and a start that
/// failed because the host's input was unavailable (the host's fault, which the job service may
/// try elsewhere).
#[must_use]
pub fn own_fault(job: &JobSeen) -> bool {
    let succeeded = job.state == crate::SUCCEEDED;
    let input_unavailable = job.state == FAILED_TO_START && job.start_failure == INPUT_UNAVAILABLE;
    !(succeeded || input_unavailable)
}

/// The job service's word for a job whose start failed.
pub const FAILED_TO_START: &str = "JOB_STATE_FAILED_TO_START";

/// The job service's code for a start that failed because its input was unavailable on the host.
pub const INPUT_UNAVAILABLE: &str = "START_FAILURE_CODE_INPUT_UNAVAILABLE";

/// The agent's detail of a start the provider refused (`INVALID_ARGUMENT` or
/// `FAILED_PRECONDITION`): the client's to fix.
pub const PROVIDER_REFUSED: &str = "provider-refused";
