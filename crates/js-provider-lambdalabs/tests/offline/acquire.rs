//! Acquire: the guards in front of a launch, and what each answer of the API becomes.

use std::time::Duration;

use js_provider_lambdalabs::{
    Acquired, ApiError, Call, Elastic, ErrorCode, Reason, Refusal, RefusalCode, SCHEDULER_TAG,
    Status,
};

use crate::rig::{INTERVAL, SCHEDULER, ask, candidate, config, id, live, rig, row, tag};

fn refused(code: RefusalCode, reason: Reason) -> Acquired {
    Acquired::Refused(Refusal { code, reason })
}

fn api(status: u16, code: ErrorCode) -> ApiError {
    ApiError::Api {
        status,
        code,
        request_id: None,
    }
}

fn held_id(answer: &Acquired) -> String {
    match answer {
        Acquired::Accepted { held, .. } => held.id.to_string(),
        other => panic!("not accepted: {other:?}"),
    }
}

#[test]
fn a_new_provider_is_disabled_and_asks_the_api_nothing() {
    let rig = rig(config(5));
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        refused(RefusalCode::Denied, Reason::Disabled)
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn enabled_but_in_dry_run_it_builds_the_launch_and_sends_nothing() {
    let rig = rig(config(5).enable());
    // Twice: the pool asks again every tick, and the answer stays the same.
    for _ in 0..2 {
        assert_eq!(
            rig.provider.acquire(&ask("a")),
            refused(RefusalCode::Denied, Reason::DryRun)
        );
    }
    assert_eq!(rig.fake.launched(), 0);
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn live_but_not_enabled_sends_nothing() {
    let rig = rig(config(5).live());
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        refused(RefusalCode::Denied, Reason::Disabled)
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn the_same_request_that_dry_run_stops_launches_once_when_live() {
    let rig = rig(live(5));
    let answer = rig.provider.acquire(&ask("a"));
    assert_eq!(rig.fake.launched(), 1);
    assert_eq!(rig.fake.count(Call::Launch), 1);
    let Acquired::Accepted { held, extras } = answer else {
        panic!("{answer:?}");
    };
    assert!(extras.is_empty());
    assert_eq!(held.acquire, "a");
    assert_eq!(held.status, Status::Booting);
    assert_eq!(held.instance_type, "gpu_1x_sample");
    assert_eq!(held.region, "region-a");
    let stored = &rig.fake.instances()[0];
    assert_eq!(stored.id, held.id);
    assert_eq!(stored.name, held.name);
}

#[test]
fn a_type_outside_the_option_table_is_refused_without_a_call() {
    let rig = rig(live(5));
    let mut request = ask("a");
    request.candidate.instance_type = "gpu_2x_other".to_owned();
    assert_eq!(
        rig.provider.acquire(&request),
        refused(RefusalCode::Shape, Reason::NotAllowed)
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn acquiring_twice_with_one_id_launches_one_instance() {
    let rig = rig(live(5));
    let first = rig.provider.acquire(&ask("a"));
    let second = rig.provider.acquire(&ask("a"));
    assert_eq!(held_id(&first), held_id(&second));
    assert_eq!(rig.fake.launched(), 1);
    assert_eq!(rig.fake.count(Call::Launch), 1);
}

#[test]
fn a_launch_whose_answer_was_lost_is_unknown_and_the_retry_finds_the_instance() {
    let rig = rig(live(5));
    rig.fake
        .lose_response(Call::Launch, ApiError::Transport("reset".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    assert_eq!(rig.fake.launched(), 1, "the lost launch landed");
    let again = rig.provider.acquire(&ask("a"));
    assert_eq!(held_id(&again), "fake0001");
    assert_eq!(rig.fake.launched(), 1, "no second instance");
    assert_eq!(rig.fake.count(Call::Launch), 1, "no second request");
}

#[test]
fn a_launch_with_no_answer_is_retried_only_once_the_hold_has_ended() {
    let rig = rig(live(5));
    rig.fake
        .fail_next(Call::Launch, ApiError::Transport("refused".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    assert_eq!(rig.fake.launched(), 0);
    // It may have landed: the retry waits for the hold, not just the API's interval.
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        Acquired::Deferred {
            wait: Duration::from_secs(600)
        }
    );
    rig.clock.advance(INTERVAL);
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        Acquired::Deferred {
            wait: Duration::from_secs(600) - INTERVAL
        }
    );
    assert_eq!(rig.fake.count(Call::Launch), 1);
    rig.clock.advance(Duration::from_secs(600) - INTERVAL);
    assert_eq!(held_id(&rig.provider.acquire(&ask("a"))), "fake0001");
    assert_eq!(rig.fake.launched(), 1);
}

#[test]
fn what_may_have_landed_is_unknown_and_never_a_refusal() {
    let cases = [
        ApiError::Transport("timed out".to_owned()),
        ApiError::Malformed("not json".to_owned()),
        ApiError::Incomplete,
        ApiError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
            request_id: None,
        },
        ApiError::RateLimited {
            retry_after: None,
            request_id: None,
        },
        api(500, ErrorCode::InternalError),
        api(502, ErrorCode::Unparsed),
        // A familiar code behind a server error is a gateway speaking.
        api(500, ErrorCode::InsufficientCapacity),
        api(503, ErrorCode::QuotaExceeded),
        // Upstream codes may follow a launch that landed, whatever the status.
        api(
            503,
            ErrorCode::Provider("provider/internal-unavailable".to_owned()),
        ),
        api(400, ErrorCode::Provider("provider/quota".to_owned())),
        api(400, ErrorCode::InternalError),
        api(400, ErrorCode::Other("something/new".to_owned())),
        api(400, ErrorCode::Unparsed),
        api(404, ErrorCode::NotFound),
        api(429, ErrorCode::RateLimited),
    ];
    for error in cases {
        let rig = rig(live(5));
        rig.fake.fail_next(Call::Launch, error.clone());
        assert_eq!(
            rig.provider.acquire(&ask("a")),
            Acquired::Unknown,
            "{error:?}"
        );
    }
}

#[test]
fn definite_refusals_map_to_their_constants() {
    let cases = [
        (400, ErrorCode::InsufficientCapacity, RefusalCode::Capacity),
        (400, ErrorCode::QuotaExceeded, RefusalCode::Quota),
        (400, ErrorCode::InvalidParameters, RefusalCode::Shape),
        (404, ErrorCode::ObjectDoesNotExist, RefusalCode::Shape),
        (400, ErrorCode::FileSystemInWrongRegion, RefusalCode::Shape),
        (401, ErrorCode::InvalidApiKey, RefusalCode::Denied),
        (403, ErrorCode::Forbidden, RefusalCode::Denied),
        (403, ErrorCode::AccountInactive, RefusalCode::Denied),
        (403, ErrorCode::InvalidAddress, RefusalCode::Denied),
    ];
    for (status, code, want) in cases {
        let rig = rig(live(5));
        rig.fake.fail_next(Call::Launch, api(status, code.clone()));
        assert_eq!(
            rig.provider.acquire(&ask("a")),
            refused(want, Reason::Api(code.clone())),
            "{code:?}"
        );
        assert_eq!(rig.fake.launched(), 0);
    }
}

#[test]
fn a_region_without_capacity_is_a_capacity_refusal_and_holds_no_slot() {
    let rig = rig(live(1));
    let mut request = ask("a");
    request.candidate = candidate("region-z");
    assert_eq!(
        rig.provider.acquire(&request),
        refused(
            RefusalCode::Capacity,
            Reason::Api(ErrorCode::InsufficientCapacity)
        )
    );
    // The refusal launched nothing, so the single slot is free for the next one.
    rig.clock.advance(INTERVAL);
    assert_eq!(held_id(&rig.provider.acquire(&ask("b"))), "fake0001");
}

#[test]
fn a_launch_with_no_answer_holds_its_slot_until_a_list_shows_it_or_the_hold_ends() {
    let rig = rig(live(1));
    rig.fake
        .fail_next(Call::Launch, ApiError::Transport("timed out".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    rig.clock.advance(INTERVAL);
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        )
    );
    assert_eq!(rig.fake.count(Call::Launch), 1);
    // Ten minutes later the hold is over and a list is trusted.
    rig.clock.advance(Duration::from_secs(600));
    assert_eq!(held_id(&rig.provider.acquire(&ask("b"))), "fake0001");
}

#[test]
fn a_launch_that_landed_unseen_is_counted_once_not_twice() {
    let rig = rig(live(1));
    rig.fake
        .lose_response(Call::Launch, ApiError::Transport("reset".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    rig.clock.advance(INTERVAL);
    // The instance is listed and the pending launch is settled by it: one, not two.
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        )
    );
}

#[test]
fn the_hold_length_is_configurable() {
    let rig = rig(live(1).unknown_hold(Duration::from_secs(30)));
    rig.fake
        .fail_next(Call::Launch, ApiError::Transport("timed out".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    rig.clock.advance(Duration::from_secs(29));
    assert!(matches!(
        rig.provider.acquire(&ask("b")),
        Acquired::Refused(_)
    ));
    rig.clock.advance(Duration::from_secs(1));
    assert!(matches!(
        rig.provider.acquire(&ask("b")),
        Acquired::Accepted { .. }
    ));
}

#[test]
fn max_machines_is_checked_against_a_fresh_list_before_each_launch() {
    let rig = rig(live(2));
    for name in ["a", "b"] {
        held_id(&rig.provider.acquire(&ask(name)));
        rig.clock.advance(INTERVAL);
    }
    assert_eq!(
        rig.provider.acquire(&ask("c")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 2,
                max_machines: 2
            }
        )
    );
    assert_eq!(rig.fake.count(Call::Launch), 2);
    // The list is read again for each acquire, not remembered from the last one.
    assert_eq!(rig.fake.count(Call::ListInstances), 3);
}

#[test]
fn with_room_for_one_more_the_same_request_launches() {
    let rig = rig(live(3));
    for name in ["a", "b", "c"] {
        held_id(&rig.provider.acquire(&ask(name)));
        rig.clock.advance(INTERVAL);
    }
    assert_eq!(rig.fake.count(Call::Launch), 3);
}

#[test]
fn instances_of_other_owners_do_not_count_but_our_terminating_ones_do() {
    let rig = rig(live(2));
    rig.fake.add_instance(row(
        "theirs",
        Some("their-box"),
        Status::Active,
        vec![tag("team", "x")],
    ));
    held_id(&rig.provider.acquire(&ask("a")));
    rig.clock.advance(INTERVAL);
    rig.fake.set_status(&id("fake0001"), Status::Terminating);
    // A terminating instance may still bill.
    held_id(&rig.provider.acquire(&ask("b")));
    rig.clock.advance(INTERVAL);
    assert_eq!(
        rig.provider.acquire(&ask("c")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 2,
                max_machines: 2
            }
        )
    );
    // A terminated one does not.
    rig.fake.set_status(&id("fake0001"), Status::Terminated);
    held_id(&rig.provider.acquire(&ask("c")));
}

#[test]
fn the_refusal_reports_the_machines_held_and_the_cap_as_two_different_numbers() {
    let rig = rig(live(1));
    held_id(&rig.provider.acquire(&ask("a")));
    rig.clock.advance(INTERVAL);
    // Two more of our own appear beyond what this scheduler launched, so the account is over the cap.
    let mut extra = rig.fake.instances()[0].clone();
    extra.id = id("extra01");
    rig.fake.add_instance(extra.clone());
    extra.id = id("extra02");
    rig.fake.add_instance(extra);
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 3,
                max_machines: 1
            }
        )
    );
}

#[test]
fn a_renamed_instance_blocks_launches_instead_of_slipping_past_max_machines() {
    let rig = rig(live(2));
    held_id(&rig.provider.acquire(&ask("a")));
    rig.clock.advance(INTERVAL);
    rig.fake
        .update_instance(&id("fake0001"), &|i| i.name = Some("my-box".to_owned()));
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        refused(RefusalCode::Denied, Reason::HalfMarked { count: 1 })
    );
    assert_eq!(rig.fake.count(Call::Launch), 1);
}

#[test]
fn an_instance_whose_tags_were_replaced_blocks_launches() {
    let rig = rig(live(5));
    held_id(&rig.provider.acquire(&ask("a")));
    rig.clock.advance(INTERVAL);
    rig.fake
        .update_instance(&id("fake0001"), &|i| i.tags = vec![tag("note", "mine")]);
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        refused(RefusalCode::Denied, Reason::HalfMarked { count: 1 })
    );
    // Once it is terminated it is not billing and the block lifts.
    rig.fake.set_status(&id("fake0001"), Status::Terminated);
    held_id(&rig.provider.acquire(&ask("b")));
}

#[test]
fn a_suspect_does_not_stop_the_idempotent_answer_for_an_acquire_already_launched() {
    let rig = rig(live(5));
    held_id(&rig.provider.acquire(&ask("a")));
    rig.clock.advance(INTERVAL);
    rig.fake.add_instance(row(
        "odd",
        Some("someone"),
        Status::Active,
        vec![tag(SCHEDULER_TAG, SCHEDULER)],
    ));
    assert_eq!(held_id(&rig.provider.acquire(&ask("a"))), "fake0001");
    assert_eq!(rig.fake.launched(), 1);
    // A new acquire is held back by the same suspect.
    assert!(matches!(
        rig.provider.acquire(&ask("b")),
        Acquired::Refused(_)
    ));
}

#[test]
fn an_instance_listed_for_this_acquire_in_a_dead_state_is_returned_as_it_is() {
    for status in [Status::Unhealthy, Status::Preempted, Status::Active] {
        let rig = rig(live(5));
        held_id(&rig.provider.acquire(&ask("a")));
        rig.fake.set_status(&id("fake0001"), status.clone());
        let Acquired::Accepted { held, .. } = rig.provider.acquire(&ask("a")) else {
            panic!("not accepted");
        };
        assert_eq!(held.status, status);
        assert_eq!(rig.fake.launched(), 1);
    }
}

#[test]
fn a_terminating_instance_of_this_acquire_is_not_adopted() {
    let rig = rig(live(5));
    held_id(&rig.provider.acquire(&ask("a")));
    rig.fake.set_status(&id("fake0001"), Status::Terminating);
    rig.clock.advance(INTERVAL);
    assert_eq!(held_id(&rig.provider.acquire(&ask("a"))), "fake0002");
}

#[test]
fn a_failed_list_before_the_launch_means_no_launch() {
    let rig = rig(live(5));
    for error in [
        ApiError::Transport("down".to_owned()),
        ApiError::Incomplete,
        api(500, ErrorCode::InternalError),
    ] {
        rig.fake.fail_next(Call::ListInstances, error);
        assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    }
    assert_eq!(rig.fake.count(Call::Launch), 0);
    // With the list answering, the same request launches.
    held_id(&rig.provider.acquire(&ask("a")));
    assert_eq!(rig.fake.count(Call::Launch), 1);
}

#[test]
fn launches_are_spaced_by_the_interval_on_the_injected_clock() {
    let rig = rig(live(5));
    held_id(&rig.provider.acquire(&ask("a")));
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        Acquired::Deferred { wait: INTERVAL }
    );
    rig.clock.advance(Duration::from_secs(5));
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        Acquired::Deferred {
            wait: Duration::from_secs(7)
        }
    );
    assert_eq!(rig.fake.count(Call::Launch), 1);
    rig.clock.advance(Duration::from_secs(7));
    held_id(&rig.provider.acquire(&ask("b")));
    assert_eq!(rig.fake.count(Call::Launch), 2);
}

#[test]
fn an_answer_naming_two_instances_keeps_the_first_and_releases_the_other() {
    let rig = rig(live(5));
    rig.fake.launch_creates(2);
    let Acquired::Accepted { held, extras } = rig.provider.acquire(&ask("a")) else {
        panic!("not accepted");
    };
    assert_eq!(held.id, id("fake0001"));
    assert_eq!(extras.len(), 1);
    assert_eq!(extras[0].id, id("fake0002"));
    assert_eq!(
        extras[0].released,
        js_provider_lambdalabs::Released::Terminating(Status::Terminating)
    );
    let states: Vec<Status> = rig.fake.instances().into_iter().map(|i| i.status).collect();
    assert_eq!(states, [Status::Booting, Status::Terminating]);
}

#[test]
fn an_answer_naming_no_instance_is_unknown_and_holds_its_slot() {
    let rig = rig(live(1));
    rig.fake.launch_creates(0);
    assert_eq!(rig.provider.acquire(&ask("a")), Acquired::Unknown);
    assert_eq!(rig.fake.launched(), 0);
    rig.clock.advance(INTERVAL);
    assert_eq!(
        rig.provider.acquire(&ask("b")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        )
    );
}
