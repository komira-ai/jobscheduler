//! The reservation tests' world: a pool of hosts, a polling consumer and a stream of
//! `BookAll` leases, on a manual clock.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::*;
use js_agent_wire::{Command, Isolation, JobSpec, Limits, Taint};
use js_lease::{Clock, Timestamp};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, RegisteredProvider, Reservation, WholeNodeConfig};
use js_types::{
    Admission, ClassId, HostId, LeaseId, ProviderId, Request, RequestId, ResourceClass,
};

pub const POLL_MS: u64 = 5_000;
pub const HOUR_S: u64 = 3_600;

pub fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

pub fn share_class() -> ResourceClass {
    ResourceClass::new(ClassId::new("share").unwrap(), Admission::BookAll, 0)
}

pub fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

/// A job of one CPU for run `run`.
pub fn job(run: &str) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
        native: None,
    }
}

pub fn no_capacity() -> StartAck {
    StartAck::Refused(RefusalCode::NoCapacity)
}

pub fn names(reservations: &[Reservation]) -> Vec<(String, String)> {
    let pairs = reservations.iter();
    let pairs = pairs.map(|r| (r.request.to_string(), r.host.to_string()));
    pairs.collect()
}

/// A pool of up to four hosts, a consumer's polled whole-node requests and a stream of
/// `BookAll` leases next to them.
pub struct World {
    pub fx: Fixture,
    pub provider: RegisteredProvider,
    pub epochs: BTreeMap<String, u64>,
    pub silent: BTreeSet<String>,
    /// The whole-node requests the consumer polls: lease sequence, request name and wall
    /// time in seconds.
    pub asks: Vec<(u64, String, u64)>,
    /// The request name behind each lease sequence that is a whole-node request.
    pub owners: BTreeMap<u64, String>,
    /// Every command sent to any host.
    pub commands: Vec<Command>,
    pub seen_booked: BTreeMap<HostId, LeaseId>,
    pub seen_marks: Vec<Reservation>,
    pub max_draining: usize,
}

impl World {
    pub fn new(hosts: &[&str], settings: impl FnOnce(WholeNodeConfig) -> WholeNodeConfig) -> Self {
        let name = ProviderId::new("builders").unwrap();
        let config = PoolConfig::new(name, whole(), pool_shape(), 4);
        let settings = settings(WholeNodeConfig::dedicated(4));
        Self::of(config.with_whole_node(settings).unwrap(), hosts)
    }

    pub fn of(config: PoolConfig, hosts: &[&str]) -> Self {
        let max_draining = config.whole_node().map_or(1, |w| w.max_draining);
        let fx = fixture_of(config);
        let provider = fx
            .registry
            .provider()
            .with_classes(vec![share_class(), whole()]);
        let mut world = Self {
            fx,
            provider,
            epochs: BTreeMap::new(),
            silent: BTreeSet::new(),
            asks: Vec::new(),
            owners: BTreeMap::new(),
            commands: Vec::new(),
            seen_booked: BTreeMap::new(),
            seen_marks: Vec::new(),
            max_draining,
        };
        hosts.iter().for_each(|host| world.join(host));
        world
    }

    pub fn join(&mut self, host: &str) {
        let epoch = self.fx.join(host, "t");
        self.epochs.insert(host.to_string(), epoch);
        self.check();
    }

    pub fn now(&self) -> Timestamp {
        self.fx.clock.now()
    }

    pub fn reservations(&self) -> Vec<Reservation> {
        self.fx.registry.reservations()
    }

    pub fn reservation(&self, name: &str) -> Option<Reservation> {
        let all = self.reservations();
        all.into_iter().find(|r| r.request == id(name))
    }

    pub fn reserved_host(&self, name: &str) -> Option<String> {
        self.reservation(name).map(|r| r.host.to_string())
    }

    pub fn lease_on(&self, host: &str) -> Option<LeaseId> {
        self.provider.booked().get(&self.host_id(host)).copied()
    }

    pub fn host_id(&self, name: &str) -> HostId {
        host(name)
    }

    pub fn whole_spec(sequence: u64, name: Option<&str>, wall_s: u64) -> StartSpec {
        let request = Request {
            class: whole().id().clone(),
            max_wall: Duration::from_secs(wall_s),
            ..request()
        };
        let spec = StartSpec::new(lease(sequence), request, &[whole()]).unwrap();
        match name {
            Some(name) => spec.with_request_id(id(name)),
            None => spec,
        }
    }

    pub fn share_spec(sequence: u64, wall_s: u64) -> StartSpec {
        let request = Request {
            class: ClassId::new("share").unwrap(),
            max_wall: Duration::from_secs(wall_s),
            ..request()
        };
        StartSpec::new(lease(sequence), request, &[share_class()]).unwrap()
    }

