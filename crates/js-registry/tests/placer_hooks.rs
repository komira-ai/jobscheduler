//! What a placer is told of the registry's books between starts: a host that says hello
//! (`Placer::host_up`), an accepted heartbeat with the reading it brought
//! (`Placer::heard`), a stopped lease (`Placer::ended`), the full listing beside the
//! candidates of a start (`Placer::pick_over`), and the CPU slack the book takes from it
//! (`Placer::cpu_slack_permille`).

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_agent_wire::{LoadDrops, NodeLoad as Wire};
use js_pool_static::Candidate;
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{MarkedTails, Placer, PoolConfig};
use js_types::sched::usage::NodeLoad;
use js_types::{Admission, ClassId, Dims, HostId, LeaseId, Request, ResourceClass, Resources};
use js_types::{ProviderId, Shape};
use support::*;

/// A placer that books the first candidate, may read the full listing, lets shared CPU
/// go `slack` thousandths over, and writes down what it is told.
#[derive(Default)]
struct Told {
    slack: u32,
    /// Read the listing at each start.
    reads: bool,
    said: Mutex<Vec<String>>,
}

impl Told {
    fn note(&self, line: String) {
        self.said.lock().unwrap().push(line);
    }

    fn said(&self) -> Vec<String> {
        std::mem::take(&mut *self.said.lock().unwrap())
    }
}

impl Placer for Told {
    fn pick(&self, _: &StartSpec, _: &[Candidate], _: &MarkedTails) -> Option<usize> {
        self.note("pick".to_owned());
        Some(0)
    }

    fn pick_over(
        &self,
        spec: &StartSpec,
        open: &[Candidate],
        marked: &MarkedTails,
        every: &dyn Fn() -> Vec<Candidate>,
    ) -> Option<usize> {
        if self.reads {
            let listed: Vec<String> = every()
                .iter()
                .map(|c| format!("{}:{}", c.host.as_str(), c.leases.len()))
                .collect();
            self.note(format!("every {}", listed.join(" ")));
        }
        self.pick(spec, open, marked)
    }

    fn cpu_slack_permille(&self) -> u32 {
        self.slack
    }

    fn host_up(&self, host: &HostId, shape: &Shape) {
        let cpu = shape.resources().cpu_millis();
        self.note(format!("up {} {cpu}", host.as_str()));
    }

    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        let seq = load.map_or("-".to_owned(), |load| load.seq.to_string());
        self.note(format!("heard {} {seq}", host.as_str()));
    }

    fn ended(&self, lease: LeaseId) {
        self.note(format!("ended {lease}"));
    }
}

/// The class that books part of a host.
fn book() -> ResourceClass {
    let id = ClassId::new("book").unwrap();
    ResourceClass::new(id, Admission::Book(Dims::new()), 0)
}

/// A `book` lease numbered `sequence` of `cpu_millis` and 1 MiB.
fn sharing(sequence: u64, cpu_millis: u64) -> StartSpec {
    let request = Request {
        class: book().id().clone(),
        max_wall: Duration::from_secs(600),
        resources: Resources::new(cpu_millis, 1 << 20, 0, None).unwrap(),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[book()]).unwrap()
}

/// A pool of `book` leases (4 cores per host) with the placer `told` set.
fn pool(told: Arc<Told>) -> Fixture {
    let name = ProviderId::new("builders").unwrap();
    let fx = fixture_of(PoolConfig::new(name, book(), pool_shape(), 4));
    fx.registry.set_placer(told);
    fx
}

/// A load reading numbered `seq`; `bad` makes its working set one byte past the most the
/// vet takes, so the vet drops it.
fn reading(seq: u64, bad: bool) -> Wire {
    Wire {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 1_000_000,
        working_set_bytes: if bad { MAX_MEMORY_BYTES + 1 } else { 1 << 30 },
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        dropped: LoadDrops::default(),
    }
}

