//! Disk as a booked dimension: a run books its `limits.disk_bytes` beside CPU and memory,
//! the booking is written to the container and read back after a restart.

use js_agent::lifecycle::label;

use super::{Harness, job, names};

const GIB: u64 = 1 << 30;

/// A job that books `gib` GiB of disk (the harness offers 10 GiB).
fn job_with_disk(run: &str, gib: u64) -> js_agent_wire::JobSpec {
    let mut spec = job(run);
    spec.limits.disk_bytes = Some(gib * GIB);
    spec
}

/// Asserts that disk is booked like CPU and memory: runs that fit together start and take
/// their bytes out of what is free, one that no longer fits beside them is refused
/// `no-capacity` and runs nothing, a stop that ends a run (cleanup passed) gives the bytes
/// back, and a run with no `disk_bytes` books none. It catches a disk term missing from
/// `booked`, `free` or the fit check, so that the sum of the runs' disk is never bounded.
#[test]
fn disk_is_booked_freed_and_refused_when_it_does_not_fit() {
    let mut h = Harness::new("disk-book");
    assert_eq!(h.life.free().disk_bytes, 10 * GIB);
    assert_eq!(names(&h.start(job_with_disk("a", 4))), ["started a"]);
    assert_eq!(names(&h.start(job_with_disk("b", 4))), ["started b"]);
    assert_eq!(h.life.free().disk_bytes, 2 * GIB);
    assert_eq!(
        names(&h.start(job_with_disk("c", 3))),
        ["refused c no-capacity"],
        "3 GiB does not fit in 2 GiB"
    );
    assert_eq!(h.fake.created().len(), 2, "the refused run created nothing");
    assert_eq!(names(&h.start(job_with_disk("d", 2))), ["started d"]);
    assert_eq!(h.life.free().disk_bytes, 0);
    assert_eq!(
        names(&h.start(job("e"))),
        ["started e"],
        "a run without disk_bytes books none"
    );
    h.fake.exit("a", 0);
    h.life.step();
    assert_eq!(
        h.life.free().disk_bytes,
        4 * GIB,
        "a's bytes are free again"
    );
}

/// Asserts that a job asking for more disk than the machine offers is refused
/// `limits-exceed-shape` (it can never fit) even when nothing is booked, that one asking
/// for exactly the offer runs, and that one more byte than the offer is refused. It
/// catches a disk term missing from the never-fits check, which would make a hopeless job
/// retriable.
#[test]
fn a_job_larger_than_the_offered_disk_is_refused_as_exceeding_the_shape() {
    let mut h = Harness::new("disk-exists");
    let mut over = job("over");
    over.limits.disk_bytes = Some(10 * GIB + 1);
    assert_eq!(names(&h.start(over)), ["refused over limits-exceed-shape"]);
    assert_eq!(names(&h.start(job_with_disk("all", 10))), ["started all"]);
    assert_eq!(h.life.free().disk_bytes, 0);
    assert_eq!(h.fake.created().len(), 1);
}

/// Asserts that the booking is written to the container as the `disk-bytes` label, and
/// that an agent that restarts reads it back: the adopted run still holds its bytes, so a
/// start that wants them is refused. It catches a booking lost at a restart, which would
/// let the disk be booked twice.
#[test]
fn the_disk_booking_survives_a_restart_through_the_label() {
    let mut h = Harness::new("disk-label");
    h.start(job_with_disk("a", 6));
    let labels = h.fake.created()[0].labels.clone();
    assert_eq!(
        labels.get(label::DISK_BYTES).map(String::as_str),
        Some((6 * GIB).to_string().as_str())
    );
    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["adopted a"]);
    assert_eq!(h.life.free().disk_bytes, 4 * GIB);
    assert_eq!(
        names(&h.start(job_with_disk("b", 5))),
        ["refused b no-capacity"]
    );
    assert_eq!(names(&h.start(job_with_disk("c", 4))), ["started c"]);
}

/// Asserts that a container with no `disk-bytes` label (an earlier agent made it) whose
/// labels are otherwise good is not adopted as a run with a guessed booking: when it cannot
/// be removed it holds the whole offer, disk included. It catches a missing label read as
/// zero bytes, which would leave the disk of a possibly running container free to book.
#[test]
fn a_container_without_the_disk_label_is_held_as_everything() {
    let mut h = Harness::new("disk-old-label");
    let old = [
        (label::RUN, "old"),
        (label::LEASE, "lease-old"),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::GPUS, ""),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
    ];
    h.fake.leak(&old, 0);
    h.fake.fail("remove", 5);
    h.restart().unwrap();
    let free = h.life.free();
    assert_eq!(
        (free.cpu_millis, free.memory_bytes, free.disk_bytes),
        (0, 0, 0),
        "the whole offer is held"
    );
}
