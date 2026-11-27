//! The load of a host with no control groups, over fake counters.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use super::{CpuTicks, HostSampler, HostStatistics, VmPages};
use crate::lifecycle::RunningLease;
use crate::usage::{
    Collector, HostCpu, LeaseCgroup, NodeSampler, Outbox, Sample, SampleError, UsageClock,
};

/// Counters that give each CPU reading of `ticks` in turn (none once they are used up), and
/// `nonfile` as the memory.
struct Fake {
    ticks: RefCell<VecDeque<CpuTicks>>,
    per_second: u64,
    nonfile: Option<u64>,
}

impl Fake {
    fn new(ticks: &[CpuTicks], per_second: u64, nonfile: Option<u64>) -> Self {
        Self {
            ticks: RefCell::new(ticks.iter().copied().collect()),
            per_second,
            nonfile,
        }
    }
}

impl HostStatistics for Fake {
    fn cpu_ticks(&self) -> Option<CpuTicks> {
        self.ticks.borrow_mut().pop_front()
    }

    fn ticks_per_second(&self) -> u64 {
        self.per_second
    }

    fn nonfile_bytes(&self) -> Option<u64> {
        self.nonfile
    }
}

/// A clock that reads what the test set, shared with the test.
#[derive(Clone, Default)]
struct Shared(Rc<Cell<u64>>);

impl UsageClock for Shared {
    fn now_usec(&self) -> u64 {
        self.0.get()
    }
}

/// Ticks with `user`, `system` and `nice` busy and 1000 idle.
fn ticks(user: u32, system: u32, nice: u32) -> CpuTicks {
    CpuTicks {
        user,
        system,
        idle: 1_000,
        nice,
    }
}

/// A lease asked for by the collector.
fn lease(id: &str) -> LeaseCgroup {
    LeaseCgroup {
        lease_id: id.to_owned(),
        dir: format!("/actions/{id}"),
        started_usec: 0,
    }
}

/// Asserts that a sample is the host's busy time and non-file memory as the node's, with no
/// active file pages, no CPU limit and no lease (each lease asked for counted unreadable), at
/// the clock's reading. Catches a host's memory not taken as the working set (which placement
/// would read as an empty host), and a lease reported with numbers no group gave.
#[test]
fn a_sample_is_the_host_s_busy_time_and_memory_as_the_node_s() {
    let clock = Shared::default();
    clock.0.set(7_000_000);
    let fake = Fake::new(&[ticks(100, 50, 10)], 100, Some(3 << 30));
    let mut sampler = HostSampler::new(fake);
    let sample = sampler.sample(&clock, &[lease("1-1"), lease("1-2")]);
    assert_eq!(
        sample,
        Ok(Sample {
            mono_usec: 7_000_000,
            cpu_usage_usec: 1_600_000,
            host_cpu_usec: 1_600_000,
            host_cpu_pressure_some_usec: None,
            actions_oom_events: None,
            working_set_bytes: 3 << 30,
            host_nonfile_bytes: Some(3 << 30),
            active_file_bytes: 0,
            cpu_limited: false,
            leases: Vec::new(),
            unreadable_leases: 2,
        })
    );
}

/// Asserts that the busy time is user, system and nice (not idle), widened across a wrap of
/// the 32-bit counters, and only grows: a reading after one near the top of a counter adds
/// what it moved past the wrap. Catches idle time counted as busy, and a wrap read as a fall
/// (a busy time that drops by four billion ticks, which the server would take for a new host).
#[test]
fn the_busy_time_counts_the_busy_states_and_grows_across_a_wrap() {
    let near_top = u32::MAX - 9;
    let fake = Fake::new(
        &[ticks(near_top, 0, 0), ticks(10, 5, 0), ticks(10, 5, 3)],
        100,
        Some(1),
    );
    let mut sampler = HostSampler::new(fake);
    let busy = |sampler: &mut HostSampler<Fake>| sampler.host_counters().map(|cpu| cpu.busy_usec);
    let first = u64::from(near_top) * 10_000;
    assert_eq!(busy(&mut sampler), Some(first));
    assert_eq!(busy(&mut sampler), Some(first + 25 * 10_000));
    assert_eq!(busy(&mut sampler), Some(first + 28 * 10_000));
    assert_eq!(busy(&mut sampler), None);
}

