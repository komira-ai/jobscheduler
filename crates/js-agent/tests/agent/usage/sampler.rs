//! The sampler over fake files, and the clocks.

use js_agent::usage::{
    LeaseCgroup, LeaseSample, MonotonicClock, Sample, SampleError, Sampler, UsageClock,
};

use super::StepClock;
use crate::support::FakeFiles;

const ACTIONS: &str = "/sys/fs/cgroup/actions";

fn cpu_stat(usage: u64) -> String {
    format!("usage_usec {usage}\nuser_usec 1\nsystem_usec 2\n")
}

fn memory_stat(inactive: u64, active: u64) -> String {
    format!(
        "anon 1\nactive_anon 11\ninactive_anon 12\ninactive_file {inactive}\nactive_file {active}\n"
    )
}

/// The node's files: usage, current, inactive, active and `cpu.max`.
fn node(usage: u64, current: u64, inactive: u64, active: u64, cpu_max: &str) -> FakeFiles {
    FakeFiles::default()
        .with(&format!("{ACTIONS}/cpu.stat"), &cpu_stat(usage))
        .with(
            &format!("{ACTIONS}/memory.current"),
            &format!("{current}\n"),
        )
        .with(
            &format!("{ACTIONS}/memory.stat"),
            &memory_stat(inactive, active),
        )
        .with(&format!("{ACTIONS}/cpu.max"), cpu_max)
}

fn with_lease(files: FakeFiles, dir: &str, usage: u64, current: u64, inactive: u64) -> FakeFiles {
    files
        .with(&format!("{dir}/cpu.stat"), &cpu_stat(usage))
        .with(&format!("{dir}/memory.current"), &format!("{current}\n"))
        .with(&format!("{dir}/memory.stat"), &memory_stat(inactive, 0))
}

fn cgroup(id: &str, started: u64) -> LeaseCgroup {
    LeaseCgroup {
        lease_id: id.to_owned(),
        dir: format!("{ACTIONS}/{id}"),
        started_usec: started,
    }
}

fn sample(files: FakeFiles, leases: &[LeaseCgroup]) -> Result<Sample, SampleError> {
    Sampler::new(files, ACTIONS).sample(&StepClock::at(777), leases)
}

/// Asserts every field of a sample with distinct values everywhere, and that the clock was
/// read once however many leases there are. It catches swapped inactive and active file
/// values, a working set read from the wrong directory, a clock read per lease and a lost
/// `started_usec`.
#[test]
fn a_sample_reads_every_field() {
    let files = node(1000, 5000, 700, 300, "max 100000\n");
    let files = with_lease(files, &format!("{ACTIONS}/a"), 40, 900, 50);
    let files = with_lease(files, &format!("{ACTIONS}/b"), 41, 1000, 10);
    let clock = StepClock::at(777);
    let got = Sampler::new(files, ACTIONS)
        .sample(&clock, &[cgroup("a", 111), cgroup("b", 222)])
        .unwrap();
    let lease = |id: &str, started, cpu, ws| LeaseSample {
        lease_id: id.to_owned(),
        started_usec: started,
        cpu_usage_usec: cpu,
        working_set_bytes: ws,
    };
    assert_eq!(
        got,
        Sample {
            mono_usec: 777,
            cpu_usage_usec: 1000,
            working_set_bytes: 4300,
            active_file_bytes: 300,
            cpu_limited: false,
            leases: vec![lease("a", 111, 40, 850), lease("b", 222, 41, 990)],
            unreadable_leases: 0,
        }
    );
    assert_eq!(clock.reads(), 1);
}

/// Asserts that a file cache larger than `memory.current` gives a working set of 0 for the
/// node and for a lease. It catches a raw subtraction, which would panic or wrap.
#[test]
fn inactive_file_above_current_gives_a_zero_working_set() {
    let files = with_lease(
        node(1, 10, 50, 0, "max 1\n"),
        &format!("{ACTIONS}/a"),
        2,
        100,
        500,
    );
    let got = sample(files, &[cgroup("a", 0)]).unwrap();
    assert_eq!(got.working_set_bytes, 0);
    assert_eq!(got.leases[0].working_set_bytes, 0);
}

