//! An acquire of a Capacity Block offer: the purchase, the launch into the block, the end
//! the machine carries, and every way the purchase can go wrong.

use std::sync::Arc;
use std::time::Duration;

use js_elastic::{Acquired, CapacityProvider, Held, HostStatus, Reason, Refusal, RefusalCode};
use js_provider_ec2::{
    Action, ApiError, BLOCK_END_TAG, BlockPurchase, InstanceState, LaunchPurchase, ReservationState,
};

use crate::rig::{
    Account, BLOCK_END, BLOCK_OFFERING, HOLD, SCHEDULER, ZONE, ask, ask_block, block_offer,
    block_offering, config, live, ours, rig, rig_over, row, stranger, tag,
};

fn refused(code: RefusalCode, reason: Reason) -> Acquired {
    Acquired::Refused(Refusal { code, reason })
}

/// An account that offers the rig's block.
fn account() -> Arc<Account> {
    let account = Arc::new(Account::new());
    account.add_block_offering(block_offering());
    account
}

/// How many times the account was asked for `action`.
fn count(account: &Account, action: Action) -> usize {
    account
        .actions()
        .iter()
        .filter(|called| **called == action)
        .count()
}

/// Asserts that an acquire of a block offer reads the reservations of the acquire, buys
/// the block with the scheduler and acquire tags, and launches into the reservation it
/// bought, in that order; that the launch names the reservation and carries the block's end
/// in the `js-block-end` tag; and that the machine answered holds that end. Catches an
/// acquire of a block offer that is refused as not allowed, or launched as on demand.
#[test]
fn a_block_acquire_buys_the_block_then_launches_into_it() {
    let rig = rig_over(account(), live(2));
    let request = ask_block("a1");
    let answer = rig.provider.acquire(&request);
    let Acquired::Accepted { held, extras } = answer else {
        panic!("not accepted: {answer:?}");
    };
    assert!(extras.is_empty());
    assert_eq!(
        rig.account.actions(),
        [
            Action::DescribeInstances,
            Action::DescribeCapacityReservations,
            Action::PurchaseCapacityBlock,
            Action::RunInstances,
        ]
    );
    let bought = rig.account.reservations();
    assert_eq!(bought.len(), 1);
    assert_eq!(bought[0].state, ReservationState::Active);
    assert_eq!(bought[0].end_unix_seconds, BLOCK_END);
    assert_eq!(
        rig.account.purchases(),
        [BlockPurchase {
            offering: BLOCK_OFFERING.to_owned(),
            tags: vec![tag("js-scheduler", SCHEDULER), tag("js-acquire-id", "a1")],
        }]
    );
    let runs = rig.account.run_requests();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].purchase,
        LaunchPurchase::CapacityBlock(bought[0].id.clone())
    );
    assert_eq!(runs[0].instance_type.as_str(), "m9.large");
    assert_eq!(runs[0].zone.as_str(), ZONE);
    assert!(
        runs[0]
            .tags
            .contains(&tag(BLOCK_END_TAG, &BLOCK_END.to_string()))
    );
    assert_eq!(
        held,
        Held {
            id: rig.account.instances()[0].id.host_ref(),
            acquire: "a1".to_owned(),
            name: Some(request.name.to_string()),
            offer: block_offer().id,
            zone: Some(ZONE.to_owned()),
            status: HostStatus::Booting,
            ends_unix_seconds: Some(BLOCK_END),
        }
    );
    // A list does not know the offer an instance was launched from, only its type.
    let listed = Held {
        offer: "m9.large".to_owned(),
        ..held
    };
    assert_eq!(rig.provider.list().unwrap(), [listed]);
}

/// Asserts that a repeat of an acquire that was answered buys no second block and
/// launches no second instance, and answers the machine with the end it was launched with.
/// Catches a retry that buys again.
#[test]
fn a_repeated_block_acquire_buys_once() {
    let rig = rig_over(account(), live(2));
    let Acquired::Accepted { held: first, .. } = rig.provider.acquire(&ask_block("a1")) else {
        panic!("not accepted");
    };
    let Acquired::Accepted { held: second, .. } = rig.provider.acquire(&ask_block("a1")) else {
        panic!("not accepted");
    };
    assert_eq!(first.id, second.id);
    assert_eq!(second.ends_unix_seconds, Some(BLOCK_END));
    assert_eq!(rig.account.purchases().len(), 1);
    assert_eq!(rig.account.launched(), 1);
}

