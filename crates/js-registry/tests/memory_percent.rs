//! The share of a pool's memory that is booked: `PoolConfig::with_memory_percent`.

mod support;

use std::sync::{Arc, Mutex};

use js_registry::{Placer, PoolConfig};
use js_types::{ExecMode, HostId, MemoryPercent, Shape};
use support::*;

/// The memory the pool lists: 8 GiB.
const LISTED: u64 = 8 << 30;

/// What 90 percent of it books, rounded down: 7730941132.8 bytes.
const NINETY: u64 = LISTED * 90 / 100;

fn percent(value: u64) -> MemoryPercent {
    MemoryPercent::new(value).unwrap()
}

fn ninety() -> PoolConfig {
    config(4).with_memory_percent(percent(90))
}

/// A placer that writes down the memory of the shape each host is listed with.
#[derive(Default)]
struct Told {
    memory: Mutex<Vec<u64>>,
}

impl Placer for Told {
    fn host_up(&self, _: &HostId, shape: &Shape) {
        self.memory
            .lock()
            .unwrap()
            .push(shape.resources().memory_bytes());
    }
}

/// Asserts that at 90 a pool of 8 GiB hosts lists floor(8 GiB x 90 / 100) bytes as bookable,
/// although the host's own claim (16 GiB) is larger than both: the view shows the cut as its
/// shape and its bookable memory, and the claim apart. It catches a percent that is not
/// applied, one rounded up, and a listing that follows the claim.
#[test]
fn ninety_percent_lists_exactly_the_cut_memory() {
    assert_eq!(NINETY, 7_730_941_132);
    let fx = fixture_of(ninety());
    fx.join("a", "t");
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.shape.resources().memory_bytes(), NINETY);
    assert_eq!(view.memory.bookable_bytes(), NINETY);
    let claim = view.claim.as_ref().unwrap();
    assert_eq!(claim.resources().memory_bytes(), 16 << 30);
}

/// Asserts that a pool without a percent lists all of the listed memory: the default of 100
/// changes nothing.
#[test]
fn the_default_lists_all_of_the_listed_memory() {
    let fx = fixture(4);
    fx.join("a", "t");
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.shape.resources().memory_bytes(), LISTED);
    assert_eq!(view.memory.bookable_bytes(), LISTED);
    assert_eq!(config(4).memory_percent(), MemoryPercent::FULL);
}

/// Asserts that a host claiming far more than the pool lists is listed at the pool's numbers
/// all the same: the placer is told the whole listed memory (8 GiB, which it cuts to the
/// percent), never the 512 GiB the host claims, and the registry's own view shows the cut. It
/// catches a listing that follows the agent's claim, which would let an agent inflate what a
/// job may take.
#[test]
fn a_larger_claim_never_raises_what_is_listed() {
    let told = Arc::new(Told::default());
    let fx = with_placer(ninety(), told.clone());
    let mut big = hello("big");
    big.shape.memory_bytes = 512 << 30;
    fx.hello(&big, &cred("t")).unwrap();
    assert_eq!(*told.memory.lock().unwrap(), [LISTED]);
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.shape.resources().memory_bytes(), NINETY);
    assert_eq!(view.memory.bookable_bytes(), NINETY);
}

/// Asserts that a hello is held to no memory: a host that reports less than the pool lists
/// joins, and the percent is applied to what it reports. A host reporting the cut (90
/// percent of the listing) is booked 90 percent of that, one reporting one byte under the
/// listing is booked 90 percent of it, and one reporting exactly the listing is booked the
/// pool's cut; the placer is told each host's whole reported memory, which it cuts itself.
/// It catches a floor that still refuses the smaller hosts, a booking of memory the host
/// does not have, and a percent applied to the pool's memory rather than the host's.
#[test]
fn a_hello_reporting_less_memory_is_booked_the_percent_of_it() {
    let told = Arc::new(Told::default());
    let fx = with_placer(ninety(), told.clone());
    let cases = [("short", NINETY), ("under", LISTED - 1), ("exact", LISTED)];
    for (name, memory) in cases {
        let mut claim = hello(name);
        claim.shape.memory_bytes = memory;
        fx.hello(&claim, &cred("t")).unwrap();
        let hosts = fx.registry.hosts();
        let view = hosts.iter().find(|view| view.id.as_str() == name).unwrap();
        assert_eq!(view.memory.configured_bytes(), memory, "{name}");
        let cut = memory * 90 / 100;
        assert_eq!(view.memory.bookable_bytes(), cut, "{name}");
        assert_eq!(view.shape.resources().memory_bytes(), cut, "{name}");
    }
    assert_eq!(*told.memory.lock().unwrap(), [NINETY, LISTED - 1, LISTED]);
    assert_eq!(fx.registry.hosts().len(), 3);
}

/// Asserts that the registry's view lists the memory the pool lists, the percent and the
/// bookable memory, and that a pool left at the default lists the percent as 100. It
/// catches a view that shows the unreduced figure as the bookable one.
#[test]
fn the_view_shows_listed_percent_and_bookable_memory() {
    let fx = fixture_of(ninety());
    fx.join("a", "t");
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.memory.configured_bytes(), LISTED);
    assert_eq!(view.memory.percent().get(), 90);
    assert_eq!(view.memory.bookable_bytes(), NINETY);
    let plain = fixture(4);
    plain.join("a", "t");
    let view = &plain.registry.hosts()[0];
    assert_eq!(view.memory.percent(), MemoryPercent::FULL);
    assert_eq!(view.memory.bookable_bytes(), LISTED);
}

/// Asserts that the booked shape is the same whichever of the percent and the
/// execution mode is set first: the cut and the mode are both kept. It catches a setter
/// that rebuilds the booked shape from the unreduced one and drops the other.
#[test]
fn the_percent_and_the_mode_may_be_set_in_either_order() {
    let a = config(4)
        .with_memory_percent(percent(90))
        .with_exec(ExecMode::Container);
    let b = config(4)
        .with_exec(ExecMode::Container)
        .with_memory_percent(percent(90));
    assert_eq!(a, b);
    let fx = fixture_of(a);
    let mut claim = hello("a");
    claim.capabilities.extend(map(&[("runtime", "container")]));
    fx.hello(&claim, &cred("t")).unwrap();
    let booked = fx.registry.hosts()[0].shape.clone();
    assert_eq!(booked.resources().memory_bytes(), NINETY);
    assert_eq!(
        booked.capabilities().get("exec").map(String::as_str),
        Some("container")
    );
}
