//! The registry's calls into the placer while a pass over the queued jobs holds the engine:
//! each returns without waiting for the engine, and the engine is told of it, in order, when
//! it is next taken. A test holds the engine on its own thread, as a pass does, and makes the
//! registry's calls on the same thread: a call that waited for the engine would never return.

use js_types::Resources;

use super::*;

/// A shape of 8 cores and 16 GiB, with an `arch`, which the scheduler needs.
fn shape() -> Shape {
    let caps = [("arch".to_owned(), "x86_64".to_owned())]
        .into_iter()
        .collect();
    Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A load reading of a host, the `seq`-th of its agent.
fn reading(seq: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 2_500_000,
        memory_working_set_bytes: 1 << 30,
        ..NodeLoad::default()
    }
}

/// A placer that knows `h1`, told while its engine was free.
fn placer_with_host() -> SchedPlacer {
    let placer = SchedPlacer::default();
    placer.host_up(&host("h1"), &shape());
    placer
}

/// Asserts that a heartbeat the registry tells while a pass holds the engine returns, and
/// reaches the engine once the pass lets go: the engine then charges the host exactly as a
/// placer told the same beat while its engine was free. A load reading that gives nothing up
/// is counted as no change either way. Catches a heartbeat that waits for the pass, which holds
/// the registry's lock, and with it every read call and heartbeat of the server, for as long as
/// the pass decides; and a kept beat that is lost.
#[test]
fn a_heartbeat_told_while_a_pass_holds_the_engine_returns_and_reaches_it_after() {
    let held = placer_with_host();
    let free = placer_with_host();
    let before = held.engine_changes();
    let pass = held.engine();
    held.heard(&host("h1"), Some(&reading(1)));
    drop(pass);
    free.heard(&host("h1"), Some(&reading(1)));
    let charged = held.load_of("h1");
    assert!(charged.is_some());
    assert_eq!(charged, free.load_of("h1"));
    assert_eq!(held.engine_changes(), before, "a reading alone");
    assert_eq!(free.engine_changes(), before, "a reading alone");
}

/// Asserts that the calls kept while a pass holds the engine reach it in the order the
/// registry made them: a hello then a loss leave the engine without the host, and a hello
/// after that brings it back. Catches kept calls told out of order, or only the last of
/// them.
#[test]
fn calls_kept_while_a_pass_holds_the_engine_reach_it_in_their_order() {
    let placer = SchedPlacer::default();
    let pass = placer.engine();
    placer.host_up(&host("h1"), &shape());
    placer.host_gone(&host("h1"));
    drop(pass);
    assert_eq!(placer.hosts_kept(), 0);
    let pass = placer.engine();
    placer.host_up(&host("h1"), &shape());
    drop(pass);
    assert_eq!(placer.hosts_kept(), 1);
}

/// Asserts the edge of what the placer holds: while a pass holds the engine it answers that it
/// holds work on every host, known or not, so the registry keeps a silent host it cannot ask
/// about; once the engine is free it answers from the engine again (no work on either).
/// Catches a placer that waits for the pass to answer, and one that answers "holds nothing"
/// unasked, which would let the registry forget a host that has a job's allocation.
#[test]
fn while_a_pass_holds_the_engine_the_placer_holds_work_on_every_host() {
    let placer = placer_with_host();
    let pass = placer.engine();
    assert!(placer.holds_work_on(&host("h1")));
    assert!(placer.holds_work_on(&host("unknown")));
    drop(pass);
    assert!(!placer.holds_work_on(&host("h1")));
    assert!(!placer.holds_work_on(&host("unknown")));
}

/// Asserts how the calls kept while a pass holds the engine count: each is counted when the
/// engine is next taken and told of it, by what it changes. A beat with nothing new, told to a
/// free engine or kept, changes nothing a pass reads and is not counted; a hello kept while the
/// engine is held is not counted while it waits, and is counted once the engine is taken again.
/// Catches a beat with nothing new counted (a pass after every heartbeat that found the engine
/// held), and a kept call that changes the engine left uncounted (a pass skipped for a host it
/// has not seen).
#[test]
fn a_kept_call_counts_when_it_is_told_by_what_it_changes() {
    let placer = placer_with_host();
    let before = placer.engine_changes();
    placer.heard(&host("h1"), None);
    assert_eq!(placer.engine_changes(), before, "told to a free engine");
    let pass = placer.engine();
    placer.heard(&host("h1"), None);
    drop(pass);
    drop(placer.engine());
    assert_eq!(placer.engine_changes(), before, "kept, then told");
    let pass = placer.engine();
    placer.host_up(&host("h2"), &shape());
    assert_eq!(placer.engine_changes(), before, "kept, not yet told");
    drop(pass);
    drop(placer.engine());
    assert_eq!(
        placer.engine_changes(),
        before + 1,
        "told when the engine was taken"
    );
    assert_eq!(placer.hosts_kept(), 2);
}

/// Asserts that a call finds an engine whose lock a panicking pass poisoned free, as the
/// engine's other takers do: the heartbeat is told at once and the host keeps its worker.
/// Catches a placer that keeps every call after a panic, so the engine never hears again.
#[test]
fn a_call_after_a_pass_panicked_tells_the_engine_at_once() {
    let placer = Arc::new(placer_with_host());
    let panicking = Arc::clone(&placer);
    let ended = std::thread::spawn(move || {
        let _pass = panicking.engine();
        panic!("a pass that panics while it holds the engine");
    })
    .join();
    assert!(ended.is_err());
    placer.heard(&host("h1"), Some(&reading(1)));
    assert!(placer.kept.lock().unwrap().is_empty());
    assert!(placer.load_of("h1").is_some());
}
