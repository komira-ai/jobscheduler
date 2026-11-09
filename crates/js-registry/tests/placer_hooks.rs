//! What a placer is told of the registry's hosts: a host that says hello
//! (`Placer::host_up`), an accepted heartbeat with the reading it brought (`Placer::heard`), a
//! host that is forgotten or given back (`Placer::host_gone`), and what it is asked
//! (`Placer::holds_work_on`).

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_agent_wire::{LoadDrops, NodeLoad as Wire};
use js_registry::{DEFAULT_LOST_RETENTION_MS, Placer};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Shape};
use support::*;

/// A placer that writes down what it is told.
#[derive(Default)]
struct Told {
    said: Mutex<Vec<String>>,
    /// The memory of the shape each host was listed with.
    memory: Mutex<Vec<u64>>,
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
    fn host_up(&self, host: &HostId, shape: &Shape) {
        let cpu = shape.resources().cpu_millis();
        self.memory
            .lock()
            .unwrap()
            .push(shape.resources().memory_bytes());
        self.note(format!("up {} {cpu}", host.as_str()));
    }

    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        let seq = load.map_or("-".to_owned(), |load| load.seq.to_string());
        self.note(format!("heard {} {seq}", host.as_str()));
    }

    fn host_gone(&self, host: &HostId) {
        self.note(format!("gone {}", host.as_str()));
    }
}

/// A pool of up to four hosts with the placer `told` set.
fn pool(told: Arc<Told>) -> Fixture {
    with_placer(config(4), told)
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

/// Catches: a hook that is not called where the registry changes its hosts, called with the
/// claim instead of the pool's shape, or handed a reading that is not new (a repeated `seq`
/// handed again would read to a placer as a counter that went back). The placer hears the
/// hello with the pool's 4 cores (the agent claims 8), each accepted beat with the reading
/// only when it brought a new one that passed the vet, and nothing of a beat the registry
/// refuses.
#[test]
fn the_placer_is_told_of_hellos_and_beats() {
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
}

/// Catches: a pool that books the whole memory for its placer still telling it the cut, showing
/// its hosts with the whole memory (a reader of the host listing would take it as bookable),
/// or no longer reporting the percent. At 90 the placer is told the host's whole 8 GiB, which
/// it cuts itself, while the host listing shows the cut and 90 percent.
#[test]
fn a_pool_that_books_whole_memory_tells_the_placer_the_whole_and_reports_the_percent() {
    let percent = js_types::MemoryPercent::new(90).unwrap();
    let config = config(4)
        .with_memory_percent(percent)
        .booking_whole_memory();
    let told = Arc::new(Told::default());
    let fx = with_placer(config, told.clone());
    fx.join("a", "t");
    assert_eq!(*told.memory.lock().unwrap(), [8 << 30]);
    let shown = fx.registry.hosts();
    assert_eq!(shown[0].shape.resources().memory_bytes(), 7_730_941_132);
    assert_eq!(shown[0].memory.percent(), percent);
    assert_eq!(shown[0].memory.bookable_bytes(), 7_730_941_132);
}

/// Catches: a host the registry forgets not told to the placer, so that a placer keeping
/// a picture of the hosts keeps every host it ever heard of. A host lost past the pool's
/// retention with no booking is told gone at the next call, once; a host given back is told
/// gone when it is removed, and not before.
#[test]
fn a_host_the_registry_forgets_is_told_gone() {
    let told = Arc::new(Told::default());
    let fx = pool(told.clone());
    fx.join("a", "t");
    let epoch = fx.join("b", "t");
    told.said();
    let silent = DEFAULT_GRACE_MS + DEFAULT_LOST_RETENTION_MS + 1;
    for _ in 0..4 {
        fx.clock.advance(Duration::from_millis(silent / 4 + 1));
        fx.beat("b", epoch);
    }
    let _ = fx.registry.hosts();
    let said = told.said();
    assert_eq!(
        said.iter().filter(|line| *line == "gone a").count(),
        1,
        "{said:?}"
    );
    assert!(!said.iter().any(|line| line == "gone b"), "{said:?}");
    let b = HostId::new("b").unwrap();
    fx.registry.drain(&b).unwrap();
    fx.registry.release_host(&b).unwrap();
    assert!(!told.said().iter().any(|line| line == "gone b"));
    fx.registry.remove(&b).unwrap();
    assert!(told.said().contains(&"gone b".to_owned()));
}

/// Catches: a registry that stops forgetting a host when its placer ignores the news (the
/// default of the call): with a placer that keeps no picture of the hosts, a host given
/// back is still removed.
#[test]
fn a_placer_that_ignores_gone_hosts_leaves_their_removal_as_it_was() {
    let fx = with_placer(config(4), Arc::new(Holding::default()));
    fx.join("a", "t");
    let a = HostId::new("a").unwrap();
    fx.registry.drain(&a).unwrap();
    fx.registry.release_host(&a).unwrap();
    assert_eq!(fx.registry.remove(&a), Ok(()));
    assert!(fx.registry.hosts().is_empty());
}
