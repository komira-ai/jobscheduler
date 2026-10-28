//! What the usage tests share: a sink that keeps its rows in memory and records what it
//! is asked, a key source over a map, and the messages an agent sends with a load.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;

use js_lease_server::{
    DropReason, HostLimits, KeySource, LoadIn, NodeRow, Observation, Observed, ProfileKey, RunEnd,
    SinkCounters, UsageFault, UsageSink,
};
use js_types::{HostId, LeaseId};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, agent_args, post};
use crate::support::{Harness, Reply, Scratch, config};

/// One reading as the sink was given it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub host: String,
    pub epoch: u64,
    pub seq: u64,
    pub minute: u64,
}

#[derive(Default)]
struct State {
    seen: Vec<Seen>,
    open: BTreeMap<(String, u64), NodeRow>,
    written: BTreeMap<String, Vec<NodeRow>>,
    flushes: Vec<u64>,
    prunes: Vec<u64>,
    ended: Vec<RunEnd>,
    minutes: BTreeSet<(LeaseId, u16)>,
    drop_with: Option<DropReason>,
    fail: bool,
}

/// A sink over memory: a reading becomes a node row for the minute it was taken in, a
/// flush closes the rows of the minutes that are over and writes them.
#[derive(Default)]
pub struct Fake {
    state: Mutex<State>,
}

impl Fake {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state().seen.clone()
    }

    pub fn flushes(&self) -> Vec<u64> {
        self.state().flushes.clone()
    }

    pub fn prunes(&self) -> Vec<u64> {
        self.state().prunes.clone()
    }

    pub fn ended(&self) -> Vec<RunEnd> {
        self.state().ended.clone()
    }

    /// Makes every later reading a dropped one, for `reason`.
    pub fn drop_readings(&self, reason: DropReason) {
        self.state().drop_with = Some(reason);
    }

    /// Makes every flush, prune and run fail.
    pub fn fail(&self) {
        self.state().fail = true;
    }

    /// The rows written for `host`, oldest first.
    pub fn written(&self, host: &str) -> Vec<NodeRow> {
        self.state().written.get(host).cloned().unwrap_or_default()
    }
}

fn down() -> UsageFault {
    UsageFault("the store is down".to_string())
}

impl UsageSink for Fake {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        _: HostLimits,
        now_minute: u64,
    ) -> Observation {
        let mut state = self.state();
        state.seen.push(Seen {
            host: host.to_string(),
            epoch,
            seq: load.seq,
            minute: now_minute,
        });
        if let Some(reason) = state.drop_with {
            return Observation {
                observed: Observed::Dropped(reason),
                fresh: Vec::new(),
            };
        }
        let key = (host.to_string(), now_minute);
        let row = state.open.entry(key).or_insert(NodeRow {
            minute: now_minute,
            cpu_millis: 0,
            ws_max_bytes: 0,
            leases: 0,
        });
        row.cpu_millis = u32::try_from(load.cpu_usage_usec / 1000).unwrap();
        row.ws_max_bytes = row.ws_max_bytes.max(load.working_set_bytes);
        row.leases = u32::try_from(load.covered.len() + load.clocks.len()).unwrap();
        let fresh = load
            .minutes
            .iter()
            .filter(|m| state.minutes.insert((m.lease, m.minute)))
            .cloned()
            .collect();
        Observation {
            observed: Observed::Accepted,
            fresh,
        }
    }

    fn run_ended(&self, end: &RunEnd, _: u64) -> Result<(), UsageFault> {
        let mut state = self.state();
        if state.fail {
            return Err(down());
        }
        state.ended.push(end.clone());
        Ok(())
    }

    fn flush(&self, now_minute: u64) -> Result<Vec<(HostId, NodeRow)>, UsageFault> {
        let mut state = self.state();
        state.flushes.push(now_minute);
        if state.fail {
            return Err(down());
        }
        let due: Vec<(String, u64)> = state
            .open
            .keys()
            .filter(|(_, minute)| *minute < now_minute)
            .cloned()
            .collect();
        let mut closed = Vec::new();
        for key in due {
            let row = state.open.remove(&key).unwrap();
            state.written.entry(key.0.clone()).or_default().push(row);
            closed.push((HostId::new(&key.0).unwrap(), row));
        }
        Ok(closed)
    }

    fn prune(&self, now_minute: u64) -> Result<(), UsageFault> {
        let mut state = self.state();
        state.prunes.push(now_minute);
        match state.fail {
            true => Err(down()),
            false => Ok(()),
        }
    }

    fn counters(&self) -> SinkCounters {
        SinkCounters::default()
    }
}

/// A key source that names the key of each run id it was given.
pub struct Keys(pub BTreeMap<String, ProfileKey>);

impl KeySource for Keys {
    fn key_of(&self, run_id: &str) -> Option<ProfileKey> {
        self.0.get(run_id).cloned()
    }
}

/// A server with agents on and this sink, naming runs by `keys`, over `scratch` and the
/// arguments `args`.
pub async fn start_over(
    scratch: Scratch,
    args: Vec<std::ffi::OsString>,
    fake: &Arc<Fake>,
    keys: &[(&str, &str)],
) -> Harness {
    let map = keys
        .iter()
        .map(|(run, key)| (run.to_string(), ProfileKey::parse(key).unwrap()))
        .collect();
    let config = config(args)
        .unwrap()
        .with_usage_sink(fake.clone())
        .with_key_source(Arc::new(Keys(map)));
    Harness::over(scratch, config).await
}

/// A server with agents on (no static pool) and the fake as its sink.
pub async fn start_usage(name: &str, fake: &Arc<Fake>, keys: &[(&str, &str)]) -> Harness {
    let scratch = Scratch::new(name);
    let args = agent_args(&scratch, false);
    start_over(scratch, args, fake, keys).await
}

/// A load of the wire, with these minutes of lease `1-1`.
pub fn load(seq: u64, cpu_usage_usec: u64, minutes: &[u16]) -> Value {
    let minute = |m: &u16| {
        json!({
            "lease_id": "1-1", "minute": m, "cpu_mean_millis": 1500,
            "cpu_peak_window_millis": 2500, "ws_max_bytes": 4096,
        })
    };
    json!({
        "seq": seq,
        "mono_usec": seq * 5_000_000,
        "cpu_usage_usec": cpu_usage_usec,
        "working_set_bytes": 1_000_000,
        "covered": ["1-1"],
        "minutes": minutes.iter().map(minute).collect::<Vec<_>>(),
    })
}

/// A heartbeat of `session` that carries `load`.
pub async fn beat_load(h: &Harness, session: &Session, load: Value) -> Reply {
    let mut body = session.beat();
    body["load"] = load;
    post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await
}
