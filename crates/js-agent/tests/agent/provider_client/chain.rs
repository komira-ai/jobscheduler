//! The `Mount` the client sends itself once a source's `Prepare` has succeeded
//! (`docs/design/start-latency.md`, eighth part): sent with no ask of it and only after the
//! `Prepare`'s answer, handed over at the lifecycle's next asks, ringing the wake once for the
//! two calls, a failure of it handed over and sent again like any call's, and nothing of it
//! once the source is released. A refused or failed `Prepare` is followed by nothing, and a
//! `Mount` the lifecycle asked itself is not sent twice: both are the flight table's
//! (`provider/flight_tests.rs`).

use std::time::Duration;

use js_agent::lifecycle::{ProviderAnswer, Providers};
use js_agent::provider::CallBounds;
use js_agent_wire::MountMode;
use js_proto::provider::v1 as pb;
use js_testkit::mount_provider::{Kind, Seen, mount_path};

use super::{
    GUARD, HOST, NAME, assert_unavailable, mount, prepare, received, release, rig, rig_with,
    ringing, settle, source, taken,
};

/// Asserts that a prepare that succeeds is followed by the mount of its source with no ask
/// of it: the provider receives the mount while nothing but the prepare was asked, and after
/// the prepare. The lifecycle's next asks are then handed the prepare's answer and the
/// directory, and send nothing more. It catches a mount that waits for the lifecycle's next
/// step after the prepare is answered, a mount sent before the prepare's answer, and a mount
/// the lifecycle asks for sent a second time.
#[test]
fn a_prepare_that_succeeds_is_followed_by_its_mount_with_no_ask() {
    let mut rig = rig("pc-chain");
    rig.fake.hold(Kind::Mount);
    let call = prepare("r1", 0, "k", MountMode::ReadOnly);
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Pending);
    assert!(
        rig.fake.wait_parked(1, GUARD),
        "the provider was not sent the mount"
    );
    assert_eq!(
        rig.fake.requests(),
        vec![
            Seen::Prepare(pb::PrepareRequest {
                source: source("js.r1", 0),
                r#ref: "k".to_owned(),
                mode: pb::MountMode::ReadOnly as i32,
            }),
            Seen::Mount(pb::MountRequest {
                source: source("js.r1", 0),
            }),
        ]
    );
    rig.fake.unhold(Kind::Mount);
    assert!(rig.client.wait_answered(NAME, &mount("r1", 0), GUARD));
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Done(None));
    assert_eq!(
        rig.client.ask(NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 0)))
    );
    assert_eq!(
        received(&rig.fake, Kind::Mount),
        1,
        "the mount was sent twice"
    );
}

/// Asserts that a prepare that is followed rings nothing of its own and that the mount rings
/// once when it answers: while the provider holds the mount no ring has come, the prepare's
/// answer is there to be handed over and the mount is pending without being sent again; once
/// the provider lets it go one ring comes and the directory is handed over. It catches a ring
/// for the prepare, which steps the worker for nothing, and a mount that rings nothing.
#[test]
fn a_followed_prepare_rings_nothing_and_its_mount_rings_once() {
    let mut rig = rig("pc-chain-wake");
    let (wake, rings) = ringing();
    rig.client.set_wake(wake);
    rig.fake.hold(Kind::Mount);
    let call = prepare("r1", 0, "k", MountMode::ReadOnly);
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Pending);
    assert!(rig.fake.wait_parked(1, GUARD));
    assert!(rings.try_recv().is_err(), "rung for the prepare");
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Done(None));
    assert_eq!(
        rig.client.ask(NAME, &mount("r1", 0)),
        ProviderAnswer::Pending
    );
    rig.fake.unhold(Kind::Mount);
    rings
        .recv_timeout(GUARD)
        .expect("rung once the mount was in");
    assert_eq!(
        rig.client.ask(NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 0)))
    );
    assert!(rings.try_recv().is_err(), "one ring for the two calls");
    assert_eq!(received(&rig.fake, Kind::Mount), 1);
}

/// Asserts that a mount that follows a prepare and fails rings after its pause, and that the
/// ask it brings is handed the failure and sends the mount again, which then answers the
/// directory. It catches a failed mount of the client's own that is lost, so the source waits
/// for ever, or that is handed over and never sent again.
#[test]
fn a_followed_mount_that_fails_is_handed_over_and_sent_again() {
    let mut rig = rig_with(
        "pc-chain-fail",
        CallBounds {
            retry: Duration::ZERO,
            ..CallBounds::default()
        },
    );
    let (wake, rings) = ringing();
    rig.client.set_wake(wake);
    rig.fake.fail_mount(1);
    let call = prepare("r1", 0, "k", MountMode::ReadOnly);
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Pending);
    rings
        .recv_timeout(GUARD)
        .expect("rung for the failed mount");
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Done(None));
    assert_unavailable(
        &rig.client.ask(NAME, &mount("r1", 0)),
        "the fake provider is not ready",
    );
    rings
        .recv_timeout(GUARD)
        .expect("rung once the mount sent again was in");
    assert_eq!(
        rig.client.ask(NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 0)))
    );
    assert_eq!(received(&rig.fake, Kind::Mount), 2);
}

/// Asserts that a release forgets what the client holds of its source: after a prepare and the
/// mount that followed it were answered and not handed over, the release is sent, and the next
/// ask of the prepare sends it again rather than handing over the old answer, and is followed
/// by a mount of its own. It catches a release that leaves a source's prepare or mount in the
/// client's table, where a prepare still out would be followed by a mount after the release.
#[test]
fn a_release_forgets_its_sources_prepare_and_mount() {
    let mut rig = rig("pc-chain-release");
    let call = prepare("r1", 0, "k", MountMode::ReadOnly);
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Pending);
    assert!(rig.client.wait_answered(NAME, &mount("r1", 0), GUARD));
    assert_eq!(
        settle(&mut rig.client, NAME, &release("r1", 0)),
        ProviderAnswer::Done(None)
    );
    assert!(rig.fake.prepared().is_empty());
    assert_eq!(
        rig.client.ask(NAME, &call),
        ProviderAnswer::Pending,
        "the prepare's answer was handed over after its source was released"
    );
    assert_eq!(
        taken(&mut rig.client, NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 0)))
    );
    assert_eq!(received(&rig.fake, Kind::Prepare), 2);
    assert_eq!(received(&rig.fake, Kind::Mount), 2);
}
