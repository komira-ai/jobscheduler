//! The rig the cases run on: its request, its room making, its fault hooks.

use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use js_elastic::conformance::Rig;
use js_elastic::conformance::live::{LiveCloud, LiveRig};
use js_elastic::{AcquireId, Acquired, HostName, HostRef, HostStatus, RefusalCode, SchedulerId};
use js_types::ProviderId;

use crate::mem::*;

fn rig<'a>(cloud: &'a MemCloud, state: &Arc<Mutex<State>>) -> LiveRig<'a> {
    let args = args_of(&[]);
    let provider = cloud
        .provider(&args.scheduler(), NonZeroUsize::MIN)
        .unwrap();
    let chosen = state.lock().unwrap().catalog.clone().unwrap().remove(0);
    LiveRig::new(&args, cloud, chosen, provider)
}

/// Asserts that the rig's request names the host as the shared cases do, stamps the run
/// label, and carries the chosen offer, and that its scheduler is the run's.
#[test]
fn the_rig_requests_the_chosen_offer_with_the_run_label() {
    let state = state(one_offer());
    let cloud = MemCloud::new(&state);
    let live = rig(&cloud, &state);
    let scheduler = SchedulerId::new("live-run-1").unwrap();
    assert_eq!(live.scheduler(), scheduler);
    let acquire = AcquireId::new("one").unwrap();
    let request = live.request(&acquire);
    assert_eq!(request.acquire, acquire);
    assert_eq!(request.pool, "live-conformance");
    assert_eq!(request.name, HostName::derive(&scheduler, &acquire));
    assert_eq!(request.offer.id, "cheap");
    assert_eq!(request.lifetime, None);
    assert_eq!(
        request.labels,
        [("validation-run".to_owned(), "run-1".to_owned())]
    );
    assert_eq!(live.provider().id(), ProviderId::known("mem"));
}

/// Asserts that `make_room` releases what is listed, that the machines still terminating
/// are not counted as launches and take room in the next provider, and that a rig with
/// room for one machine launches one.
#[test]
fn make_room_releases_and_launches_count_from_the_machines_after_it() {
    let state = state(one_offer());
    seed_machine(&state, "i-00000000000000009");
    let cloud = MemCloud::new(&state);
    let mut live = rig(&cloud, &state);
    assert_eq!(
        live.launches(),
        1,
        "before make_room the old machine counts"
    );
    live.make_room(1);
    assert_eq!(statuses(&state), [HostStatus::Terminating]);
    assert_eq!(live.launches(), 0);
    assert_eq!(cloud.rooms(), [1, 2]);
    let acquire = AcquireId::new("fresh").unwrap();
    let answer = live.provider().acquire(&live.request(&acquire));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    assert_eq!(live.launches(), 1);
}

/// Asserts that every hook that needs a fault the live cloud cannot make, and the count
/// of terminates, panics with the name of the hook and says so.
#[test]
fn every_fault_hook_panics_with_its_name() {
    let state = state(one_offer());
    let cloud = MemCloud::new(&state);
    let mut live = rig(&cloud, &state);
    let id = HostRef("i-1".to_owned());
    type Hook = Box<dyn Fn(&mut LiveRig<'_>)>;
    let hooks: Vec<(&str, Hook)> = vec![
        (
            "refuse_next",
            Box::new(|r| r.refuse_next(RefusalCode::Quota)),
        ),
        (
            "lose_next_answer_that_landed",
            Box::new(|r| r.lose_next_answer_that_landed()),
        ),
        (
            "lose_next_answer_that_did_not_land",
            Box::new(|r| r.lose_next_answer_that_did_not_land()),
        ),
        (
            "lose_next_launch_that_did_not_land",
            Box::new(|r| r.lose_next_launch_that_did_not_land()),
        ),
        (
            "fail_read_after_next_terminate",
            Box::new(|r| r.fail_read_after_next_terminate()),
        ),
        (
            "foreign_machine_exists",
            Box::new(|r| {
                r.foreign_machine_exists();
            }),
        ),
        ("fail_list", Box::new(|r| r.fail_list())),
        ("forget", Box::new(move |r| r.forget(&id))),
        ("throttle_reads", Box::new(|r| r.throttle_reads())),
        ("outwait_unresolved", Box::new(|r| r.outwait_unresolved())),
        (
            "terminates",
            Box::new(|r| {
                r.terminates();
            }),
        ),
    ];
    for (name, hook) in hooks {
        let payload = catch_unwind(AssertUnwindSafe(|| hook(&mut live))).unwrap_err();
        let text = payload.downcast_ref::<String>().cloned().unwrap();
        assert_eq!(
            text,
            format!("{name} is not available against a live cloud")
        );
    }
}
