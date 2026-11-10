//! What the tests that drive the job service share: the flags that turn it on, a job body
//! builder, the calls a client makes (submit, read, cancel) through its gateway, and the
//! messages an agent that speaks the per-job protocol sends about the jobs it holds.

use std::ffi::OsString;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, agent_args, hello_body, post, session};
use crate::support::{Harness, Reply, Scratch, TOKEN, request};

/// The base flags with agent registration on and the job service on
/// `listen`.
pub fn job_args(scratch: &Scratch, listen: &str) -> Vec<OsString> {
    let mut args = agent_args(scratch);
    args.push("--job-listen".into());
    args.push(listen.into());
    args
}

/// A server with agent registration on and the job service on a loopback port.
pub async fn job_server(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = job_args(&scratch, "127.0.0.1:0");
    Harness::with(scratch, args).await
}

/// The body of a `SubmitJob` call, built up from a job id: 1 core and 1 GiB, no disk, no
/// GPU, no constraint, the host shared, and a wall limit of ten minutes until a method
/// says otherwise.
#[derive(Debug, Clone)]
pub struct JobBody {
    id: String,
    cpu_millis: u64,
    memory_bytes: u64,
    disk_bytes: Option<u64>,
    gpus: Option<(u32, String)>,
    constraints: Option<Value>,
    wall_limit_seconds: u64,
    whole_host: bool,
}

impl JobBody {
    /// A job `id` that needs 1 core and 1 GiB for ten minutes.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpus: None,
            constraints: None,
            wall_limit_seconds: 600,
            whole_host: false,
        }
    }

    /// Needs `cpu_millis` thousandths of a core and `memory_bytes` of memory.
    pub fn needs(mut self, cpu_millis: u64, memory_bytes: u64) -> Self {
        self.cpu_millis = cpu_millis;
        self.memory_bytes = memory_bytes;
        self
    }

    /// Needs `disk_bytes` of disk.
    pub fn disk(mut self, disk_bytes: u64) -> Self {
        self.disk_bytes = Some(disk_bytes);
        self
    }

    /// Needs `count` GPUs of `class`.
    pub fn gpus(mut self, count: u32, class: &str) -> Self {
        self.gpus = Some((count, class.to_owned()));
        self
    }

    /// Carries `constraints`, the JSON of the spec's `constraints` message (os, arch,
    /// isaLevel, accelerator).
    pub fn constraints(mut self, constraints: Value) -> Self {
        self.constraints = Some(constraints);
        self
    }

    /// Is limited to `seconds` of wall time.
    pub fn wall(mut self, seconds: u64) -> Self {
        self.wall_limit_seconds = seconds;
        self
    }

    /// Needs a host to itself, and nothing else: the other needs are not sent.
    pub fn whole_host(mut self) -> Self {
        self.whole_host = true;
        self
    }

    /// The `needs` of a job that shares its host.
    fn needs_json(&self) -> Value {
        let mut needs = json!({
            "cpuMillis": self.cpu_millis.to_string(),
            "memoryBytes": self.memory_bytes.to_string(),
        });
        if let Some(disk) = self.disk_bytes {
            needs["diskBytes"] = json!(disk.to_string());
        }
        if let Some((count, class)) = &self.gpus {
            needs["gpus"] = json!(count);
            needs["gpuClass"] = json!(class);
        }
        needs
    }

    /// The JSON the gateway takes: a `spec` with the fields set.
    pub fn json(&self) -> Value {
        // The service takes a whole-host need with no other need beside it.
        let needs = match self.whole_host {
            true => json!({ "wholeHost": true }),
            false => self.needs_json(),
        };
        let mut spec = json!({
            "jobId": self.id,
            "image": format!("team/app@sha256:{}", "a".repeat(64)),
            "command": "run",
            "needs": needs,
            "wallLimitSeconds": self.wall_limit_seconds.to_string(),
        });
        if let Some(constraints) = &self.constraints {
            spec["constraints"] = constraints.clone();
        }
        json!({ "spec": spec })
    }
}

