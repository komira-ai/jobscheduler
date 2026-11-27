//! The readers of what the tests are told: the job service's answers through its HTTP gateway
//! (the proto3 JSON of `ListHostsResponse`, `GetJobResponse` and `GetResultResponse`, where a
//! field at its default value is left out), what `docker load` prints, and the versions of a
//! package `apt-cache madison` lists.

use std::fmt;

use serde_json::Value;

/// A host as the job service lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSeen {
    /// `HOST_STATE_LIVE`, `HOST_STATE_LOST`, `HOST_STATE_AWAITING`, or `HOST_STATE_UNSPECIFIED`
    /// when the answer leaves the state out.
    pub state: String,
    /// The version its agent reported, empty when left out.
    pub agent_version: String,
}

/// The host `id` of a `GET /v1/hosts` answer, or `None` when it is not listed (or the answer
/// is not a host list).
#[must_use]
pub fn host(list: &Value, id: &str) -> Option<HostSeen> {
    list.get("hosts")?
        .as_array()?
        .iter()
        .find(|h| h.get("id").and_then(Value::as_str) == Some(id))
        .map(|h| HostSeen {
            state: text_or(h, "state", "HOST_STATE_UNSPECIFIED"),
            agent_version: text_or(h, "agentVersion", ""),
        })
}

/// A job as the job service answers `GET /v1/jobs/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSeen {
    /// `JOB_STATE_QUEUED` to `JOB_STATE_FAILED_TO_START`.
    pub state: String,
    /// The host it is placed on, empty when none.
    pub host_id: String,
    /// The number of its attempt (1 for the first), 0 when left out.
    pub attempt: u64,
    /// Whether the service says its host has been silent past its grace
    /// (`hostSilentSinceMs` given).
    pub host_silent: bool,
}

impl JobSeen {
    /// Whether the job has ended, whichever way.
    #[must_use]
    pub fn ended(&self) -> bool {
        matches!(
            self.state.as_str(),
            "JOB_STATE_SUCCEEDED"
                | "JOB_STATE_FAILED"
                | "JOB_STATE_CANCELLED"
                | "JOB_STATE_LOST"
                | "JOB_STATE_FAILED_TO_START"
        )
    }
}

/// The job of a `GET /v1/jobs/{id}` (or `POST /v1/jobs`) answer.
///
/// # Errors
///
/// A line saying the answer holds no job, or a job with no state.
pub fn job(answer: &Value) -> Result<JobSeen, String> {
    let job = answer
        .get("job")
        .filter(|j| j.is_object())
        .ok_or_else(|| format!("no job in {answer}"))?;
    let state = job
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("a job with no state: {job}"))?;
    Ok(JobSeen {
        state: state.to_owned(),
        host_id: text_or(job, "hostId", ""),
        attempt: job.get("attempt").and_then(Value::as_u64).unwrap_or(0),
        host_silent: job.get("hostSilentSinceMs").is_some(),
    })
}

/// A job's result as the job service answers `GET /v1/jobs/{id}/result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSeen {
    /// The job's end state.
    pub state: String,
    /// Its program's exit code, when it exited (an optional field: present at 0).
    pub exit_code: Option<i64>,
    /// The attempt the result is of, 0 when left out.
    pub attempt: u64,
}

/// The result of a `GET /v1/jobs/{id}/result` answer.
///
/// # Errors
///
/// A line saying the answer holds no result, or a result with no state.
pub fn result(answer: &Value) -> Result<ResultSeen, String> {
    let result = answer
        .get("result")
        .filter(|r| r.is_object())
        .ok_or_else(|| format!("no result in {answer}"))?;
    let state = result
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("a result with no state: {result}"))?;
    Ok(ResultSeen {
        state: state.to_owned(),
        exit_code: result.get("exitCode").and_then(Value::as_i64),
        attempt: result.get("attempt").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// The image `docker load` says it loaded: the `sha256:<64 hex>` of its line
/// `Loaded image ID: ...` (an image with no tag, as the js-lease-server image is) or
/// `Loaded image: ...`.
#[must_use]
pub fn loaded_image(printed: &str) -> Option<String> {
    printed.lines().find_map(|line| {
        let rest = line
            .strip_prefix("Loaded image ID: ")
            .or_else(|| line.strip_prefix("Loaded image: "))?
            .trim();
        let hex = rest.strip_prefix("sha256:")?;
        (hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| rest.to_owned())
    })
}

/// A release version, `MAJOR.MINOR.PATCH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `text` as a version, or `None` when it is not three whole numbers joined by dots.
    #[must_use]
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.split('.').map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse::<u64>().ok())
                .flatten()
        });
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Some(a)), Some(Some(b)), Some(Some(c)), None) => Some(Version(a, b, c)),
            _ => None,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The newest version below `released` of those `apt-cache madison` printed (lines
/// `package | version | source`), or `None` when it lists none below it.
#[must_use]
pub fn previous_version(madison: &str, released: Version) -> Option<Version> {
    madison
        .lines()
        .filter_map(|line| Version::parse(line.split('|').nth(1)?.trim()))
        .filter(|v| *v < released)
        .max()
}

fn text_or(object: &Value, key: &str, absent: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(absent)
        .to_owned()
}
