//! The scaler's demand and busy hosts read from the job tables through the shell, over a
//! fake world: a queued job launches one machine and is held to it, a host with a live job
//! allocation is never idled, a whole-host job launches one machine of the pool's shape, a
//! lease the registry books counts for neither, and a tick that cannot read the tables does
//! nothing. Time is an injected clock; nothing sleeps.

use std::time::Duration;

use js_elastic::Offer;
use js_jobs::{Booked, Needs};
use js_scaler::{Phase, Records};
use js_types::{LeaseId, Model, Resources, accelerator_of};

use crate::fakes::{
    Rig, allocation, allocation_of, job, job_with, live, offer, record, records, rig, spec_of, view,
};

fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// The acquires the fake provider was asked for, by machine name, in order.
fn acquired(rig: &Rig) -> Vec<String> {
    let log = rig.log();
    let names = log.iter().filter_map(|each| each.strip_prefix("acquire:"));
    names
        .map(|rest| rest.split(':').next().unwrap_or_default().to_owned())
        .collect()
}

/// Whether the shell asked the registry to drain `host`.
fn drained(rig: &Rig, host: &str) -> bool {
    rig.log().contains(&format!("Drain:{host}"))
}

/// Asserts that, with no host and one queued job, one tick issues exactly one acquire (no
/// registry lease exists to stand for the job), and that the next ticks, with the launch
/// still on its way, issue no other. It catches a queued job the scaler does not see, which
/// launches nothing, and a job counted again at every tick, which launches a machine each.
#[tokio::test]
async fn queued_job_drives_one_acquire() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("j1")];
    rig.scaler.tick().await;
    assert_eq!(acquired(&rig).len(), 1, "{:?}", rig.log());
    for _ in 0..3 {
        rig.advance(Duration::from_secs(5));
        rig.scaler.tick().await;
    }
    assert_eq!(acquired(&rig).len(), 1, "{:?}", rig.log());
    let state = rig.scaler.state();
    assert_eq!(state.records.instances.len(), 1);
    assert_eq!(state.records.instances[0].phase, Phase::Booting);
}

/// Asserts that while the machine launched for one queued job is on its way, a second
/// queued job that the same machine would fit gets a machine of its own at the next tick:
/// the first machine is held for the first job. It catches a machine on its way counted for
/// a job it was not launched for, which leaves that job without a machine until the first
/// one boots.
#[tokio::test]
async fn a_machine_on_its_way_is_held_for_the_job_it_was_launched_for() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("j1")];
    rig.scaler.tick().await;
    rig.world().queued = vec![job("j1"), job("j2")];
    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    assert_eq!(acquired(&rig).len(), 2, "{:?}", rig.log());
    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    assert_eq!(acquired(&rig).len(), 2, "{:?}", rig.log());
}

/// Asserts that a registered host with one live job allocation and no registry lease is
/// never drained or released, with the clock moved far past the pool's idle time, and that
/// the same host with no allocation is drained once it has been quiet for that time. It
/// catches a host that runs only job allocations read as idle, which the scaler would take
/// down under the running job.
#[tokio::test]
async fn running_allocation_host_never_idled() {
    let rig = rig(records(vec![record("js-busy", Phase::Registered)]), live());
    rig.world().hosts = vec![view("js-busy")];
    rig.world().live = vec![allocation("j-running", "js-busy")];
    for _ in 0..12 {
        rig.scaler.tick().await;
        rig.advance(mins(60));
    }
    assert!(!drained(&rig, "js-busy"), "{:?}", rig.log());
    assert!(
        !rig.log().iter().any(|each| each.starts_with("release:")),
        "{:?}",
        rig.log()
    );

    rig.world().live.clear();
    rig.scaler.tick().await;
    rig.advance(mins(10));
    rig.scaler.tick().await;
    assert!(drained(&rig, "js-busy"), "{:?}", rig.log());
}

/// An offer like [`offer`] named `id`, with `cpu_millis`, `gib` GiB, `gpus` a10s and `price`.
fn sized(id: &str, cpu_millis: u64, gib: u64, gpus: u32, price: u64) -> Offer {
    Offer {
        id: id.to_owned(),
        cpu_millis,
        memory_bytes: gib << 30,
        accelerator: Some(accelerator_of(
            &Model::parse("nvidia-a10").unwrap(),
            gpus,
            None,
        )),
        price_cents_per_hour: Some(price),
        ..offer()
    }
}

/// Asserts that a queued whole-host job, in a pool whose floor is eight cores, 32 GiB and one
/// a10, launches exactly one machine of the offer that brings that shape (`x`), not the
/// cheaper smaller one a share job takes, nor the larger one, nor a second machine at the
/// next ticks; and that once the job is allocated the whole host on that machine's host, the
/// host reads busy and is not drained. It catches a whole-host job read as a job of no size,
/// which launches the smallest machine, and a whole-host allocation not counted as work.
#[tokio::test]
async fn whole_host_job_launch_sized_to_shape() {
    let offers = vec![
        sized("small", 2000, 8, 1, 20),
        sized("x", 8000, 64, 1, 60),
        sized("big", 32000, 256, 4, 200),
    ];
    let floor = Resources::new(8000, 32 << 30, 1, Some("nvidia-a10".to_owned())).unwrap();

    let share = rig(Records::default(), live());
    share.world().offers = offers.clone();
    share.world().queued = vec![job("j-share")];
    share.scaler.tick().await;
    let launched: Vec<String> = share
        .world()
        .machines
        .iter()
        .map(|m| m.offer.clone())
        .collect();
    assert_eq!(launched, ["small"]);

    let rig = rig(Records::default(), live());
    rig.world().offers = offers;
    rig.world().whole_host.resources = floor;
    rig.world().queued = vec![job_with("j-whole", spec_of(Needs::WholeHost))];
    for _ in 0..3 {
        rig.scaler.tick().await;
        rig.advance(Duration::from_secs(5));
    }
    let launched: Vec<String> = rig
        .world()
        .machines
        .iter()
        .map(|m| m.offer.clone())
        .collect();
    assert_eq!(launched, ["x"], "{:?}", rig.log());
    let name = acquired(&rig).remove(0);

    rig.world().hosts = vec![view(&name)];
    rig.scaler.tick().await;
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );
    rig.world().queued.clear();
    rig.world().live = vec![allocation_of("j-whole", &name, Booked::WholeHost)];
    for _ in 0..6 {
        rig.advance(mins(60));
        rig.scaler.tick().await;
    }
    assert!(!drained(&rig, &name), "{:?}", rig.log());
    assert_eq!(acquired(&rig).len(), 1, "{:?}", rig.log());
}