/// `SubmitJob` with `body` through the job service's gateway on `h`'s job port.
pub async fn submit_body(h: &Harness, body: &Value) -> Reply {
    let job = h.running.job_addr().expect("the job service is on");
    let body = body.to_string().into_bytes();
    request(job, "POST", "/v1/jobs", Some(TOKEN), Some(&body)).await
}

/// `SubmitJob` of the job `id` through the job service's gateway on `h`'s job port: 1.5
/// cores and 1 GiB, no GPU, for ten minutes.
pub async fn submit(h: &Harness, id: &str) -> Reply {
    submit_body(h, &JobBody::new(id).needs(1500, 1 << 30).json()).await
}

/// The job service's answer about the job `id`.
pub async fn job_of(h: &Harness, id: &str) -> Reply {
    let addr = h.running.job_addr().expect("the job service is on");
    request(addr, "GET", &format!("/v1/jobs/{id}"), Some(TOKEN), None).await
}

/// The state name (`JOB_STATE_...`) the job service reports for the job `id`, which must be
/// known.
pub async fn job_state(h: &Harness, id: &str) -> String {
    let read = job_of(h, id).await;
    assert_eq!(read.status, 200, "{}", read.body);
    read.json()["job"]["state"].as_str().unwrap().to_owned()
}

/// `CancelJob` of the job `id` through the job service's gateway.
pub async fn cancel(h: &Harness, id: &str) -> Reply {
    let addr = h.running.job_addr().expect("the job service is on");
    let path = format!("/v1/jobs/{id}:cancel");
    request(addr, "POST", &path, Some(TOKEN), Some(b"{}")).await
}

/// The hello of a host of the shared pool that also speaks the per-job messages and
/// reports the arch `x86_64`.
pub fn per_job_hello(host: &str) -> Value {
    let mut hello = hello_body(host);
    hello["capabilities"]["per-job"] = json!("true");
    hello["capabilities"]["arch"] = json!("x86_64");
    hello
}

/// Sends `hello` (an agent that speaks the per-job messages) and returns the session it
/// opened; the hello must be admitted.
pub async fn job_agent(h: &Harness, hello: &Value) -> Session {
    let host = hello["host_id"].as_str().unwrap().to_owned();
    session(&post(h, "/v1/agent/hello", Some(JOIN), hello).await, &host)
}

/// The heartbeat of `agent` listing `jobs` (each a `(job id, state, result)`), answered 200.
pub async fn beat_with(h: &Harness, agent: &Session, jobs: &[Value]) -> Value {
    let mut body = agent.beat();
    body["jobs"] = json!(jobs);
    let reply = post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

/// A job `id` the agent lists as running.
pub fn running(id: &str) -> Value {
    json!({ "job_id": id, "state": "running" })
}

/// A job `id` the agent lists as ended with exit code 0, its result not yet acknowledged,
/// under the allocation `allocation`.
pub fn exited(id: &str, allocation: &str) -> Value {
    json!({
        "job_id": id,
        "state": "exited_unacked",
        "result": {
            "job_id": id,
            "allocation_id": allocation,
            "outcome": "succeeded",
            "exit_code": 0,
        },
    })
}

/// The `(job id, allocation id)` of each start in `reply`, and the job ids it stops.
pub fn commands(reply: &Value) -> (Vec<(String, String)>, Vec<String>) {
    let mut starts = Vec::new();
    let mut stops = Vec::new();
    for command in reply["commands"].as_array().into_iter().flatten() {
        let job = command["job_id"].as_str().unwrap_or_default().to_owned();
        match command["type"].as_str() {
            Some("job_start") => {
                let allocation = command["allocation_id"].as_str().unwrap().to_owned();
                starts.push((job, allocation));
            }
            Some("job_stop") => stops.push(job),
            _ => {}
        }
    }
    (starts, stops)
}

/// The one start `reply` carries.
pub fn one_start(reply: &Value) -> (String, String) {
    let (starts, stops) = commands(reply);
    assert!(stops.is_empty(), "{reply}");
    assert_eq!(starts.len(), 1, "{reply}");
    starts.into_iter().next().unwrap()
}