/// Catches: a hook that is not called where the registry changes its books, called with
/// the claim instead of the pool's shape, or handed a reading that is not new (a repeated
/// `seq` handed again would read to a placer as a counter that went back). The placer
/// hears the hello with the pool's 4 cores (the agent claims 8), each accepted beat with
/// the reading only when it brought a new one that passed the vet, nothing of a beat the
/// registry refuses, and the stop of a lease once it is stopped.
#[test]
fn the_placer_is_told_of_hellos_beats_and_stops() {
    let told = Arc::new(Told::default());
    let fx = pool(told.clone());
    let epoch = fx.join("a", "t");
    assert_eq!(told.said(), ["up a 4000"]);
    let beat = |load: Option<Wire>| {
        let heartbeat = js_agent_wire::Heartbeat {
            load,
            ..fx.heartbeat("a", epoch)
        };
        fx.registry.heartbeat(&heartbeat, &cred("t"))
    };
    beat(None).unwrap();
    beat(Some(reading(1, false))).unwrap();
    beat(Some(reading(1, false))).unwrap();
    beat(Some(reading(2, true))).unwrap();
    beat(Some(reading(3, false))).unwrap();
    let said = [
        "heard a -",
        "heard a 1",
        "heard a -",
        "heard a -",
        "heard a 3",
    ];
    assert_eq!(told.said(), said);
    let stale = js_agent_wire::Heartbeat {
        session: "not-the-session".to_owned(),
        ..fx.heartbeat("a", epoch)
    };
    assert!(fx.registry.heartbeat(&stale, &cred("t")).is_err());
    assert!(told.said().is_empty());
    let mut provider = fx.registry.provider().with_classes(vec![book()]);
    assert_eq!(started_on(&provider.start(sharing(1, 1000))), Some("a"));
    assert_eq!(told.said(), ["pick"]);
    provider.stop(&lease(1));
    assert_eq!(told.said(), [format!("ended {}", lease(1))]);
}

/// Catches: a listing that shows only the candidates (a full host left out) or leaves out
/// the leases on a host. With `a` full and `b` empty, the start's candidate is `b` alone,
/// and the listing read beside it shows both, `a` with its four leases.
#[test]
fn a_placer_may_read_every_listed_host_at_a_start() {
    let told = Arc::new(Told {
        reads: true,
        ..Told::default()
    });
    let fx = pool(told.clone());
    fx.join("a", "t");
    let mut provider = fx.registry.provider().with_classes(vec![book()]);
    for sequence in 1..=4 {
        assert_eq!(
            started_on(&provider.start(sharing(sequence, 1000))),
            Some("a")
        );
    }
    fx.join("b", "t");
    told.said();
    assert_eq!(started_on(&provider.start(sharing(5, 1000))), Some("b"));
    assert_eq!(told.said(), ["every a:4 b:0", "pick"]);
}

/// Catches: the placer's slack not handed to the book (a host whose cores are booked
/// refuses one more), handed for memory too, or handed by a second placer, which takes no
/// effect. At 250 thousandths a 4-core host takes five one-core leases and not a sixth;
/// with no slack (the default of a placer) it takes four.
#[test]
fn the_book_lets_shared_cpu_go_over_by_the_placers_slack() {
    let told = Arc::new(Told {
        slack: 250,
        ..Told::default()
    });
    let fx = pool(told);
    fx.registry.set_placer(Arc::new(Told {
        slack: 1000,
        ..Told::default()
    }));
    fx.join("a", "t");
    let mut provider = fx.registry.provider().with_classes(vec![book()]);
    for sequence in 1..=5 {
        assert_eq!(
            started_on(&provider.start(sharing(sequence, 1000))),
            Some("a")
        );
    }
    let sixth = provider.start(sharing(6, 1000));
    assert_eq!(sixth, StartAck::Refused(RefusalCode::NoCapacity));
    let fx = pool(Arc::new(Told::default()));
    fx.join("a", "t");
    let mut provider = fx.registry.provider().with_classes(vec![book()]);
    for sequence in 1..=4 {
        assert_eq!(
            started_on(&provider.start(sharing(sequence, 1000))),
            Some("a")
        );
    }
    let fifth = provider.start(sharing(5, 1000));
    assert_eq!(fifth, StartAck::Refused(RefusalCode::NoCapacity));
}
