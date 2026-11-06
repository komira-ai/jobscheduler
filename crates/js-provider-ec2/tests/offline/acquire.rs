//! What an acquire does: the checks that refuse it with nothing sent, the launch guard,
//! and the launch with its tags and client token.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, CapacityProvider, Extra, Held, HostName, HostStatus,
    Observed, Purchase, Reason, Refusal, RefusalCode, Released, SchedulerId,
};
use js_provider_ec2::{
    Action, ApiError, ClientToken, InstanceState, InstanceType, LabelError, LaunchPurchase,
};
use tracing::Level;

use crate::rig::{
    ARM_IMAGE, Account, HOLD, SCHEDULER, X86_IMAGE, ZONE, ask, candidate, config, host, id, live,
    live_x86_only, logged, marks, name_of, offer, ours, rig, rig_over, row, stranger, tag,
};

/// The machine of an answer that must be `Accepted`.
fn accepted(answer: Acquired) -> Held {
    match answer {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

fn refused(code: RefusalCode, reason: Reason) -> Acquired {
    Acquired::Refused(Refusal { code, reason })
}

/// How many times the account was asked to launch.
fn runs(account: &Account) -> usize {
    let actions = account.actions();
    actions
        .iter()
        .filter(|action| **action == Action::RunInstances)
        .count()
}

/// Asserts that two acquires with one id answer the same machine and the cloud launches
/// once, with one client token derived from the scheduler and the acquire; and that when
/// the cloud's window for the token has passed, a provider that remembers nothing finds
/// the machine again by its tags and launches nothing. It catches a retry that launches a
/// second machine.
#[test]
fn acquire_is_idempotent_on_the_acquire_id() {
    let first_rig = rig(live(2));
    let request = ask("a1");
    let first = accepted(first_rig.provider.acquire(&request));
    let second = accepted(first_rig.provider.acquire(&request));
    assert_eq!(first.id, second.id);
    assert_eq!(first.name, Some(request.name.to_string()));
    assert_eq!(second.name, first.name);
    assert_eq!(first_rig.account.launched(), 1);
    let sent = first_rig.account.run_requests();
    assert_eq!(sent.len(), 1);
    let scheduler = SchedulerId::new(SCHEDULER).unwrap();
    assert_eq!(
        sent[0].client_token,
        ClientToken::derive(&request.acquire, &scheduler)
    );

    first_rig.account.forget_client_tokens();
    let restarted = rig_over(Arc::clone(&first_rig.account), live(2));
    let third = accepted(restarted.provider.acquire(&request));
    assert_eq!(third.id, first.id);
    assert_eq!(third.acquire, "a1");
    assert_eq!(first_rig.account.launched(), 1);
    assert_eq!(runs(&first_rig.account), 1);
}

/// Asserts that a launch that is throttled answers `Unknown`, sends no second launch while
/// the hold lasts (the retry is deferred by the time that is left of it), lists the acquire
/// as unresolved, and that a throttle after the launch landed is settled by the next try
/// finding the machine, so the cloud launched once. It catches a throttle read as a refusal,
/// which would let a retry launch twice.
#[test]
fn throttle_is_unknown_and_never_a_second_launch() {
    let rig = rig(live(2));
    let throttle = ApiError::answered("RequestLimitExceeded");
    rig.account
        .fail_next(Action::RunInstances, throttle.clone());
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(rig.account.launched(), 0);
    assert_eq!(rig.provider.unresolved(), [AcquireId::new("a1").unwrap()]);
    assert_eq!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Deferred { wait: HOLD }
    );
    rig.clock.advance(Duration::from_secs(100));
    assert_eq!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Deferred {
            wait: HOLD - Duration::from_secs(100)
        }
    );
    assert_eq!(runs(&rig.account), 1, "the retries sent no launch");

    rig.account.lose_after_next(Action::RunInstances, throttle);
    assert_eq!(rig.provider.acquire(&ask("a2")), Acquired::Unknown);
    assert_eq!(rig.account.launched(), 1);
    let landed = accepted(rig.provider.acquire(&ask("a2")));
    assert_eq!(landed.id.as_str(), rig.account.instances()[0].id.as_str());
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.provider.unresolved(), [AcquireId::new("a1").unwrap()]);
}

