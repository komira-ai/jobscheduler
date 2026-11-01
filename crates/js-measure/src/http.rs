//! The live farm: the lease server's HTTP API through `js-client`.

use std::path::Path;

use js_client::{Client, ClientError, LogTail, REQUEST_LIMIT, Reply};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::farm::{
    Acquired, Curve, Farm, FarmError, HostShape, LeaseAsk, RunAsk, RunStanding, Standing, UsageRow,
};

/// The lease server, reached with a bearer token.
pub struct HttpFarm {
    client: Client,
}

impl HttpFarm {
    /// Checks the flags and builds the client (see [`Client::connect`]); sends nothing.
    ///
    /// # Errors
    ///
    /// [`ClientError`] naming the unusable flag.
    pub fn connect(
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            client: Client::connect(server, token_file, ca_cert, REQUEST_LIMIT)?,
        })
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

#[derive(Deserialize)]
struct GrantedBody {
    id: String,
    host: Option<String>,
}

#[derive(Deserialize)]
struct LeasesBody {
    leases: Vec<LeaseEntry>,
}

#[derive(Deserialize)]
struct LeaseEntry {
    id: String,
    state: String,
    host: Option<String>,
}

#[derive(Deserialize)]
struct QueuedBody {
    run_id: String,
}

#[derive(Deserialize)]
struct RunBody {
    state: String,
    outcome: Option<String>,
    log_tail: Option<LogTail>,
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

#[derive(Deserialize)]
struct KeyBody {
    runs: u64,
    curve: Option<Vec<PointBody>>,
}

#[derive(Deserialize)]
struct PointBody {
    cores_milli: Option<u64>,
    ws_bytes: u64,
}

impl Farm for HttpFarm {
    fn acquire(&self, ask: &LeaseAsk) -> Result<Acquired, FarmError> {
        let body = json!({
            "class": ask.class,
            "resources": { "cpu_millis": ask.cpu_millis, "memory_bytes": ask.memory_bytes },
            "max_wall_seconds": ask.max_wall_seconds,
        });
        let reply = self.client.post("/v1/leases", &body).map_err(transport)?;
        let status = reply.status;
        let body = body_of(reply, &[201, 202])?;
        let granted: GrantedBody = serde_json::from_value(body).map_err(malformed)?;
        Ok(match (status, granted.host) {
            (201, Some(host)) => Acquired::Held {
                id: granted.id,
                host,
            },
            _ => Acquired::Pending { id: granted.id },
        })
    }

    fn lease(&self, id: &str) -> Result<Standing, FarmError> {
        let reply = self.client.get("/v1/leases").map_err(transport)?;
        let body = body_of(reply, &[200])?;
        let list: LeasesBody = serde_json::from_value(body).map_err(malformed)?;
        let entry = list.leases.into_iter().find(|lease| lease.id == id);
        Ok(match entry {
            Some(LeaseEntry {
                state,
                host: Some(host),
                ..
            }) if state == "held" => Standing::Held { host },
            Some(entry) if entry.state == "pending" => Standing::Pending,
            _ => Standing::Gone,
        })
    }

    fn start_run(&self, lease: &str, ask: &RunAsk) -> Result<String, FarmError> {
        let body = json!({
            "image": ask.image,
            "args": ask.args,
            "limits": {
                "cpu_millis": ask.cpu_millis,
                "memory_bytes": ask.memory_bytes,
                "wall_ms": ask.wall_ms,
            },
            "profile": ask.profile,
        });
        let path = format!("/v1/leases/{lease}/runs");
        let reply = self.client.post(&path, &body).map_err(transport)?;
        let body = body_of(reply, &[202])?;
        let queued: QueuedBody = serde_json::from_value(body).map_err(malformed)?;
        Ok(queued.run_id)
    }

    fn run(&self, lease: &str, run: &str) -> Result<RunStanding, FarmError> {
        let path = format!("/v1/leases/{lease}/runs/{run}");
        let reply = self.client.get(&path).map_err(transport)?;
        let body = body_of(reply, &[200])?;
        let run: RunBody = serde_json::from_value(body).map_err(malformed)?;
        Ok(RunStanding {
            state: run.state,
            outcome: run.outcome,
            log_tail: run.log_tail,
        })
    }

    fn release(&self, lease: &str) -> Result<(), FarmError> {
        let reply = self
            .client
            .delete(&format!("/v1/leases/{lease}"))
            .map_err(transport)?;
        body_of(reply, &[200, 202]).map(drop)
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

    fn curve(&self, profile: &str) -> Result<Option<Curve>, FarmError> {
        let reply = self
            .client
            .get(&format!("/v1/usage/keys/{profile}"))
            .map_err(transport)?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = body_of(reply, &[200])?;
        let key: KeyBody = serde_json::from_value(body).map_err(malformed)?;
        Ok(key.curve.map(|points| Curve {
            runs: key.runs,
            ws_bytes: points.iter().map(|p| p.ws_bytes).collect(),
            cores_milli: points.iter().filter_map(|p| p.cores_milli).collect(),
        }))
    }
}
