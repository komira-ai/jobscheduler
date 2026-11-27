//! A provider that restarted while a start waited on it (`providing`): it lost every source
//! it held, as tierstore does at each of its releases, and answers their `Mount` "the source
//! is not prepared here" (`UNAVAILABLE`). The start asks it from the start again: each `Mount`
//! answered unavailable is followed by the source's `Prepare`, and the next `Mount` waits for
//! that `Prepare` to be taken. The providers here keep the sources they take and lose them
//! when the test restarts them (`fake_providers`); the agent's own restart is
//! `providers_restart`'s.

use js_agent::lifecycle::ProviderAnswer;
use js_agent_wire::{HeldState, Layer, MountMode, Outcome, refusal};

use super::fake_providers::{FakeProviders, WAIT_MS, mount, prepare, rig, tree, with_layer};
use super::per_job::{job_start, listing};
use super::{Harness, names};

/// Steps `h` four times and returns every event they gave, as `names` has them.
fn steps(h: &mut Harness) -> Vec<String> {
    (0..4).flat_map(|_| names(&h.life.step())).collect()
}

/// The `Prepare` and `Mount` calls the providers were sent, in order.
fn asked(h: &Harness) -> Vec<String> {
    h.log
        .borrow()
        .iter()
        .filter(|line| line.starts_with("provider prepare ") || line.starts_with("provider mount "))
        .cloned()
        .collect()
}

/// The message of the `start-failed` finding of `job_id`.
fn start_failed(h: &Harness, job_id: &str) -> String {
    let found = h.life.findings();
    let found = found.iter().find(|finding| {
        finding.run_id.as_deref() == Some(job_id) && finding.code == "start-failed"
    });
    found.expect("the finding").message.clone()
}

/// The `Prepare` the start of `with_layer` sends for its source.
const PREPARE: &str = "provider prepare tier j1#0 k1 ReadOnly";

/// The `Mount` of that source.
const MOUNT: &str = "provider mount tier j1#0";

/// A rig whose provider keeps the sources it takes, has taken the source of `j1` (a start of
/// `with_layer`) and has its first `Mount` still out, and then restarts: it has lost the
/// source and will answer the directory `t1` of the root `ro` once the source is taken again.
fn restarted(name: &str) -> (Harness, FakeProviders) {
    let (mut h, providers) = rig(name, FakeProviders::keep_sources);
    providers.then("tier", &mount("j1", 0), ProviderAnswer::Pending);
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    h.apply(vec![job_start(with_layer("j1"))]);
    assert_eq!(asked(&h), [PREPARE, MOUNT]);
    providers.restart();
    (h, providers)
}

/// Asserts that a start whose provider restarted between the source's `Prepare` and its
/// `Mount` (the `Mount` still out, then answered "the source is not prepared here") sends the
/// source's `Prepare` again in the step that is handed that answer, sends the next `Mount`
/// only after it, and runs on the directory the provider then answers, with the test's clock
/// not moved at all: well inside `--mount-provider-wait`, where it failed at that wait
/// `input-unavailable` when the agent asked only `Mount` once the source had been taken.
/// Catches a source taken once and never asked from the start again after the provider lost
/// it.
#[test]
fn a_provider_that_restarted_between_prepare_and_mount_is_asked_to_prepare_again() {
    let (mut h, _providers) = restarted("lost-mid");
    h.life.step();
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, PREPARE],
        "prepared again at once"
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(steps(&mut h), ["started j1"]);
    assert_eq!(asked(&h), [PREPARE, MOUNT, PREPARE, MOUNT]);
    let looked_up = h.fake.looked_up("j1").expect("looked up");
    assert_eq!(
        looked_up.layers,
        [Layer::HostPath {
            path: tree(&h, "ro", "t1")
        }]
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
}