/// Asserts that the image of a launch is the one configured for the architecture of the
/// offer, `arm64` or `x86_64`, and that an offer of an architecture with no image is
/// refused as not allowed, with nothing sent and a warning that names the architecture and
/// not the payload.
#[test]
fn the_image_is_selected_by_offer_architecture() {
    let rig = rig(live(3));
    let arm = AcquireRequest {
        offer: offer("c9g.large/on-demand", "arm64"),
        ..ask("a1")
    };
    accepted(rig.provider.acquire(&arm));
    accepted(rig.provider.acquire(&ask("a2")));
    let images: Vec<String> = rig
        .account
        .run_requests()
        .iter()
        .map(|run| run.image.to_string())
        .collect();
    assert_eq!(images, [ARM_IMAGE, X86_IMAGE]);

    let narrow = crate::rig::rig(live_x86_only(2));
    let (answer, warnings) = logged(Level::WARN, || narrow.provider.acquire(&arm));
    assert_eq!(answer, refused(RefusalCode::Shape, Reason::NotAllowed));
    assert!(warnings.contains("no image is configured"), "{warnings}");
    assert!(warnings.contains("arm64"), "{warnings}");
    assert!(!warnings.contains("user-data"), "{warnings}");
    assert!(narrow.account.actions().is_empty());
    let other = AcquireRequest {
        offer: offer("m9.large/on-demand", "riscv64"),
        ..ask("a3")
    };
    assert_eq!(
        narrow.provider.acquire(&other),
        refused(RefusalCode::Shape, Reason::NotAllowed)
    );
}

/// Asserts that a provider that is not enabled refuses every acquire as denied, and that
/// nothing is sent to the cloud.
#[test]
fn a_provider_that_is_not_enabled_refuses_and_sends_nothing() {
    let rig = rig(config(1).live());
    assert_eq!(
        rig.provider.acquire(&ask("a1")),
        refused(RefusalCode::Denied, Reason::Disabled)
    );
    assert!(rig.account.actions().is_empty());
}

/// Asserts that a dry run builds the launch, logs it once at info level and after that at
/// debug level, refuses it as denied with the dry-run reason, and sends nothing; and that
/// no log line carries the payload.
#[test]
fn a_dry_run_logs_the_launch_once_and_sends_nothing() {
    let info = rig(config(1).enable());
    let (answers, lines) = logged(Level::INFO, || {
        [
            info.provider.acquire(&ask("a1")),
            info.provider.acquire(&ask("a1")),
        ]
    });
    let dry = refused(RefusalCode::Denied, Reason::DryRun);
    assert_eq!(answers, [dry.clone(), dry]);
    assert_eq!(lines.matches("dry run: a launch would be sent").count(), 1);
    assert!(!lines.contains("user-data"), "{lines}");
    assert!(info.account.actions().is_empty());

    let debug = rig(config(1).enable());
    let (_, lines) = logged(Level::DEBUG, || {
        debug.provider.acquire(&ask("a1"));
        debug.provider.acquire(&ask("a1"));
    });
    assert_eq!(lines.matches("dry run: a launch would be sent").count(), 2);
}

/// Asserts that an offer the provider does not launch is refused as a shape error with
/// nothing sent: one with no zone, one that is not on demand, spot or a Capacity Block
/// with terms (a Capacity Block with no terms, or no purchase stated), and one of an
/// instance type the allow list leaves out; and that an allowed type, and a spot offer,
/// are launched, a spot offer as spot.
#[test]
fn an_offer_the_provider_does_not_launch_is_refused_as_shape() {
    let rig = rig(live(5));
    let no_zone = AcquireRequest {
        offer: js_elastic::Offer {
            zone: None,
            ..candidate()
        },
        ..ask("a1")
    };
    let block = AcquireRequest {
        offer: js_elastic::Offer {
            purchase: Some(Purchase::CapacityBlock),
            ..candidate()
        },
        ..ask("a2")
    };
    let unstated = AcquireRequest {
        offer: js_elastic::Offer {
            purchase: None,
            ..candidate()
        },
        ..ask("a3")
    };
    for request in [no_zone, block, unstated] {
        assert_eq!(
            rig.provider.acquire(&request),
            refused(RefusalCode::Shape, Reason::NotAllowed)
        );
    }
    assert!(rig.account.actions().is_empty());

    let only = BTreeSet::from([InstanceType("c9g.large".to_owned())]);
    let limited = crate::rig::rig(live(5).allowed(only));
    assert_eq!(
        limited.provider.acquire(&ask("a4")),
        refused(RefusalCode::Shape, Reason::NotAllowed)
    );
    assert!(limited.account.actions().is_empty());
    let arm = AcquireRequest {
        offer: offer("c9g.large/on-demand", "arm64"),
        ..ask("a5")
    };
    accepted(limited.provider.acquire(&arm));

    let spot = AcquireRequest {
        offer: js_elastic::Offer {
            purchase: Some(Purchase::Spot),
            ..offer("m9.large/spot", "x86_64")
        },
        ..ask("a6")
    };
    accepted(rig.provider.acquire(&spot));
    let sent = rig.account.run_requests();
    assert_eq!(sent[0].purchase, LaunchPurchase::Spot);
    assert_eq!(sent[0].instance_type, InstanceType("m9.large".to_owned()));
    assert_eq!(sent[0].zone.as_str(), ZONE);
    assert_eq!(
        limited.account.run_requests()[0].purchase,
        LaunchPurchase::OnDemand
    );
}

