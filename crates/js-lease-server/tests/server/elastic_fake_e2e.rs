//! The scaler, the registry and the join tokens of the real server against the elastic fake
//! cloud, which keeps its own bill: a request waits, a machine is launched, a host says hello
//! with the token in its user data, the lease ends, the host is released, and the cloud's
//! bill for the machine is checked against the times the cloud saw. The refusals, the
//! preemption, the stuck states and plan mode that the scaler tests run on the pure step are
//! run here through the whole server. Time is the test's clock (the server's and the cloud's
//! move together in [`Rig::advance`]); nothing sleeps, nothing measures elapsed time and
//! nothing reaches a real cloud.

use js_elastic::{GpuClass, Offer, PriceUnit, RefusalCode};
use js_elastic_fake::{Bill, Billing, Call, ElasticFake, FakeClock, Step};
use js_provider::{Provider, StartAck, StopAck};
use js_provider_lambdalabs::Call as LambdaCall;
use js_scaler::{Blocked, Phase};
use js_types::{ExecMode, LeaseId};

use crate::lambda_e2e_support::Rig;
use crate::scaler_support::{account, elastic_cloud, spec, start, text_with};
use crate::support::Scratch;

/// The price of the cloud's one offer, in cents per hour: two cents a minute.
const PRICE: u64 = 120;