/// Asserts the per-second counters: the busy time with no CPU pressure (a Mac has none), and
/// that a host with no ticks a second to say is counted at one a second rather than divided
/// by zero. Catches a pressure of zero invented for a host that has no such counter.
#[test]
fn the_host_counters_have_no_pressure_and_a_zero_rate_is_one() {
    let mut sampler = HostSampler::new(Fake::new(&[ticks(3, 2, 1)], 0, Some(1)));
    assert_eq!(
        sampler.host_counters(),
        Some(HostCpu {
            busy_usec: 6_000_000,
            pressure_some_usec: None,
        })
    );
}

/// Asserts that counters that cannot be read fail the sample, the CPU load before the memory,
/// with texts that name what could not be read and no path. Catches a zero invented for a host
/// that cannot be read.
#[test]
fn counters_that_cannot_be_read_fail_the_sample() {
    let clock = Shared::default();
    let mut no_cpu = HostSampler::new(Fake::new(&[], 100, None));
    assert_eq!(no_cpu.sample(&clock, &[]), Err(SampleError::HostCpuLoad));
    let mut no_memory = HostSampler::new(Fake::new(&[ticks(1, 1, 1)], 100, None));
    assert_eq!(no_memory.sample(&clock, &[]), Err(SampleError::HostMemory));
    assert_eq!(
        SampleError::HostCpuLoad.to_string(),
        "the host CPU load cannot be read"
    );
    assert_eq!(
        SampleError::HostMemory.to_string(),
        "the host memory statistics cannot be read"
    );
}

/// Asserts a Mac's non-file memory: the anonymous pages that are not purgeable, the wired and
/// the compressor's, times the page size, and no fewer than zero anonymous pages when more
/// are purgeable than counted. Catches purgeable or file pages counted as held, and the
/// compressed memory left out.
#[test]
fn the_non_file_memory_is_anonymous_less_purgeable_wired_and_compressed() {
    let pages = VmPages {
        internal: 1_000,
        purgeable: 100,
        wired: 50,
        compressed: 25,
    };
    assert_eq!(pages.nonfile_bytes(16_384), 975 * 16_384);
    let mostly_purgeable = VmPages {
        purgeable: 2_000,
        ..pages
    };
    assert_eq!(mostly_purgeable.nonfile_bytes(4_096), 75 * 4_096);
    let huge = VmPages {
        wired: u64::MAX,
        ..pages
    };
    assert_eq!(huge.nonfile_bytes(2), u64::MAX);
}

/// Asserts that a collector over a host sampler publishes the host's load as the heartbeat
/// sends it (its busy time and non-file memory as the host's and the node's), with the
/// per-second busy rate and no pressure when the outbox asks for seconds; and that a host
/// whose counters cannot be read gives no load and one line that says why. Catches a Mac
/// whose heartbeat still carries no load.
#[test]
fn a_collector_over_the_host_publishes_its_load() {
    let clock = Shared::default();
    let outbox = Arc::new(Outbox::new().with_host_seconds(true));
    outbox.set_window(Duration::from_secs(5));
    let fake = Fake::new(
        &[ticks(100, 0, 0), ticks(250, 0, 0), ticks(400, 0, 0)],
        100,
        Some(2 << 30),
    );
    let sampler: Box<dyn NodeSampler> = Box::new(HostSampler::new(fake));
    let mut collector = Collector::with_sampler(sampler, Box::new(clock.clone()), outbox.clone());
    clock.0.set(1_000_000);
    assert!(collector.step());
    clock.0.set(2_000_000);
    assert!(collector.step());
    assert_eq!(collector.run(&[]), Vec::<String>::new());
    let load = outbox.take().expect("the heartbeat carries a load");
    assert_eq!(load.host_cpu_usec, Some(4_000_000));
    assert_eq!(load.cpu_usage_usec, 4_000_000);
    assert_eq!(load.host_nonfile_bytes, Some(2 << 30));
    assert_eq!(load.working_set_bytes, 2 << 30);
    assert_eq!(load.second_cpu_millis, vec![1_500]);
    assert_eq!(load.second_cpu_pressure_some_permille, Vec::<u16>::new());

    let outbox = Arc::new(Outbox::new());
    outbox.set_window(Duration::from_secs(5));
    let unread: Box<dyn NodeSampler> = Box::new(HostSampler::new(Fake::new(&[], 100, None)));
    let mut collector = Collector::with_sampler(unread, Box::new(clock), outbox.clone());
    assert_eq!(
        collector.run(&[]),
        ["usage: no load while the host CPU load cannot be read"]
    );
    assert!(outbox.take().is_none());
}