/// Asserts that a host name that does not carry this scheduler's prefix is refused as a
/// shape error with nothing sent, because the instance could not be recognised as ours
/// afterwards.
#[test]
fn a_host_name_of_another_scheduler_is_refused() {
    let rig = rig(live(1));
    let acquire = AcquireId::new("a1").unwrap();
    let other = SchedulerId::new("sched-two").unwrap();
    let request = AcquireRequest {
        name: HostName::derive(&other, &acquire),
        ..ask("a1")
    };
    assert_eq!(
        rig.provider.acquire(&request),
        refused(RefusalCode::Shape, Reason::Name)
    );
    assert!(rig.account.actions().is_empty());
}

/// Asserts that a label which cannot be a tag is refused as a shape error that carries the
/// words for why, with nothing sent; and that labels which can are tags of the instance,
/// after the three marks, in the order given.
#[test]
fn labels_become_tags_or_the_acquire_is_refused() {
    let rig = rig(live(2));
    let bad = AcquireRequest {
        labels: vec![("Team".to_owned(), "infra".to_owned())],
        ..ask("a1")
    };
    assert_eq!(
        rig.provider.acquire(&bad),
        refused(
            RefusalCode::Shape,
            Reason::Label(LabelError::Key("Team".to_owned()).to_string())
        )
    );
    assert!(rig.account.actions().is_empty());
    let good = AcquireRequest {
        labels: vec![
            ("team".to_owned(), "infra".to_owned()),
            ("tier".to_owned(), "gpu".to_owned()),
        ],
        ..ask("a2")
    };
    accepted(rig.provider.acquire(&good));
    let mut expected = marks(SCHEDULER, "a2");
    expected.push(tag("js-label:team", "infra"));
    expected.push(tag("js-label:tier", "gpu"));
    assert_eq!(rig.account.instances()[0].tags, expected);
}

/// Asserts that the launched instance carries the scheduler, the acquire and the host name
/// as tags, is of the offer's type in the offer's zone from the configured image, and that
/// the answer names it as booting, with the offer, the zone and the name.
#[test]
fn the_launched_instance_carries_the_scheduler_tags() {
    let rig = rig(live(1));
    let request = ask("a1");
    let held = accepted(rig.provider.acquire(&request));
    let launched = &rig.account.instances()[0];
    assert_eq!(launched.tags, marks(SCHEDULER, "a1"));
    assert_eq!(
        launched.tag("Name"),
        Some(name_of(SCHEDULER, "a1").as_str())
    );
    assert_eq!(launched.instance_type.as_str(), "m9.large");
    assert_eq!(launched.zone.as_str(), ZONE);
    assert_eq!(launched.image.as_str(), X86_IMAGE);
    assert_eq!(held.id, launched.id.host_ref());
    assert_eq!(held.acquire, "a1");
    assert_eq!(held.name, Some(request.name.to_string()));
    assert_eq!(held.offer, "m9.large/on-demand");
    assert_eq!(held.zone.as_deref(), Some(ZONE));
    assert_eq!(held.status, HostStatus::Booting);
}

/// Asserts that when the cap is reached the acquire is denied with the count and the cap,
/// and nothing is launched; that a launch with no answer counts toward the cap while it is
/// unresolved; and that the cap is the number of machines, not of acquires.
#[test]
fn the_cap_counts_launched_and_unanswered_machines() {
    let rig = rig(live(1));
    accepted(rig.provider.acquire(&ask("a1")));
    let at_max = refused(
        RefusalCode::Denied,
        Reason::AtMaxMachines {
            held: 1,
            max_machines: 1,
        },
    );
    assert_eq!(rig.provider.acquire(&ask("a2")), at_max);
    assert_eq!(rig.account.launched(), 1);

    let lost = crate::rig::rig(live(1));
    lost.account
        .fail_next(Action::RunInstances, ApiError::Timeout);
    assert_eq!(lost.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lost.provider.acquire(&ask("a2")), at_max);
    assert_eq!(runs(&lost.account), 1);
}

