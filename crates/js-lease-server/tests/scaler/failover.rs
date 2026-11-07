//! What the scaler's shell adds to the step's failover over a fake world: the news that a
//! catalog refresh completed, and the log line of each refused acquire. Time is an injected
//! clock; nothing sleeps.

use std::time::Duration;

use js_elastic::{Acquired, Offer, Reason, Refusal, RefusalCode};
use js_scaler::{Records, Settings};

use crate::fakes::{START, job, live, logged, offer, rig};

/// Settings that wait 60 seconds between attempts and pass an instance type over for an hour.
fn settings() -> Settings {
    Settings {
        acquire_retry: Duration::from_secs(60),
        quota_backoff: Duration::from_secs(3600),
        ..live()
    }
}

/// The answer of a provider that refuses with `code`.
fn refused(code: RefusalCode) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api("refused".to_owned()),
    })
}

/// How many acquires the fake has seen.
fn acquires(log: &[String]) -> usize {
    log.iter()
        .filter(|entry| entry.starts_with("acquire:"))
        .count()
}

/// A rig with one job queued whose every acquire is refused for capacity, ticked until
/// the first refusal is taken in; the clock has not moved.
async fn refused_everywhere() -> crate::fakes::Rig {
    let rig = rig(Records::default(), settings());
    rig.world().queued = vec![job("r1")];
    rig.world().acquire_answers = (0..8).map(|_| refused(RefusalCode::Capacity)).collect();
    for _ in 0..3 {
        rig.scaler.tick().await;
    }
    assert_eq!(acquires(&rig.log()), 1);
    rig
}

/// Asserts that the shell passes a completed catalog refresh to the step: a request every
/// offer was refused for waits its `acquire_retry` and asks nothing at a tick before it
/// ends, as long as the provider's count of fetches has not moved; when the count moves, the
/// next tick asks again at once, and the tick after that, with the count still, waits again.
/// It catches a refresh that is never told to the step, one told at every tick, and a count
/// read as a change when it has not moved.
#[tokio::test]
async fn the_shell_tells_the_step_a_catalog_refresh_completed() {
    let rig = refused_everywhere().await;
    rig.advance(Duration::from_secs(30));
    rig.scaler.tick().await;
    assert_eq!(acquires(&rig.log()), 1);

    rig.world().catalog_fetches = 1;
    rig.scaler.tick().await;
    assert_eq!(acquires(&rig.log()), 2);

    rig.advance(Duration::from_secs(1));
    for _ in 0..2 {
        rig.scaler.tick().await;
    }
    assert_eq!(acquires(&rig.log()), 2);
}

/// Asserts that a request every offer was refused for is asked about again once its
/// `acquire_retry` has passed, with no refresh of the catalog: a tick just before it asks
/// nothing, a tick at it acquires. It catches a request that is never retried without a
/// refresh, and one retried early.
#[tokio::test]
async fn a_request_every_offer_was_refused_for_is_asked_again_after_the_retry() {
    let rig = refused_everywhere().await;
    rig.advance(Duration::from_secs(59));
    rig.scaler.tick().await;
    assert_eq!(acquires(&rig.log()), 1);

    rig.advance(Duration::from_secs(1));
    rig.scaler.tick().await;
    assert_eq!(acquires(&rig.log()), 2);
}

/// The log lines that tell of a refusal for the offer `id`.
fn refusal_lines(id: &str) -> Vec<String> {
    let needle = format!("offer={id} ");
    logged()
        .lines()
        .filter(|line| line.contains("acquire refused") && line.contains(&needle))
        .map(str::to_owned)
        .collect()
}

/// A rig with one job queued, whose offer is `id` in the zone `zone` and which the
/// provider refuses once with `code`; ticked three times with the clock still.
async fn refused_once(id: &str, zone: &str, code: RefusalCode) {
    let rig = rig(Records::default(), settings());
    let one = Offer {
        id: id.to_owned(),
        zone: Some(zone.to_owned()),
        ..offer()
    };
    rig.world().offers = vec![one];
    rig.world().queued = vec![job("r1")];
    rig.world().acquire_answers = [refused(code)].into();
    for _ in 0..3 {
        rig.scaler.tick().await;
    }
    assert_eq!(acquires(&rig.log()), 1);
}

/// Asserts that a refusal for capacity is logged once with its offer, zone and code and
/// without an instance type to pass over. It catches a refusal that is not logged, one
/// logged at each tick, and a capacity refusal logged as if it passed an instance type over.
#[tokio::test]
async fn a_refused_acquire_is_logged_with_its_offer_and_code() {
    refused_once("a10-logcap", "zcap", RefusalCode::Capacity).await;
    let lines = refusal_lines("a10-logcap");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("zone=zcap"), "{}", lines[0]);
    assert!(lines[0].contains("code=Capacity"), "{}", lines[0]);
    assert!(!lines[0].contains("instance_type="), "{}", lines[0]);
    assert!(!lines[0].contains("until_ms="), "{}", lines[0]);
}

/// Asserts that a refusal for quota is logged once with its offer, zone and code, the
/// instance type and region passed over, and the time the pass-over ends: the clock's time
/// at the refusal plus the quota backoff, in milliseconds. It catches a quota refusal logged
/// without what it passed over, and a pass-over that ends at the wrong time.
#[tokio::test]
async fn a_quota_refusal_is_logged_with_what_it_passes_over_and_until_when() {
    refused_once("a10-logquota", "zquota", RefusalCode::Quota).await;
    let lines = refusal_lines("a10-logquota");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let until = START + 3_600_000;
    assert!(lines[0].contains("zone=zquota"), "{}", lines[0]);
    assert!(lines[0].contains("code=Quota"), "{}", lines[0]);
    assert!(
        lines[0].contains("instance_type=a10-logquota"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("region=zquota"), "{}", lines[0]);
    assert!(
        lines[0].contains(&format!("until_ms={until}")),
        "{}",
        lines[0]
    );
}