/// Asserts that a provider that is still down after its restart (its `Prepare` answered
/// unavailable, "socket missing") is asked `Prepare` at each step and not `Mount` until it
/// takes the source again, and that the start then runs. Catches a `Mount` sent to a provider
/// that has not taken the source back, which can only be answered "not prepared here", and a
/// source given up after the first `Prepare` that failed.
#[test]
fn a_provider_still_down_after_its_restart_is_asked_to_prepare_until_it_takes_the_source() {
    let (mut h, providers) = restarted("lost-down");
    let down = ProviderAnswer::Unavailable("socket missing".to_owned());
    let again = prepare("j1", 0, "k1", MountMode::ReadOnly);
    providers.then("tier", &again, down.clone());
    providers.then("tier", &again, down);
    h.life.step();
    h.life.step();
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, PREPARE, PREPARE],
        "no Mount while the Prepare is not taken"
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(steps(&mut h), ["started j1"]);
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, PREPARE, PREPARE, PREPARE, MOUNT]
    );
}

/// Asserts that a provider that restarted and never takes the source again fails the start
/// `input-unavailable` once `--mount-provider-wait` has passed since its admission and not a
/// moment before, naming the last reason it gave (its `Prepare`'s), and that the source is
/// released. Catches a start held for ever by a provider that came back without it, and a
/// failure that names the stale `Mount` answer instead of why the source is not taken.
#[test]
fn a_provider_that_never_takes_the_source_back_fails_the_start_at_the_wait() {
    let (mut h, providers) = restarted("lost-never");
    let down = ProviderAnswer::Unavailable("socket missing".to_owned());
    let again = prepare("j1", 0, "k1", MountMode::ReadOnly);
    providers.always("tier", &again, down);
    h.life.step();
    h.clock.advance(WAIT_MS - 1);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    h.clock.advance(1);
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_UNAVAILABLE));
    assert_eq!(
        start_failed(&h, "j1"),
        "the provider tier did not mount source 0 within 30 s: socket missing"
    );
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
}

/// Asserts that a provider that restarted and then refuses the source's `Prepare` fails the
/// start `provider-refused` at the step that is handed the refusal, with its reason, and
/// releases the source. Catches a refusal of the `Prepare` sent again that is waited out as if
/// the provider were down.
#[test]
fn a_prepare_refused_after_the_restart_fails_the_start_provider_refused() {
    let (mut h, providers) = restarted("lost-refused");
    let again = prepare("j1", 0, "k1", MountMode::ReadOnly);
    let refused = ProviderAnswer::Refused("no such tree".to_owned());
    providers.always("tier", &again, refused);
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::PROVIDER_REFUSED));
    assert_eq!(
        start_failed(&h, "j1"),
        "the provider tier refused source 0: no such tree"
    );
    steps(&mut h);
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
    assert!(h.fake.created().is_empty());
}

/// Asserts that a `Mount` answered unavailable by a provider that did not restart ("not
/// ready": it still holds the source and is making its tree) is followed by the source's
/// `Prepare` too, which it takes as it took it before, and that the start runs on the next
/// `Mount`'s directory; and that a step whose `Mount` is still out, or is answered with no
/// directory, sends no `Prepare`. Catches a `Prepare` sent again while the `Mount` is out, or
/// for an answer that does not say the source may be lost.
#[test]
fn only_a_mount_answered_unavailable_is_followed_by_a_prepare() {
    let (mut h, providers) = rig("lost-not-ready", FakeProviders::keep_sources);
    let asked_mount = mount("j1", 0);
    providers.then("tier", &asked_mount, ProviderAnswer::Pending);
    providers.then("tier", &asked_mount, ProviderAnswer::Pending);
    providers.then("tier", &asked_mount, ProviderAnswer::Done(None));
    let not_ready = ProviderAnswer::Unavailable("not ready".to_owned());
    providers.then("tier", &asked_mount, not_ready);
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    h.apply(vec![job_start(with_layer("j1"))]);
    h.life.step();
    assert_eq!(asked(&h), [PREPARE, MOUNT], "a Mount out: nothing sent");
    h.life.step();
    assert_eq!(asked(&h), [PREPARE, MOUNT], "no directory: no Prepare");
    h.life.step();
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, MOUNT, PREPARE],
        "unavailable: Mount, then Prepare at once"
    );
    assert_eq!(steps(&mut h), ["started j1"]);
    assert_eq!(asked(&h), [PREPARE, MOUNT, MOUNT, PREPARE, MOUNT]);
}