/// Asserts that a launch whose answer was lost after it landed counts toward the cap once:
/// while it is unresolved and no list shows its machine, the launch is counted; once a list
/// shows that machine, the machine is counted and the unresolved launch of the same acquire
/// is not counted again. It catches a count that adds the unresolved launch beside the
/// machine it made, which would refuse an acquire the cap still has room for.
#[test]
fn an_unresolved_launch_and_the_machine_it_made_count_once() {
    let rig = rig(live(2));
    rig.account
        .lose_after_next(Action::RunInstances, ApiError::Timeout);
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.provider.capacity().can_add, Some(1));
    let listed = rig.provider.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].acquire, "a1");
    let capacity = rig.provider.capacity();
    assert_eq!((capacity.supplied, capacity.can_add), (1, Some(1)));
    accepted(rig.provider.acquire(&ask("a2")));
    assert_eq!(rig.account.launched(), 2);
}

/// Asserts that an instance of the acquire that carries some of this scheduler's marks and
/// not all (here the right tags and another name) denies the acquire as half-marked with
/// nothing launched; and that the provider counts such an instance, once a list has shown
/// it, toward the cap for every other acquire.
#[test]
fn a_half_marked_instance_blocks_its_acquire_and_counts_toward_the_cap() {
    let rig = rig(live(1));
    let mut tags = marks(SCHEDULER, "a1");
    tags[2] = tag("Name", "renamed");
    rig.account
        .add_instance(row(&stranger(8), InstanceState::Running, tags));
    assert_eq!(
        rig.provider.acquire(&ask("a1")),
        refused(RefusalCode::Denied, Reason::HalfMarked { count: 1 })
    );
    assert_eq!(rig.account.launched(), 0);
    assert_eq!(rig.provider.capacity().can_add, Some(0));
    assert_eq!(
        rig.provider.acquire(&ask("a2")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        )
    );
}

/// Asserts that when the instances of the acquire cannot be read, the acquire answers
/// `Unknown` and no launch is sent, and that the acquire is not left unresolved: nothing was
/// sent that could have landed.
#[test]
fn a_failed_read_before_the_launch_is_unknown_and_sends_no_launch() {
    let rig = rig(live(1));
    let reset = ApiError::Transport("reset".to_owned());
    rig.account.fail_next(Action::DescribeInstances, reset);
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(rig.account.actions(), [Action::DescribeInstances]);
    assert!(rig.provider.unresolved().is_empty());
    accepted(rig.provider.acquire(&ask("a1")));
}

/// Asserts that a terminated instance of the acquire is not its machine: the acquire
/// launches a new one.
#[test]
fn a_terminated_instance_of_the_acquire_is_not_its_machine() {
    let rig = rig(live(2));
    let old = stranger(7);
    rig.account
        .add_instance(ours(&old, InstanceState::Terminated, "a1"));
    let held = accepted(rig.provider.acquire(&ask("a1")));
    assert_ne!(held.id, host(&old));
    assert_eq!(rig.account.launched(), 1);
}

/// Asserts that a launch that took effect and answered that it started nothing is
/// `Unknown` and unresolved, and that the next try finds the machine it started.
#[test]
fn a_launch_that_answers_no_instance_is_unknown_and_found_again() {
    let rig = rig(live(1));
    rig.account.answer_no_instance();
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.provider.unresolved(), [AcquireId::new("a1").unwrap()]);
    let held = accepted(rig.provider.acquire(&ask("a1")));
    assert_eq!(held.id, rig.account.instances()[0].id.host_ref());
    assert!(rig.provider.unresolved().is_empty());
    assert_eq!(rig.account.launched(), 1);
}

/// Asserts that when a launch names more instances than the one asked for, the extras are
/// released at once and reported with what the release read back, an extra the cloud does
/// not know is reported as unknown and not as gone (a read right after a launch can lag),
/// and that all of them count toward the cap and the supply as they are.
#[test]
fn a_launch_that_started_extra_instances_releases_them() {
    let account = Arc::new(Account::new());
    let extra = stranger(5);
    let lagging = stranger(6);
    account.add_instance(ours(&extra, InstanceState::Pending, "extra"));
    account.name_also(vec![id(&extra), id(&lagging)]);
    let rig = rig_over(account, live(5));
    let Acquired::Accepted { held, extras } = rig.provider.acquire(&ask("a1")) else {
        panic!("not accepted");
    };
    assert_eq!(
        extras,
        [
            Extra {
                id: host(&extra),
                released: Released::Terminating(HostStatus::Terminating)
            },
            Extra {
                id: host(&lagging),
                released: Released::Unknown
            },
        ]
    );
    assert_eq!(rig.account.terminates(), 1);
    assert_eq!(held.status, HostStatus::Booting);
    let capacity = rig.provider.capacity();
    assert_eq!((capacity.supplied, capacity.can_add), (2, Some(2)));
    assert_eq!(
        rig.provider.observe(&host(&extra)),
        Observed::Present(HostStatus::Terminating)
    );
}