/// The one offer of the cloud: the size the test hosts announce (8 cores, 64 GiB, one a100).
fn offer() -> Offer {
    Offer {
        id: "gpu_1x_a100".to_owned(),
        zone: Some("us-west-1".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 512 << 30,
        gpus: 1,
        gpu_class: Some(GpuClass::parse("nvidia-a100").unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: Some(PRICE),
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

/// A cloud that bills as Lambda does (whole minutes) at [`PRICE`].
fn cloud() -> ElasticFake {
    ElasticFake::new(
        FakeClock::new(1_000_000_000_000),
        Billing::Lambda,
        vec![offer()],
    )
}

/// What the cloud would charge for `minutes` whole minutes of the offer.
fn minutes_billed(minutes: u64) -> Bill {
    Bill(u128::from(PRICE) * u128::from(minutes) * 60_000)
}

/// Takes a rig through launch, hello and a placed lease to a host that is registered and
/// idle once the lease ends: the machine's name, the host credential and the session.
async fn idle_host(rig: &Rig) -> (String, String, crate::agents_support::Session) {
    let (name, token) = rig.launch().await;
    let (me, credential) = rig.join(&token, &name).await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
    assert!(matches!(rig.present(), StartAck::Unknown));
    let lease = LeaseId::new(1, 1);
    assert_eq!(rig.ready(&credential, &me).await, lease.to_string());
    let ack = rig.present();
    assert!(matches!(&ack, StartAck::Started { host } if host.as_str() == name));
    let stopped = rig.registry().provider().stop(&lease);
    assert!(matches!(stopped, StopAck::Stopped { .. }));
    for _ in 0..2 {
        assert_eq!(rig.beat(&credential, &me).await.status, 200);
    }
    (name, credential, me)
}

/// Ticks in steps of ten seconds, the host beating until its release is sent, until the
/// cloud has been asked to release a machine; the cloud's time of that call.
async fn until_release_is_asked(
    rig: &Rig,
    cloud: &ElasticFake,
    credential: &str,
    me: &crate::agents_support::Session,
) -> u64 {
    for _ in 0..40 {
        rig.advance(10);
        let beat = rig.beat(credential, me).await;
        if cloud.count(Call::Release) == 0 {
            assert_eq!(beat.status, 200, "{}", beat.body);
        }
        rig.tick().await;
        if cloud.count(Call::Release) > 0 {
            return cloud.clock().now_ms();
        }
    }
    panic!("the host was never released");
}

/// Asserts the whole life of a machine through the real server and the cloud's own bill. A
/// request waits; the cloud refuses the first launch for capacity and makes nothing (the
/// bill is nothing); the next launch succeeds; the host says hello with the token in the
/// user data; the lease is placed and ends; the host is drained, marked and released; the
/// cloud answers `Terminating` and then the record goes `Gone` when the cloud says it no
/// longer has the machine. The cloud's bill is then exactly the whole minutes between its
/// launch and the release call it saw, at the price in its catalog: two cents a minute. The
/// scaler's record holds the same price. It catches a launch that bills before it happens,
/// a release that is never sent, a machine left running, and a bill that is not the cloud's.
#[tokio::test]
async fn a_machine_launched_after_a_capacity_refusal_is_billed_to_the_minute_and_released() {
    let cloud = cloud();
    cloud.script(Call::Acquire, &[Step::Refuse(RefusalCode::Capacity)]);
    let rig = Rig::elastic("e2e_cloud_life", cloud.clone()).await;
    rig.tick().await;
    rig.wait();
    rig.tick().await;
    assert_eq!((cloud.count(Call::Acquire), cloud.launched()), (1, 0));
    assert_eq!(cloud.bill(), Bill::default());
    for _ in 0..3 {
        if cloud.launched() == 0 {
            rig.tick().await;
        }
    }
    assert_eq!((cloud.count(Call::Acquire), cloud.launched()), (2, 1));
    let launched_at = cloud.clock().now_ms();
    assert_eq!(rig.phase(), Phase::Launching);
    assert_eq!(rig.state().records.instances.len(), 1);
    assert_eq!(rig.state().records.instances[0].price_cents_per_hour, PRICE);

    let (name, token) = (rig.name(), rig.token(0));
    assert!(rig.script(0).contains(&format!("burst:{name}")));
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    let (me, credential) = rig.join(&token, &name).await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
    assert!(matches!(rig.present(), StartAck::Unknown));
    let lease = LeaseId::new(1, 1);
    assert_eq!(rig.ready(&credential, &me).await, lease.to_string());
    let ack = rig.present();
    assert!(matches!(&ack, StartAck::Started { host } if host.as_str() == name));
    assert_eq!(
        cloud.count(Call::Release),
        0,
        "a leased host is not released"
    );

    let stopped = rig.registry().provider().stop(&lease);
    assert!(matches!(stopped, StopAck::Stopped { .. }));
    for _ in 0..2 {
        assert_eq!(rig.beat(&credential, &me).await.status, 200);
    }
    let released_at = until_release_is_asked(&rig, &cloud, &credential, &me).await;
    assert_eq!(rig.phase(), Phase::Releasing);
    for _ in 0..10 {
        if rig.phase() != Phase::Gone {
            rig.advance(10);
            rig.tick().await;
        }
    }
    assert_eq!(rig.phase(), Phase::Gone);
    assert!(cloud.running().is_empty());
    assert_eq!(cloud.launched(), 1);

    let minutes = (released_at - launched_at).div_ceil(60_000);
    assert!((1..=10).contains(&minutes), "{minutes} minutes");
    assert_eq!(cloud.bill(), minutes_billed(minutes));
    assert_eq!(cloud.bill().cents_up(), PRICE / 60 * minutes);
    rig.assert_status_is_clean(&[token.as_str(), credential.as_str()])
        .await;
}

/// Asserts the refusals that hold the pool back, through the server: for quota, denial and a
/// request that fits nothing the pool says why after the tick that learns of the refusal,
/// nothing is launched and the bill is nothing, the cloud is asked once during the whole
/// cooldown, and after it (ten minutes) the machine is launched. It catches a refusal
/// that does not stop the pool from asking again, and one that stops it for longer.
#[tokio::test]
async fn a_quota_denial_or_shape_refusal_holds_the_pool_for_the_cooldown() {
    let cases = [
        (RefusalCode::Quota, Blocked::Quota),
        (RefusalCode::Denied, Blocked::Denied),
        (RefusalCode::Shape, Blocked::Shape),
    ];
    for (code, blocked) in cases {
        let cloud = cloud();
        cloud.script(Call::Acquire, &[Step::Refuse(code)]);
        let rig = Rig::elastic(&format!("e2e_cloud_refuse_{code:?}"), cloud.clone()).await;
        rig.tick().await;
        rig.wait();
        rig.tick().await;
        rig.tick().await;
        assert_eq!(rig.state().blocked["burst"], blocked, "{code:?}");
        assert_eq!(cloud.launched(), 0);
        assert_eq!(cloud.bill(), Bill::default());
        assert!(rig.state().records.instances.is_empty());

        rig.advance(9 * 60);
        rig.tick().await;
        assert_eq!(
            cloud.count(Call::Acquire),
            1,
            "{code:?}: asked during the cooldown"
        );
        rig.advance(2 * 60);
        rig.h.checks.step().await;
        rig.tick().await;
        assert_eq!(cloud.launched(), 1, "{code:?}");
    }
}

/// Asserts preemption through the server: the cloud reclaims a registered machine two and a
/// half minutes after its launch; the next tick's list shows it preempted, the record goes
/// to `Releasing` and the release is sent, and the record is `Gone` when the cloud says
/// it has no more of the machine. The cloud's bill for the machine stops at the
/// preemption: three whole minutes, however long the test goes on. It catches a preempted
/// machine the scaler goes on counting as running, and one it never releases.
#[tokio::test]
async fn a_preempted_machine_is_released_and_billed_only_until_it_was_taken() {
    let cloud = cloud();
    let rig = Rig::elastic("e2e_cloud_preempt", cloud.clone()).await;
    let (name, token) = rig.launch().await;
    let (me, credential) = rig.join(&token, &name).await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
    let id = cloud.running()[0].clone();
    rig.withdraw();

    rig.advance(150);
    assert_eq!(rig.beat(&credential, &me).await.status, 200);
    cloud.preempt(&id);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_eq!(cloud.count(Call::Release), 1);
    for _ in 0..10 {
        if rig.phase() != Phase::Gone {
            rig.advance(10);
            rig.tick().await;
        }
    }
    assert_eq!(rig.phase(), Phase::Gone);
    rig.advance(3600);
    assert_eq!(cloud.bill_of(&id), minutes_billed(3));
    assert_eq!(cloud.bill(), minutes_billed(3));
    assert!(cloud.running().is_empty());
}

/// Asserts a release the cloud refuses forever, through the server: the host is idle and
/// the release is sent and refused; for 44 minutes after that the record stays `Releasing`
/// and nothing is said; at 45 (`boot_timeout` and `unknown_hold`) the server raises the
/// alert that the provider does not confirm the machine gone; meanwhile the machine goes on
/// running and billing (90 cents in the 45 minutes), the record still counts against the
/// pool, and the release is asked again every tick. When the cloud accepts, the record is
/// `Gone` and the alert clears. It catches an Unknown or a refusal read as gone, a stuck
/// machine nobody is told about, and an alert that never clears.
#[tokio::test]
async fn a_release_the_cloud_refuses_forever_alerts_at_boot_timeout_plus_unknown_hold() {
    let cloud = cloud();
    let rig = Rig::elastic("e2e_cloud_stuck_release", cloud.clone()).await;
    let (_, credential, me) = idle_host(&rig).await;
    cloud.script_forever(Call::Release, Step::Refuse(RefusalCode::Denied));
    let asked_at = until_release_is_asked(&rig, &cloud, &credential, &me).await;
    assert_eq!(rig.phase(), Phase::Releasing);

    let alerts = || rig.h.running.scaler().unwrap().alerts();
    let stuck = |text: &String| text.contains("does not confirm");
    for minute in 1..=45 {
        rig.advance(60);
        rig.tick().await;
        let raised = alerts().iter().any(stuck);
        assert_eq!(raised, minute >= 45, "minute {minute}: {:?}", alerts());
    }
    let asked = cloud.count(Call::Release);
    assert!(asked >= 45, "{asked} release calls");
    assert_eq!(rig.phase(), Phase::Releasing);
    assert!(rig.state().records.instances[0].counts());
    assert_eq!(cloud.running().len(), 1);
    let now = cloud.clock().now_ms();
    assert_eq!(
        cloud.bill_between(asked_at, now).cents_up(),
        PRICE * 45 / 60
    );

    cloud.script_forever(Call::Release, Step::Pass);
    for _ in 0..10 {
        if rig.phase() != Phase::Gone {
            rig.advance(10);
            rig.tick().await;
        }
    }
    assert_eq!(rig.phase(), Phase::Gone);
    assert!(cloud.running().is_empty());
    assert!(!alerts().iter().any(stuck), "{:?}", alerts());
}

/// Asserts a list that fails forever, through the server: with a request waiting, for 30
/// minutes the pool launches nothing (no acquire reaches the cloud) and the scaler says why
/// on every tick (`ListFailed`). When the list works the machine is launched. This is a
/// block, not an alert: the server shows the reason only in the scaler's state and logs, and
/// raises no alert for a list that never works; that belongs to the alert sink of cost
/// safety's next part. It catches a launch made without knowing what exists.
#[tokio::test]
async fn a_list_that_fails_forever_launches_nothing_and_the_scaler_says_why() {
    let cloud = cloud();
    cloud.script_forever(Call::List, Step::Lost { landed: false });
    let rig = Rig::elastic("e2e_cloud_list_fails", cloud.clone()).await;
    rig.tick().await;
    rig.wait();
    for minute in 0..30 {
        rig.advance(60);
        rig.tick().await;
        let blocked = rig.state().blocked["burst"];
        assert_eq!(blocked, Blocked::ListFailed, "minute {minute}");
    }
    assert_eq!(cloud.count(Call::Acquire), 0);
    assert_eq!(cloud.launched(), 0);
    assert!(cloud.count(Call::List) >= 30);
    assert_eq!(rig.registry().waiting().len(), 1);

    cloud.script_forever(Call::List, Step::Pass);
    rig.tick().await;
    assert_eq!(cloud.launched(), 1);
}

/// Asserts a 429 storm on launches, through the server: for twenty half-minutes every
/// acquire is answered with a deferral, nothing is launched, the request still waits, the
/// scaler keeps one record for it and asks again every other tick; when the storm ends the
/// machine is launched. It catches a deferral that fails the request, and a retry that
/// makes a record, and so a machine, per attempt.
#[tokio::test]
async fn a_429_storm_on_launches_keeps_one_record_and_launches_when_it_ends() {
    let cloud = cloud();
    cloud.script_forever(Call::Acquire, Step::Throttled);
    let rig = Rig::elastic("e2e_cloud_429", cloud.clone()).await;
    rig.tick().await;
    rig.wait();
    for _ in 0..20 {
        rig.advance(30);
        rig.tick().await;
    }
    assert_eq!(cloud.launched(), 0);
    assert_eq!(rig.state().records.instances.len(), 1);
    assert_eq!(rig.registry().waiting().len(), 1);
    assert!(cloud.count(Call::Acquire) >= 5);

    cloud.script_forever(Call::Acquire, Step::Pass);
    for _ in 0..4 {
        rig.advance(30);
        rig.tick().await;
    }
    assert_eq!(cloud.launched(), 1);
    assert_eq!(rig.state().records.instances.len(), 1);
}

/// Asserts plan mode through the server, for each kind of provider the tests have: with a
/// pool in `plan` mode and requests presented for thirty ticks, the cloud under it is
/// never asked to launch or to release (the elastic fake records no `acquire` and no
/// `release`; the Lambda account records no launch and no terminate), and nothing is
/// launched. It catches a plan pool that spends, through either provider.
#[tokio::test]
async fn a_pool_in_plan_mode_makes_zero_create_calls_through_any_provider() {
    for elastic in [true, false] {
        let scratch = Scratch::new(if elastic {
            "e2e_plan_elastic"
        } else {
            "e2e_plan_lambda"
        });
        let cloud = cloud();
        let lambda = match elastic {
            true => {
                elastic_cloud(&scratch, &cloud);
                None
            }
            false => Some(account(&scratch)),
        };
        let source = text_with(&scratch, "plan", "initialise", &[("exec", "\"container\"")]);
        let h = start(scratch, &source).await;
        let registry = h.running.registry().unwrap();
        for n in 0..30 {
            h.clock.advance(std::time::Duration::from_secs(10));
            let ack = registry.provider().start(spec(1 + n, &format!("r{n}")));
            assert!(!matches!(ack, StartAck::Started { .. }));
            h.ticks.step().await;
        }
        let calls = cloud.calls();
        assert!(!calls.contains(&Call::Acquire), "{calls:?}");
        assert!(!calls.contains(&Call::Release), "{calls:?}");
        assert_eq!(cloud.launched(), 0);
        if let Some(account) = lambda {
            assert_eq!(account.count(LambdaCall::Launch), 0);
            assert_eq!(account.count(LambdaCall::Terminate), 0);
            assert_eq!(account.launched(), 0);
        }
    }
}
