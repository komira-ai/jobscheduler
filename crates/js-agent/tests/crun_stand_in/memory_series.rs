//! A run's non-file memory second by second (`--memory-series`) over the stand-in `crun`: the
//! runtime reads `memory.current` and `memory.stat` of the lease's group at each poll of the
//! running job, on the agent's monotonic clock, and gives the series once the job has ended.
//! Each test moves its own monotonic clock by hand and writes the memory files itself; nothing
//! waits for time. A job that is stopped is started with [`crate::adopt::started`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent::crun::{CrunRuntime, CrunSettings, Hooks};
use js_agent::lifecycle::{Exit, RunHandle, Runtime, Status};
use js_agent_wire::MemorySeries;

use crate::node::{LONG, NoImages, Node};

/// One second, in microseconds.
const SECOND: u64 = 1_000_000;
/// One MiB, in bytes.
const MIB: u64 = 1024 * 1024;

/// A runtime on `node` that records series when `on`, reading `mono` for its monotonic clock.
fn open(node: &Node, on: bool, mono: fn() -> u64) -> CrunRuntime {
    let settings = CrunSettings {
        memory_series: on,
        ..node.settings("/root")
    };
    let hooks = Hooks {
        mono_us: mono,
        ..Node::hooks(LONG, LONG)
    };
    CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens")
}

/// Sets the clock `at` to `micros` microseconds.
fn at_us(clock: &AtomicU64, micros: u64) {
    clock.store(micros, Ordering::SeqCst);
}

/// Sets the clock `at` to `seconds` seconds.
fn at(clock: &AtomicU64, seconds: u64) {
    at_us(clock, seconds * SECOND);
}

/// Writes the `memory.current` of the lease `name` on `node`.
fn current(node: &Node, name: &str, bytes: u64) {
    std::fs::write(
        node.cgroup_of(name).join("memory.current"),
        format!("{bytes}\n"),
    )
    .expect("memory.current");
}

/// Writes the `memory.stat` of the lease `name` on `node` with `file` and `shmem` bytes.
fn stat(node: &Node, name: &str, file: u64, shmem: u64) {
    std::fs::write(
        node.cgroup_of(name).join("memory.stat"),
        format!("anon 1\nfile {file}\nshmem {shmem}\n"),
    )
    .expect("memory.stat");
}

/// Writes both memory files of the lease `name`: `used` bytes in all, `file` of them file
/// pages, `shmem` of those shared memory.
fn memory(node: &Node, name: &str, used: u64, file: u64, shmem: u64) {
    current(node, name, used);
    stat(node, name, file, shmem);
}

/// Stops the running `id` and waits for its end, then reads the end with a poll at 20 s.
fn ended_at_twenty(runtime: &mut CrunRuntime, clock: &AtomicU64, id: &RunHandle) {
    runtime.stop(id, LONG).expect("stopped");
    runtime.wait_ended(id);
    at(clock, 20);
    assert!(matches!(runtime.poll(id), Ok(Status::Exited(Exit { .. }))));
}

/// Proves a job polled while it runs gives, per second, the largest of its non-file memory
/// (`memory.current - (file - shmem)`) in MiB rounded up, over the run that ends where the
/// poll saw the end. Started at 10 s: 10 s reads 500 MiB with 100 file and 20 shared (420),
/// 10.5 s reads 300 MiB with no file pages (300), so the first point is the larger 420, not
/// the last (300) nor the mean; 11 s reads 200 MiB and one byte (201); 13 s reads 600 MiB
/// with more shared memory than file pages (600, the difference never negative), and 12 s,
/// which no poll read, holds the 201 before it. The end seen at 20 s makes the run 10 s long.
/// Catches a series of the mean or last sample, one that ignores the file pages or the
/// shared memory, a gap that is zero, a rounding down, and a run that ends where it was read.
#[test]
fn a_polled_job_gives_its_largest_non_file_memory_by_the_second() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("memseries-polled");
    let mut runtime = open(&node, true, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    let polls = [
        (10 * SECOND, 500 * MIB, 100 * MIB, 20 * MIB),
        (10 * SECOND + SECOND / 2, 300 * MIB, 0, 0),
        (11 * SECOND, 200 * MIB + 1, 0, 0),
        (13 * SECOND, 600 * MIB, 10 * MIB, 50 * MIB),
    ];
    for (micros, used, file, shmem) in polls {
        at_us(&MONO, micros);
        memory(&node, &id.0, used, file, shmem);
        assert_eq!(runtime.poll(&id), Ok(Status::Running));
    }
    ended_at_twenty(&mut runtime, &MONO, &id);
    at(&MONO, 50);
    assert_eq!(
        runtime.memory_series(&id),
        Some(MemorySeries {
            granularity_secs: 1,
            points: vec![420, 201, 201, 600],
            run_usec: 10 * SECOND,
            partial: false,
        })
    );
    runtime.remove(&id).expect("removed");
}

