//! The failure brake through the scaler's shell over a fake world: a tripped pool launches
//! nothing and raises one alert, the operator's reset is applied at the next tick with its
//! records written first, a reset whose records cannot be written is asked again, and asking
//! twice is one reset. Time is an injected clock; nothing sleeps.

use std::time::Duration;

use js_scaler::{Blocked, Brake, Millis, Records};

use crate::fakes::{START, live, request, rig};

fn tripped() -> Records {
    let mut records = Records::default();
    let brake = Brake {
        failures: 3,
        tripped_at: Some(Millis::new(START)),
    };
    records.brake.insert("gpu".to_owned(), brake);
    records
}

/// Asserts that a tripped pool with a request waiting makes no call at all, says why in
/// `blocked`, and raises one alert that names the pool and the reset. It catches a trip that
/// the shell does not honour and an alert that is never raised.
#[tokio::test]
async fn a_tripped_pool_launches_nothing_and_raises_one_alert() {
    let rig = rig(tripped(), live());
    rig.world().waiting = vec![request("r1")];
    for _ in 0..3 {
        rig.scaler.tick().await;
        rig.advance(Duration::from_secs(60));
    }
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.scaler.state().blocked["gpu"], Blocked::Tripped);
    let alerts = rig.scaler.alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains("pool \"gpu\" is in plan"), "{alerts:?}");
    assert!(
        alerts[0].contains("/v1/pools/gpu/brake/reset"),
        "{alerts:?}"
    );
}

/// Asserts the reset: asked twice before one tick it is one reset, the tick writes the
/// records first and then launches for the waiting request, the brake and the alert are gone
/// and the written records hold no brake. It catches a reset that is not written before the
/// launch it allows, and one that needs a second ask.
#[tokio::test]
async fn a_reset_is_applied_at_the_next_tick_and_written_first() {
    let rig = rig(tripped(), live());
    rig.world().waiting = vec![request("r1")];
    rig.scaler.tick().await;
    assert_eq!(rig.scaler.alerts().len(), 1);

    rig.scaler.reset_brake();
    rig.scaler.reset_brake();
    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    let log = rig.log();
    assert_eq!(log.first().map(String::as_str), Some("save"), "{log:?}");
    assert!(
        log.iter().any(|entry| entry.starts_with("acquire:")),
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|entry| *entry == "save").count(),
        1,
        "{log:?}"
    );
    assert!(rig.scaler.state().records.brake.is_empty());
    assert!(rig.scaler.alerts().is_empty());
    assert!(rig.world().saved.as_ref().unwrap().brake.is_empty());
}

/// Asserts that a reset whose records cannot be written is not lost: the tick calls nothing
/// else and the pool stays tripped, and the next tick, with the disk back and no new ask,
/// makes the reset. It catches a request that is dropped when its write fails.
#[tokio::test]
async fn a_reset_that_cannot_be_written_is_asked_again() {
    let rig = rig(tripped(), live());
    rig.scaler.tick().await;
    rig.world().save_ok = false;
    rig.scaler.reset_brake();
    rig.scaler.tick().await;
    assert_eq!(rig.log(), ["save"]);
    assert!(rig.scaler.state().records.tripped("gpu"));

    rig.world().save_ok = true;
    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    assert!(!rig.scaler.state().records.tripped("gpu"));
    assert!(rig.world().saved.as_ref().unwrap().brake.is_empty());
}

/// Asserts that a reset of a pool with no brake writes nothing and calls nothing, however
/// often it is asked. It catches a reset that costs a write per ask.
#[tokio::test]
async fn a_reset_with_nothing_to_reset_writes_nothing() {
    let rig = rig(Records::default(), live());
    for _ in 0..3 {
        rig.scaler.reset_brake();
        rig.scaler.tick().await;
        rig.advance(Duration::from_secs(5));
    }
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert!(rig.world().saved.is_none());
}