    /// A `BookAll` lease of `wall_s` seconds, started now.
    pub fn share(&mut self, sequence: u64, wall_s: u64) -> StartAck {
        let ack = self.provider.start(Self::share_spec(sequence, wall_s));
        self.check();
        ack
    }

    /// Starts whole-node request `name` for lease `sequence` and keeps polling it.
    pub fn ask(&mut self, sequence: u64, name: &str) -> StartAck {
        self.ask_for(sequence, name, 600)
    }

    pub fn ask_for(&mut self, sequence: u64, name: &str, wall_s: u64) -> StartAck {
        self.asks.push((sequence, name.to_string(), wall_s));
        self.owners.insert(sequence, name.to_string());
        self.poll(sequence, name, wall_s)
    }

    pub fn poll(&mut self, sequence: u64, name: &str, wall_s: u64) -> StartAck {
        let ack = self
            .provider
            .start(Self::whole_spec(sequence, Some(name), wall_s));
        self.check();
        ack
    }

    /// The consumer stops polling `name` and cancels it.
    pub fn cancel(&mut self, name: &str) -> bool {
        self.asks.retain(|(_, asked, _)| asked != name);
        let cancelled = self.fx.registry.cancel_request(&id(name));
        self.check();
        cancelled
    }

    pub fn stop(&mut self, sequence: u64) {
        self.stop_lease(&lease(sequence));
    }

    pub fn stop_lease(&mut self, held: &LeaseId) {
        self.provider.stop(held);
        self.check();
    }

    /// Moves the clock by `ms`, five seconds at a time: every host that is not silent
    /// beats and every request not yet booked polls its start, at each step and at the
    /// last instant.
    pub fn step(&mut self, ms: u64) {
        let mut left = ms;
        while left > 0 {
            let part = left.min(POLL_MS);
            left -= part;
            self.fx.clock.advance(Duration::from_millis(part));
            let hosts: Vec<(String, u64)> = self.epochs.clone().into_iter().collect();
            for (name, epoch) in hosts {
                if !self.silent.contains(&name) {
                    let reply = self
                        .fx
                        .registry
                        .heartbeat(&self.fx.heartbeat(&name, epoch), &cred("t"))
                        .unwrap();
                    self.commands.extend(reply.commands);
                }
            }
            self.check();
            for (sequence, name, wall_s) in self.asks.clone() {
                let booked = self.provider.booked();
                if !booked.values().any(|held| *held == lease(sequence)) {
                    self.poll(sequence, &name, wall_s);
                }
            }
        }
    }

    /// Asserts, after one call: a host carries at most one mark and a request holds at
    /// most one; at most `max_draining` marks drain; a lease that appeared on a host
    /// marked before the call is that mark's request's own.
    pub fn check(&mut self) {
        let marks = self.reservations();
        let hosts: BTreeSet<_> = marks.iter().map(|m| m.host.clone()).collect();
        let requests: BTreeSet<_> = marks.iter().map(|m| m.request.clone()).collect();
        assert_eq!(hosts.len(), marks.len(), "a host carries two marks");
        assert_eq!(requests.len(), marks.len(), "a request holds two marks");
        let draining = marks.iter().filter(|m| !m.assigned).count();
        assert!(
            draining <= self.max_draining,
            "more draining marks than allowed"
        );
        let booked = self.provider.booked();
        for (on, held) in &booked {
            if self.seen_booked.get(on) != Some(held) {
                let before = self.seen_marks.iter().find(|m| m.host == *on);
                let owner = self
                    .owners
                    .iter()
                    .find(|(sequence, _)| lease(**sequence) == *held)
                    .map(|(_, name)| id(name));
                if let Some(before) = before {
                    assert_eq!(
                        Some(&before.request),
                        owner.as_ref(),
                        "a lease landed on a marked host"
                    );
                }
            }
        }
        self.seen_booked = booked;
        self.seen_marks = marks;
    }

    pub fn taint(&mut self, name: &str) {
        let epoch = self.epochs[name];
        let mut message = self.fx.heartbeat(name, epoch);
        message.taint = Some(Taint {
            reason: "check".into(),
            run_id: None,
            check: None,
        });
        self.fx.registry.heartbeat(&message, &cred("t")).unwrap();
        self.check();
    }
}

pub fn drain_deadline(ms: u64) -> impl FnOnce(WholeNodeConfig) -> WholeNodeConfig {
    move |w| WholeNodeConfig {
        drain_deadline_ms: Some(ms),
        ..w
    }
}

pub fn plain(w: WholeNodeConfig) -> WholeNodeConfig {
    w
}