/// A lease that started at 0, taken over from no earlier process.
fn running(id: &str) -> RunningLease {
    RunningLease {
        lease_id: id.to_owned(),
        cgroup: None,
        started_usec: Some(0),
        adopted: false,
    }
}

/// Asserts that a whole-host reading covers each lease that was running at the last reading
/// published and still is: none at the first reading (no lease has a baseline), the lease that
/// ran through the second, not the one that started since, not one taken over from an earlier
/// process, and not one that has ended since. Catches a Mac load that covers no lease, which the server would charge
/// at each lease's booking on top of the host's measured use, so that a Mac's running jobs
/// would count twice.
#[test]
fn a_whole_host_reading_covers_the_leases_that_ran_through_it() {
    let clock = Shared::default();
    let outbox = Arc::new(Outbox::new());
    outbox.set_window(Duration::from_secs(5));
    let fake = Fake::new(
        &[
            ticks(100, 0, 0),
            ticks(200, 0, 0),
            ticks(300, 0, 0),
            ticks(400, 0, 0),
        ],
        100,
        Some(1 << 30),
    );
    let sampler: Box<dyn NodeSampler> = Box::new(HostSampler::new(fake));
    let mut collector = Collector::with_sampler(sampler, Box::new(clock.clone()), outbox.clone());
    let adopted = RunningLease {
        adopted: true,
        ..running("1-9")
    };
    clock.0.set(5_000_000);
    assert_eq!(
        collector.run(&[running("1-1"), adopted.clone()]),
        Vec::<String>::new()
    );
    assert_eq!(outbox.take().expect("a load").covered, Vec::<String>::new());
    outbox.delivered();
    clock.0.set(10_000_000);
    let now = [running("1-1"), running("1-2"), adopted.clone()];
    assert_eq!(collector.run(&now), Vec::<String>::new());
    assert_eq!(outbox.take().expect("a load").covered, ["1-1"]);
    outbox.delivered();
    clock.0.set(15_000_000);
    assert_eq!(collector.run(&now), Vec::<String>::new());
    assert_eq!(outbox.take().expect("a load").covered, ["1-1", "1-2"]);
    outbox.delivered();
    clock.0.set(20_000_000);
    assert_eq!(collector.run(&[running("1-1")]), Vec::<String>::new());
    assert_eq!(outbox.take().expect("a load").covered, ["1-1"]);
}

/// A node sampler of control groups that read none: a fixed node, and no lease.
struct NoLeases;

impl NodeSampler for NoLeases {
    fn sample(
        &mut self,
        clock: &dyn UsageClock,
        leases: &[LeaseCgroup],
    ) -> Result<Sample, SampleError> {
        Ok(Sample {
            mono_usec: clock.now_usec(),
            cpu_usage_usec: 0,
            host_cpu_usec: 0,
            host_cpu_pressure_some_usec: None,
            actions_oom_events: None,
            working_set_bytes: 0,
            host_nonfile_bytes: None,
            active_file_bytes: 0,
            cpu_limited: false,
            leases: Vec::new(),
            unreadable_leases: u32::try_from(leases.len()).unwrap_or(u32::MAX),
        })
    }

    fn host_counters(&mut self) -> Option<HostCpu> {
        None
    }
}

/// Asserts that a reading that is not the whole host's covers only the leases it read: a
/// lease that ran through two readings of a sampler that read none of its group is not
/// covered. Catches a cgroup host that names covered a lease whose use no reading held, which
/// the server would then charge at nothing.
#[test]
fn a_reading_of_groups_covers_only_the_leases_it_read() {
    let clock = Shared::default();
    let outbox = Arc::new(Outbox::new());
    outbox.set_window(Duration::from_secs(5));
    let mut collector =
        Collector::with_sampler(Box::new(NoLeases), Box::new(clock.clone()), outbox.clone());
    for at in [5_000_000, 10_000_000] {
        clock.0.set(at);
        assert_eq!(collector.run(&[running("1-1")]), Vec::<String>::new());
        assert_eq!(outbox.take().expect("a load").covered, Vec::<String>::new());
        outbox.delivered();
    }
}