/// Asserts that a lease the registry books, with no job row and no allocation, launches
/// nothing and keeps no host: the leased host is drained once it has been quiet for the
/// pool's idle time, while the release itself still waits for the lease (the guard just
/// before a release asks the registry too). It catches the registry's leases still read as
/// demand or as work.
#[tokio::test]
async fn registry_lease_no_longer_counts() {
    let rig = rig(
        records(vec![record("js-leased", Phase::Registered)]),
        live(),
    );
    let mut leased = view("js-leased");
    leased.lease = Some(LeaseId::new(1, 1));
    rig.world().hosts = vec![leased];
    rig.scaler.tick().await;
    rig.advance(mins(10));
    rig.scaler.tick().await;
    assert!(drained(&rig, "js-leased"), "{:?}", rig.log());
    assert!(acquired(&rig).is_empty(), "{:?}", rig.log());
    for _ in 0..4 {
        rig.advance(Duration::from_secs(5));
        rig.scaler.tick().await;
    }
    assert!(
        !rig.log().iter().any(|each| each.starts_with("release:")),
        "{:?}",
        rig.log()
    );
}

/// Asserts that a job read queued and also found among the live allocations (it was placed
/// between the two reads) is no demand: nothing is launched, and its host is busy. It
/// catches a machine launched for a job that already has a host.
#[tokio::test]
async fn a_job_placed_between_the_reads_waits_for_nothing() {
    let rig = rig(
        records(vec![record("js-placed", Phase::Registered)]),
        live(),
    );
    rig.world().hosts = vec![view("js-placed")];
    rig.world().queued = vec![job("j1")];
    rig.world().live = vec![allocation("j1", "js-placed")];
    for _ in 0..3 {
        rig.scaler.tick().await;
        rig.advance(mins(30));
    }
    assert!(acquired(&rig).is_empty(), "{:?}", rig.log());
    assert!(!drained(&rig, "js-placed"), "{:?}", rig.log());
}

/// Asserts that a tick whose read of the job tables fails makes no call and writes nothing,
/// with a job waiting and an idle host past its idle time, and leaves the heartbeat where
/// it was; and that the next tick, with the tables readable again, launches for the job. It
/// catches a failed read taken as no demand and no host in use, which drains hosts under
/// running jobs.
#[tokio::test]
async fn a_tick_that_cannot_read_the_job_tables_does_nothing() {
    let rig = rig(records(vec![record("js-idle", Phase::Registered)]), live());
    let mut busy = view("js-idle");
    busy.claim = None;
    rig.world().hosts = vec![busy];
    rig.world().queued = vec![job("j1")];
    rig.world().jobs_fail = true;
    for _ in 0..3 {
        rig.scaler.tick().await;
        rig.advance(mins(30));
    }
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.scaler.last_tick(), None);

    rig.world().jobs_fail = false;
    rig.scaler.tick().await;
    assert_eq!(acquired(&rig).len(), 1, "{:?}", rig.log());
    assert!(rig.scaler.last_tick().is_some());
}

/// Asserts the life of a reservation through a machine's boot: the host of the machine
/// launched for a queued job, once registered, is not drained while the job waits for the
/// engine, however long, and a second queued job gets a machine of its own instead of that
/// host; once the job is placed there the host is busy; and once the job is done and gone
/// the host is drained at the pool's idle time. It catches a new host idled before the engine
/// places the job it was launched for, and a reservation that outlives the job.
#[tokio::test]
async fn the_host_launched_for_a_job_waits_for_it_and_is_let_go_after() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("j1")];
    rig.scaler.tick().await;
    let name = acquired(&rig).remove(0);
    rig.world().hosts = vec![view(&name)];
    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );
    for _ in 0..4 {
        rig.advance(mins(30));
        rig.scaler.tick().await;
    }
    assert!(!drained(&rig, &name), "{:?}", rig.log());
    assert_eq!(acquired(&rig).len(), 1, "{:?}", rig.log());

    rig.world().queued = vec![job("j1"), job("j2")];
    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    assert_eq!(acquired(&rig).len(), 2, "{:?}", rig.log());

    rig.world().queued = vec![job("j2")];
    rig.world().live = vec![allocation("j1", &name)];
    rig.advance(mins(30));
    rig.scaler.tick().await;
    assert!(!drained(&rig, &name), "{:?}", rig.log());

    rig.world().live.clear();
    rig.scaler.tick().await;
    rig.advance(mins(10));
    rig.scaler.tick().await;
    assert!(drained(&rig, &name), "{:?}", rig.log());
}
