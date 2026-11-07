//! The live farm: the job service's JSON gateway and the server's host and usage reads,
//! through `js-client`.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use js_client::{Client, ClientError, REQUEST_LIMIT, Reply};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::farm::{Farm, FarmError, HostShape, JobAsk, JobStanding, LogTail, Phase, UsageRow};

/// The job server, reached with a bearer token: the jobs on its job port, the host list and
/// the usage rows on its main port.
pub struct HttpFarm {
    jobs: Client,
    client: Client,
}

impl HttpFarm {
    /// Checks the flags and builds the clients (see [`Client::connect`]), `job_server` for
    /// the job service (`--job-listen`) and `server` for hosts and usage; sends nothing.
    ///
    /// # Errors
    ///
    /// [`ClientError`] naming the unusable flag.
    pub fn connect(
        server: &str,
        job_server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            jobs: Client::connect(job_server, token_file, ca_cert, REQUEST_LIMIT)?,
            client: Client::connect(server, token_file, ca_cert, REQUEST_LIMIT)?,
        })
    }

    /// The outcome and the output of an ended job.
    fn ended(&self, id: &str, state: &str) -> Result<(String, LogTail), FarmError> {
        let reply = self
            .jobs
            .get(&format!("/v1/jobs/{id}/result"))
            .map_err(transport)?;
        let body = body_of(reply, &[200])?;
        let result: ResultEnvelope = serde_json::from_value(body).map_err(malformed)?;
        let result = result.result;
        let tail = LogTail {
            stdout: text_of(&result.stdout_tail)?,
            stderr: text_of(&result.stderr_tail)?,
            truncated: result.truncated,
        };
        Ok((outcome_of(state, &result.reason).to_owned(), tail))
    }
}

fn transport(err: ClientError) -> FarmError {
    FarmError::Transport(err.to_string())
}

fn malformed(err: serde_json::Error) -> FarmError {
    FarmError::Malformed(err.to_string())
}

/// The body of a reply whose status is one of `ok`, else the server's refusal.
fn body_of(reply: Reply, ok: &[u16]) -> Result<Value, FarmError> {
    match ok.contains(&reply.status) {
        true => Ok(reply.body),
        false => Err(FarmError::Refused {
            status: reply.status,
            code: reply
                .body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
    }
}

/// How an ended job ended, as the records name it: from its state and the reason its result
/// gives.
fn outcome_of(state: &str, reason: &str) -> &'static str {
    match (state, reason) {
        ("JOB_STATE_SUCCEEDED", _) => "succeeded",
        ("JOB_STATE_FAILED", "RESULT_REASON_MEMORY") => "out_of_memory",
        ("JOB_STATE_FAILED", "RESULT_REASON_WALL_LIMIT") => "wall_limit",
        ("JOB_STATE_FAILED", _) => "failed",
        ("JOB_STATE_CANCELLED", _) => "cancelled",
        ("JOB_STATE_LOST", _) => "lost",
        _ => "failed_to_start",
    }
}

/// The text of a base64 field of the gateway; bytes that are not UTF-8 read with the
/// replacement character.
fn text_of(field: &str) -> Result<String, FarmError> {
    let bytes = STANDARD
        .decode(field)
        .map_err(|err| FarmError::Malformed(err.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Deserialize)]
struct JobEnvelope {
    job: JobBody,
}

#[derive(Deserialize)]
struct JobBody {
    id: String,
    #[serde(default)]
    state: String,
    #[serde(default, rename = "hostId")]
    host_id: String,
}

#[derive(Deserialize)]
struct ResultEnvelope {
    result: ResultBody,
}

#[derive(Deserialize)]
struct ResultBody {
    #[serde(default)]
    reason: String,
    #[serde(default, rename = "stdoutTail")]
    stdout_tail: String,
    #[serde(default, rename = "stderrTail")]
    stderr_tail: String,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct HostsBody {
    hosts: Vec<HostBody>,
}

#[derive(Deserialize)]
struct HostBody {
    id: String,
    shape: ShapeBody,
}

#[derive(Deserialize)]
struct ShapeBody {
    cpu_millis: u64,
    memory_bytes: u64,
}

#[derive(Deserialize)]
struct RowsBody {
    rows: Vec<RowBody>,
}

#[derive(Deserialize)]
struct RowBody {
    minute: u64,
    cpu_millis: u64,
    ws_bytes: u64,
    leases: u64,
}

impl Farm for HttpFarm {
    fn submit(&self, ask: &JobAsk) -> Result<String, FarmError> {
        let needs = match ask.whole_host {
            true => json!({ "wholeHost": true }),
            false => json!({
                "cpuMillis": ask.cpu_millis.to_string(),
                "memoryBytes": ask.memory_bytes.to_string(),
            }),
        };
        let body = json!({ "spec": {
            "image": ask.image,
            "args": ask.args,
            "needs": needs,
            "wallLimitSeconds": ask.wall_limit_seconds.to_string(),
        }});
        let reply = self.jobs.post("/v1/jobs", &body).map_err(transport)?;
        let body = body_of(reply, &[201])?;
        let made: JobEnvelope = serde_json::from_value(body).map_err(malformed)?;
        Ok(made.job.id)
    }

    fn job(&self, id: &str) -> Result<JobStanding, FarmError> {
        let reply = self
            .jobs
            .get(&format!("/v1/jobs/{id}"))
            .map_err(transport)?;
        let body = body_of(reply, &[200])?;
        let job: JobEnvelope = serde_json::from_value(body).map_err(malformed)?;
        let job = job.job;
        let mut standing = JobStanding {
            state: job.state,
            host: Some(job.host_id).filter(|host| !host.is_empty()),
            outcome: None,
            log_tail: None,
        };
        if standing.phase() == Phase::Over {
            let (outcome, tail) = self.ended(id, &standing.state)?;
            standing.outcome = Some(outcome);
            standing.log_tail = Some(tail);
        }
        Ok(standing)
    }

    fn cancel(&self, id: &str) -> Result<(), FarmError> {
        let reply = self
            .jobs
            .post(&format!("/v1/jobs/{id}:cancel"), &json!({}))
            .map_err(transport)?;
        body_of(reply, &[200]).map(drop)
    }

    fn host(&self, id: &str) -> Result<HostShape, FarmError> {
        let reply = self.client.get("/v1/hosts").map_err(transport)?;
        let body = body_of(reply, &[200])?;
        let list: HostsBody = serde_json::from_value(body).map_err(malformed)?;
        let found = list.hosts.into_iter().find(|host| host.id == id);
        found
            .map(|host| HostShape {
                cpu_millis: host.shape.cpu_millis,
                memory_bytes: host.shape.memory_bytes,
            })
            .ok_or_else(|| FarmError::Malformed(format!("host {id} is not listed")))
    }

    fn usage_rows(&self, host: &str, from: u64, to: u64) -> Result<Vec<UsageRow>, FarmError> {
        let path = format!("/v1/usage/nodes/{host}?from={from}&to={to}");
        let reply = self.client.get(&path).map_err(transport)?;
        let body = body_of(reply, &[200])?;
        let rows: RowsBody = serde_json::from_value(body).map_err(malformed)?;
        Ok(rows
            .rows
            .into_iter()
            .map(|row| UsageRow {
                minute: row.minute,
                cpu_millis: row.cpu_millis,
                ws_bytes: row.ws_bytes,
                leases: row.leases,
            })
            .collect())
    }
}