/// Asserts that a purchase the cloud refuses by an error code in the refusal table launches
/// nothing, is refused under the code's constant with the code as the reason, leaves the
/// acquire unresolved nowhere, and that the retry of the same acquire buys and launches.
/// Catches a refused purchase that is launched anyway, or kept as unknown.
#[test]
fn a_refused_purchase_launches_nothing() {
    let rig = rig_over(account(), live(2));
    rig.account.fail_next(
        Action::PurchaseCapacityBlock,
        ApiError::answered("UnauthorizedOperation"),
    );
    assert_eq!(
        rig.provider.acquire(&ask_block("a1")),
        refused(
            RefusalCode::Denied,
            Reason::Api("UnauthorizedOperation".to_owned())
        )
    );
    assert_eq!(count(&rig.account, Action::RunInstances), 0);
    assert!(rig.account.reservations().is_empty());
    assert!(rig.provider.unresolved().is_empty());
    assert!(matches!(
        rig.provider.acquire(&ask_block("a1")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(rig.account.purchases().len(), 1);
}

/// Asserts that an offering the account does not offer is refused by the cloud's
/// `InvalidParameterValue` as a shape error, with nothing launched.
#[test]
fn a_purchase_of_an_unknown_offering_is_refused_as_shape() {
    let rig = rig_over(Arc::new(Account::new()), live(2));
    assert_eq!(
        rig.provider.acquire(&ask_block("a1")),
        refused(
            RefusalCode::Shape,
            Reason::Api("InvalidParameterValue".to_owned())
        )
    );
    assert_eq!(rig.account.launched(), 0);
}

/// Asserts that a purchase that fails with no cloud answer (a timeout) is unknown, launches
/// nothing and keeps the acquire unresolved; that a retry while the hold lasts is deferred
/// and does not buy; and that, when the purchase had landed, the retry after the hold
/// finds the reservation by its tags, buys nothing more and launches into it. Catches a
/// retry that buys a second block.
#[test]
fn a_purchase_with_no_answer_is_unknown_and_the_retry_reuses_the_block() {
    let rig = rig_over(account(), live(2));
    rig.account
        .lose_after_next(Action::PurchaseCapacityBlock, ApiError::Timeout);
    assert_eq!(rig.provider.acquire(&ask_block("a1")), Acquired::Unknown);
    assert_eq!(count(&rig.account, Action::RunInstances), 0);
    assert_eq!(rig.provider.unresolved().len(), 1);
    assert_eq!(rig.account.purchases().len(), 1);
    assert_eq!(
        rig.provider.acquire(&ask_block("a1")),
        Acquired::Deferred { wait: HOLD }
    );
    assert_eq!(count(&rig.account, Action::PurchaseCapacityBlock), 1);

    rig.clock.advance(HOLD + Duration::from_secs(1));
    let Acquired::Accepted { held, .. } = rig.provider.acquire(&ask_block("a1")) else {
        panic!("not accepted");
    };
    assert_eq!(held.ends_unix_seconds, Some(BLOCK_END));
    assert_eq!(rig.account.purchases().len(), 1);
    assert_eq!(rig.account.reservations().len(), 1);
    assert_eq!(
        rig.account.run_requests()[0].purchase,
        LaunchPurchase::CapacityBlock(rig.account.reservations()[0].id.clone())
    );
}

/// Asserts that a purchase that fails with no cloud answer and did not land is unknown too,
/// and that after the hold the retry buys once. Catches a retry that never buys.
#[test]
fn a_purchase_that_never_landed_is_bought_by_the_retry() {
    let rig = rig_over(account(), live(2));
    rig.account
        .fail_next(Action::PurchaseCapacityBlock, ApiError::Timeout);
    assert_eq!(rig.provider.acquire(&ask_block("a1")), Acquired::Unknown);
    assert!(rig.account.purchases().is_empty());
    rig.clock.advance(HOLD + Duration::from_secs(1));
    assert!(matches!(
        rig.provider.acquire(&ask_block("a1")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(rig.account.purchases().len(), 1);
}

/// Asserts that a reservation that is not active (scheduled, payment-pending, and a state
/// this release does not know) launches nothing and is refused as a capacity refusal with
/// the state as the cloud spells it; that the acquire is left unresolved nowhere; and that
/// the retry of the same acquire finds the same reservation, buys nothing, refuses again
/// while it is not active, and launches into it once it is. Catches a launch into a block
/// that has not started.
#[test]
fn a_reservation_that_is_not_active_launches_nothing() {
    let states = [
        ReservationState::Scheduled,
        ReservationState::PaymentPending,
        ReservationState::Other("brand-new".to_owned()),
    ];
    for state in states {
        let rig = rig_over(account(), live(2));
        rig.account.buy_blocks_as(state.clone());
        let expected = refused(RefusalCode::Capacity, Reason::Api(state.name().to_owned()));
        assert_eq!(rig.provider.acquire(&ask_block("a1")), expected);
        assert_eq!(count(&rig.account, Action::RunInstances), 0);
        assert!(rig.provider.unresolved().is_empty());
        assert_eq!(rig.provider.acquire(&ask_block("a1")), expected);
        assert_eq!(rig.account.purchases().len(), 1);
        assert_eq!(count(&rig.account, Action::RunInstances), 0);

        let id = rig.account.reservations()[0].id.clone();
        rig.account
            .set_reservation_state(&id, ReservationState::Active);
        assert!(matches!(
            rig.provider.acquire(&ask_block("a1")),
            Acquired::Accepted { .. }
        ));
        assert_eq!(rig.account.purchases().len(), 1);
        assert_eq!(
            rig.account.run_requests()[0].purchase,
            LaunchPurchase::CapacityBlock(id)
        );
    }
}

/// Asserts that a read of the reservations that fails answers unknown, buys nothing,
/// launches nothing and leaves no unresolved acquire, so that the next try reads again and
/// buys. Catches a purchase made without knowing whether the block was already bought.
#[test]
fn a_failed_reservation_read_buys_and_launches_nothing() {
    let rig = rig_over(account(), live(2));
    rig.account.fail_next(
        Action::DescribeCapacityReservations,
        ApiError::Transport("reset".to_owned()),
    );
    assert_eq!(rig.provider.acquire(&ask_block("a1")), Acquired::Unknown);
    assert_eq!(count(&rig.account, Action::PurchaseCapacityBlock), 0);
    assert_eq!(count(&rig.account, Action::RunInstances), 0);
    assert!(rig.provider.unresolved().is_empty());
    assert!(matches!(
        rig.provider.acquire(&ask_block("a1")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(rig.account.purchases().len(), 1);
}

/// Asserts that a launch into the block that the cloud refuses leaves the block bought and
/// refuses the acquire, and that the retry launches into the same block without buying
/// another. Catches a retry that pays for a second block.
#[test]
fn a_refused_launch_into_a_bought_block_is_retried_without_buying() {
    let rig = rig_over(account(), live(2));
    rig.account.fail_next(
        Action::RunInstances,
        ApiError::answered("InsufficientInstanceCapacity"),
    );
    assert_eq!(
        rig.provider.acquire(&ask_block("a1")),
        refused(
            RefusalCode::Capacity,
            Reason::Api("InsufficientInstanceCapacity".to_owned())
        )
    );
    assert!(matches!(
        rig.provider.acquire(&ask_block("a1")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(rig.account.purchases().len(), 1);
}

/// Asserts that a dry run buys nothing and asks the cloud nothing for a block offer, and is
/// refused as a dry run.
#[test]
fn a_dry_run_purchases_nothing() {
    let rig = rig_over(account(), config(2).enable());
    assert_eq!(
        rig.provider.acquire(&ask_block("a1")),
        refused(RefusalCode::Denied, Reason::DryRun)
    );
    assert!(rig.account.actions().is_empty());
}

/// Asserts that the machine cap and an instance already launched for the acquire are
/// answered before anything is bought: at the cap a block offer is refused with no
/// purchase, and an acquire whose instance exists answers it without buying again.
#[test]
fn the_guards_come_before_the_purchase() {
    let rig = rig_over(account(), live(1));
    assert!(matches!(
        rig.provider.acquire(&ask("a0")),
        Acquired::Accepted { .. }
    ));
    assert_eq!(
        rig.provider.acquire(&ask_block("a1")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        )
    );
    assert_eq!(count(&rig.account, Action::DescribeCapacityReservations), 0);
    assert!(rig.account.purchases().is_empty());

    let existing = rig_over(account(), live(2));
    existing
        .account
        .add_instance(ours(&stranger(2), InstanceState::Running, "a2"));
    let Acquired::Accepted { held, .. } = existing.provider.acquire(&ask_block("a2")) else {
        panic!("not accepted");
    };
    assert_eq!(held.id.as_str(), stranger(2));
    assert!(existing.account.purchases().is_empty());
}

/// Asserts that a list reads the end of the block from the `js-block-end` tag, and has none
/// for an instance with no such tag or a tag that is not a whole number of seconds. Catches
/// a list that drops the end of a machine in a block.
#[test]
fn a_list_reads_the_block_end_from_the_tag() {
    let rig = rig(live(5));
    let tagged = |end: &str, acquire: &str| {
        let mut tags = crate::rig::marks(SCHEDULER, acquire);
        tags.push(tag(BLOCK_END_TAG, end));
        tags
    };
    rig.account.add_instance(row(
        &stranger(1),
        InstanceState::Running,
        tagged("1800086400", "a1"),
    ));
    rig.account.add_instance(row(
        &stranger(2),
        InstanceState::Running,
        tagged("soon", "a2"),
    ));
    rig.account
        .add_instance(ours(&stranger(3), InstanceState::Running, "a3"));
    let ends: Vec<Option<u64>> = rig
        .provider
        .list()
        .unwrap()
        .iter()
        .map(|held| held.ends_unix_seconds)
        .collect();
    assert_eq!(ends, [Some(1_800_086_400), None, None]);
}

/// Asserts that the two actions of the block calls are named as the cloud spells them.
#[test]
fn the_block_actions_carry_the_clouds_names() {
    assert_eq!(
        Action::PurchaseCapacityBlock.name(),
        "PurchaseCapacityBlock"
    );
    assert_eq!(
        Action::DescribeCapacityReservations.to_string(),
        "DescribeCapacityReservations"
    );
}
