//! The read-only view: what it keeps of an account and what it cannot do.

use std::sync::Arc;

use js_elastic::{AcquireId, HostName, HostStatus, SchedulerId};
use js_provider_lambdalabs::{
    ACQUIRE_TAG, ApiError, Architecture, Call, FakeLambda, LambdaRead, Reader, SCHEDULER_TAG, Tag,
};

use crate::rig::{SCHEDULER, offer, row, tag};

/// The tags and name of an instance launched by `scheduler` for `acquire`.
fn marks(scheduler: &str, acquire: &str) -> (String, Vec<Tag>) {
    let name = HostName::derive(
        &SchedulerId::new(scheduler).unwrap(),
        &AcquireId::new(acquire).unwrap(),
    );
    let tags = vec![tag(SCHEDULER_TAG, scheduler), tag(ACQUIRE_TAG, acquire)];
    (name.as_str().to_owned(), tags)
}

fn reader(fake: &Arc<FakeLambda>) -> Reader {
    let api: Arc<dyn LambdaRead> = fake.clone();
    Reader::new(api, SchedulerId::new(SCHEDULER).unwrap())
}

/// Asserts what the view keeps of a mixed account: the scheduler's whole instances in
/// every region, the half-marked ones as suspects, and neither another scheduler's nor a
/// terminated one. It catches a view that reads only one region, one that counts another
/// user's instance as ours (an alarm on a neighbour), and one that hides a half-marked
/// instance that still bills.
#[test]
fn the_view_keeps_ours_and_suspects_in_every_region_and_nothing_else() {
    let fake = Arc::new(FakeLambda::new());
    let (name, tags) = marks(SCHEDULER, "a");
    fake.add_instance(row("ours-a", Some(&name), HostStatus::Active, tags));
    let (name, tags) = marks(SCHEDULER, "b");
    let mut elsewhere = row("ours-b", Some(&name), HostStatus::Booting, tags);
    elsewhere.region = "region-z".to_owned();
    fake.add_instance(elsewhere);
    let (name, tags) = marks(SCHEDULER, "c");
    fake.add_instance(row("done", Some(&name), HostStatus::Terminated, tags));
    let (name, tags) = marks("other-sched", "d");
    fake.add_instance(row("theirs", Some(&name), HostStatus::Active, tags));
    fake.add_instance(row(
        "half",
        Some("renamed"),
        HostStatus::Active,
        vec![tag(SCHEDULER_TAG, SCHEDULER)],
    ));
    fake.add_instance(row("plain", None, HostStatus::Active, Vec::new()));

    let found = reader(&fake).tagged().unwrap();
    let ids = |held: &[js_elastic::Held]| {
        held.iter()
            .map(|h| h.id.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&found.ours), ["ours-a", "ours-b"]);
    assert_eq!(found.ours[1].zone.as_deref(), Some("region-z"));
    assert_eq!(ids(&found.suspects), ["half"]);
}

/// Asserts that a failed list is an error and never an empty view, and that the prices
/// come by type name. It catches a read failure turned into "nothing is running".
#[test]
fn a_failed_list_is_an_error_and_prices_come_by_type() {
    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(vec![offer(
        "gpu_1x_sample",
        75,
        1,
        Architecture::X86_64,
        &["region-a"],
    )]);
    let view = reader(&fake);
    fake.fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    assert!(view.tagged().is_err());
    assert_eq!(view.prices().unwrap()["gpu_1x_sample"], 75);
    fake.fail_next(Call::InstanceTypes, ApiError::Transport("down".to_owned()));
    assert!(view.prices().is_err());
    // Reading is all the view did.
    assert_eq!(fake.count(Call::Terminate) + fake.count(Call::Launch), 0);
}