/// Asserts that the node's CPU is the parent's counter in three samples while a lease's
/// directory disappears and the parent keeps rising. It catches a node counter that is the
/// sum of the leases, which falls when a lease is removed.
#[test]
fn node_counter_is_the_parents_not_the_leases_sum() {
    let a = format!("{ACTIONS}/a");
    let b = format!("{ACTIONS}/b");
    let leases = [cgroup("a", 0), cgroup("b", 0)];
    let both = with_lease(
        with_lease(node(1000, 1, 0, 0, "max 1\n"), &a, 40, 1, 0),
        &b,
        41,
        1,
        0,
    );
    let first = sample(both, &leases).unwrap();
    assert_eq!((first.cpu_usage_usec, first.leases.len()), (1000, 2));

    let gone = node(1100, 1, 0, 0, "max 1\n");
    let second = sample(with_lease(gone, &a, 50, 1, 0), &leases).unwrap();
    assert_eq!((second.cpu_usage_usec, second.leases.len()), (1100, 1));

    let third = sample(node(1200, 1, 0, 0, "max 1\n"), &leases).unwrap();
    assert_eq!((third.cpu_usage_usec, third.leases.len()), (1200, 0));
    assert_eq!(third.unreadable_leases, 2);
}

/// Asserts that a lease with no files and a lease with unparsable ones are left out and
/// counted while the good lease and the node survive, and that each of the three files of
/// a lease, missing or garbled, takes the lease out alone. It catches a sample that fails
/// as a whole and a lease that is dropped without being counted.
#[test]
fn an_unreadable_lease_is_skipped_and_counted() {
    let good = format!("{ACTIONS}/good");
    let bad = format!("{ACTIONS}/bad");
    let base = with_lease(node(1, 1, 0, 0, "max 1\n"), &good, 5, 6, 1);
    let garbage = base
        .clone()
        .with(&format!("{bad}/cpu.stat"), "usage_usec x\n");
    let got = sample(
        garbage,
        &[cgroup("good", 9), cgroup("bad", 9), cgroup("none", 9)],
    )
    .unwrap();
    assert_eq!(got.unreadable_leases, 2);
    assert_eq!(got.leases.len(), 1);
    assert_eq!(got.leases[0].lease_id, "good");
    assert_eq!(got.cpu_usage_usec, 1);

    for file in ["cpu.stat", "memory.current", "memory.stat"] {
        let path = format!("{good}/{file}");
        for broken in [base.clone().without(&path), base.clone().with(&path, "x\n")] {
            let got = sample(broken, &[cgroup("good", 9)]).unwrap();
            assert_eq!((got.leases.len(), got.unreadable_leases), (0, 1), "{file}");
        }
    }
}

/// Asserts that each of the node's three files, removed and then garbled, is an error of
/// its own variant, that `memory.stat` without either cache key is the same error, that the
/// text is fixed, and that no zeros are invented. It catches a swallowed failure (a bogus
/// baseline of zero) and a wrong variant.
#[test]
fn an_unreadable_node_file_is_an_error() {
    let whole = node(1, 2, 0, 0, "max 1\n");
    let cases = [
        ("cpu.stat", SampleError::NodeCpuStat),
        ("memory.current", SampleError::NodeMemoryCurrent),
        ("memory.stat", SampleError::NodeMemoryStat),
    ];
    for (file, error) in cases {
        let path = format!("{ACTIONS}/{file}");
        assert_eq!(sample(whole.clone().without(&path), &[]), Err(error));
        assert_eq!(
            sample(whole.clone().with(&path, "garbage\n"), &[]),
            Err(error)
        );
    }
    let stat = format!("{ACTIONS}/memory.stat");
    let only_inactive = whole.clone().with(&stat, "inactive_file 1\n");
    assert_eq!(sample(only_inactive, &[]), Err(SampleError::NodeMemoryStat));
    let only_active = whole.with(&stat, "active_file 1\n");
    assert_eq!(sample(only_active, &[]), Err(SampleError::NodeMemoryStat));

    assert_eq!(
        SampleError::NodeCpuStat.to_string(),
        "the node cpu.stat cannot be read"
    );
    assert_eq!(
        SampleError::NodeMemoryCurrent.to_string(),
        "the node memory.current cannot be read"
    );
    assert_eq!(
        SampleError::NodeMemoryStat.to_string(),
        "the node memory.stat cannot be read"
    );
}

/// Asserts the clock is read once, right after `cpu.stat` and before the other files: not
/// at all when `cpu.stat` fails, and once when a later file fails. It catches a reading
/// taken before the counter it is paired with.
#[test]
fn the_clock_is_read_after_the_node_cpu_stat() {
    let whole = node(1, 2, 0, 0, "max 1\n");
    let clock = StepClock::at(5);
    let sampler = Sampler::new(
        whole.clone().without(&format!("{ACTIONS}/cpu.stat")),
        ACTIONS,
    );
    assert!(sampler.sample(&clock, &[]).is_err());
    assert_eq!(clock.reads(), 0);
    let sampler = Sampler::new(whole.without(&format!("{ACTIONS}/memory.current")), ACTIONS);
    assert!(sampler.sample(&clock, &[]).is_err());
    assert_eq!(clock.reads(), 1);
}