/// Proves a run of an agent without the flag records no memory series, though its polls and
/// CPU go on as always. Catches a memory series recorded by default.
#[test]
fn a_job_of_an_agent_without_the_flag_gives_no_memory_series() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("memseries-off");
    let mut runtime = open(&node, false, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    memory(&node, &id.0, 500 * MIB, 0, 0);
    assert_eq!(runtime.poll(&id), Ok(Status::Running));
    ended_at_twenty(&mut runtime, &MONO, &id);
    assert_eq!(runtime.memory_series(&id), None);
    runtime.remove(&id).expect("removed");
}

/// Proves a poll whose memory files cannot be read adds nothing: at 10 s 100 MiB is read;
/// at 11 s `memory.current` says 900 MiB but `memory.stat` is gone; at 12 s 400 MiB is read;
/// at 13 s `memory.stat` says 900 MiB of nothing but `memory.current` is gone; at 14 s 200
/// MiB is read; at 15 s `memory.stat` lacks `shmem`, with 900 MiB in `memory.current`. The
/// points are 100, 100, 400, 400, 200 and the unread poll at 15 s adds none. Catches an
/// unreadable poll that counts the half it read, one that reads as 0, and one that fails the
/// poll.
#[test]
fn a_poll_whose_memory_files_cannot_be_read_adds_nothing() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("memseries-blind-poll");
    let mut runtime = open(&node, true, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    let dir = node.cgroup_of(&id.0);
    let poll = |runtime: &mut CrunRuntime, seconds: u64| {
        at(&MONO, seconds);
        assert_eq!(runtime.poll(&id), Ok(Status::Running));
    };
    memory(&node, &id.0, 100 * MIB, 0, 0);
    poll(&mut runtime, 10);
    current(&node, &id.0, 900 * MIB);
    std::fs::remove_file(dir.join("memory.stat")).expect("removed memory.stat");
    poll(&mut runtime, 11);
    memory(&node, &id.0, 400 * MIB, 0, 0);
    poll(&mut runtime, 12);
    stat(&node, &id.0, 900 * MIB, 0);
    std::fs::remove_file(dir.join("memory.current")).expect("removed memory.current");
    poll(&mut runtime, 13);
    memory(&node, &id.0, 200 * MIB, 0, 0);
    poll(&mut runtime, 14);
    current(&node, &id.0, 900 * MIB);
    std::fs::write(dir.join("memory.stat"), "file 0\n").expect("memory.stat");
    poll(&mut runtime, 15);
    ended_at_twenty(&mut runtime, &MONO, &id);
    assert_eq!(
        runtime.memory_series(&id),
        Some(MemorySeries {
            granularity_secs: 1,
            points: vec![100, 100, 400, 400, 200],
            run_usec: 10 * SECOND,
            partial: false,
        })
    );
    runtime.remove(&id).expect("removed");
}

/// Proves a run no poll read gives no series, and so does a container the runtime does not
/// hold. Catches a series made of no sample and a panic on an id it never saw.
#[test]
fn a_job_no_poll_read_and_an_unknown_container_give_no_memory_series() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("memseries-unread");
    let mut runtime = open(&node, true, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    ended_at_twenty(&mut runtime, &MONO, &id);
    assert_eq!(runtime.memory_series(&id), None);
    let unknown = RunHandle("never-held".to_owned());
    assert_eq!(runtime.memory_series(&unknown), None);
    runtime.remove(&id).expect("removed");
}

/// Proves a run the agent adopted after a restart gives a partial series that starts at its
/// first poll in the new process: 300 MiB at 100 s, 500 MiB at 102 s, the end seen at 103 s.
/// The points are 300, 300 (held) and 500, over 3 s, marked partial. Catches an adopted run's
/// series that starts at the restart, one that is not marked partial, and one that ends
/// where it was read.
#[test]
fn an_adopted_job_gives_a_partial_memory_series_from_its_first_poll() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("memseries-adopted");
    let mut first = open(&node, true, mono);
    let id = crate::adopt::held(&node, &mut first, "r1-hold");
    drop(first);
    let mut second = open(&node, true, mono);
    assert_eq!(second.swept().adopted, ["r1-hold"]);
    for (seconds, used) in [(100, 300 * MIB), (102, 500 * MIB)] {
        at(&MONO, seconds);
        memory(&node, &id.0, used, 0, 0);
        assert_eq!(second.poll(&id), Ok(Status::Running));
    }
    second.stop(&id, LONG).expect("stopped");
    second.wait_ended(&id);
    at(&MONO, 103);
    assert!(matches!(second.poll(&id), Ok(Status::Exited(_))));
    at(&MONO, 200);
    assert_eq!(
        second.memory_series(&id),
        Some(MemorySeries {
            granularity_secs: 1,
            points: vec![300, 300, 500],
            run_usec: 3 * SECOND,
            partial: true,
        })
    );
    second.remove(&id).expect("removed");
}