/// Asserts `cpu.max`: `max` is not limited and a quota is limited, and a file that is
/// absent or unparsable fails the sample with its own error instead of a guessed value.
/// It catches a made-up `actions_cpu_limited` on the wire for a node whose limit is unknown.
#[test]
fn cpu_max_that_cannot_be_read_fails_the_sample() {
    let limited = |files: FakeFiles| sample(files, &[]).unwrap().cpu_limited;
    let whole = node(1, 2, 0, 0, "max 100000\n");
    assert!(!limited(whole.clone()));
    assert!(limited(
        whole
            .clone()
            .with(&format!("{ACTIONS}/cpu.max"), "50000 100000\n")
    ));
    let missing = whole.clone().without(&format!("{ACTIONS}/cpu.max"));
    assert_eq!(sample(missing, &[]), Err(SampleError::NodeCpuMax));
    let junk = whole.with(&format!("{ACTIONS}/cpu.max"), "x\n");
    assert_eq!(sample(junk, &[]), Err(SampleError::NodeCpuMax));
}

/// Asserts a thousand readings of the production clock never go back.
/// Both constructors give a clock. It
/// cannot catch a wall clock; the next test guards that.
#[test]
fn monotonic_clock_does_not_go_back() {
    for clock in [MonotonicClock::new(), MonotonicClock::default()] {
        let mut last = clock.now_usec();
        for _ in 0..1000 {
            let now = clock.now_usec();
            assert!(now >= last);
            last = now;
        }
    }
}

/// Asserts that no source of the module names a wall clock. It is a crude guard that turns
/// red the day one slips in, which no behaviour test would notice.
#[test]
fn usage_sources_use_no_wall_clock() {
    let sources = [
        include_str!("../../../src/usage/binner.rs"),
        include_str!("../../../src/usage/cgroup.rs"),
        include_str!("../../../src/usage/clock.rs"),
        include_str!("../../../src/usage/mod.rs"),
        include_str!("../../../src/usage/sampler.rs"),
        include_str!("../../../../js-agent-usage/src/binner.rs"),
        include_str!("../../../../js-agent-usage/src/lib.rs"),
        include_str!("../../../../js-agent-usage/src/sample.rs"),
    ];
    let banned = [["System", "Time"].concat(), ["UNIX", "_EPOCH"].concat()];
    for source in sources {
        for word in &banned {
            assert!(!source.contains(word.as_str()), "{word}");
        }
    }
}

/// Asserts that a lease's usage is read only from a group below the actions group, and that
/// the direct runner's `<lease>/ctr` is one: the lease at `<lease>/ctr` and the one at an
/// engine's scope are sampled; a lease whose directory is in another tree, only looks like
/// the actions group by name, is the actions group itself or has a `..` is counted as
/// unreadable, though its files are there and would parse. It catches a sampler that reads
/// whatever directory it is given: a lease then reports another tree's (or the whole
/// node's) usage as its own.
#[test]
fn only_groups_below_the_actions_group_are_sampled() {
    let lease = |id: &str, dir: String| LeaseCgroup {
        lease_id: id.to_owned(),
        dir,
        started_usec: 5,
    };
    let sampled = [
        format!("{ACTIONS}/lease-1/ctr"),
        format!("{ACTIONS}/libpod-ab.scope/container"),
    ];
    let refused = [
        "/sys/fs/cgroup/actions-old/lease-2/ctr".to_owned(),
        "/sys/fs/cgroup/other/lease-3/ctr".to_owned(),
        ACTIONS.to_owned(),
        format!("{ACTIONS}/../other/ctr"),
    ];
    let mut files = node(1000, 5000, 700, 300, "max 100000\n");
    let mut leases = Vec::new();
    for (at, dir) in sampled.iter().chain(&refused).enumerate() {
        files = with_lease(files, dir, 40, 900, 50);
        leases.push(lease(&format!("l{at}"), dir.clone()));
    }
    let got = sample(files, &leases).unwrap();
    let ids: Vec<&str> = got.leases.iter().map(|l| l.lease_id.as_str()).collect();
    assert_eq!(ids, ["l0", "l1"]);
    assert_eq!(got.unreadable_leases, 4);
}
