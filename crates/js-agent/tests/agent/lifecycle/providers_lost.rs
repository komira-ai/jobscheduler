//! A provider that restarted while a start waited on it (`providing`): it lost every source
//! it held, as tierstore does at each of its releases, and answers their `Mount` "the source
//! is not prepared here" (`UNAVAILABLE`). The start asks it from the start again: after a
//! `Mount` answered unavailable, the source's next ask (once its wait is over, `pace`) sends
//! its `Prepare` again, and the `Mount` follows once that is taken. The providers here keep
//! the sources they take and lose them when the test restarts them (`fake_providers`); the
//! agent's own restart is `providers_restart`'s.

use js_agent::lifecycle::{Event, ProviderAnswer};
use js_agent_wire::{HeldState, Layer, MountMode, Outcome, refusal};

use super::fake_providers::{FakeProviders, WAIT_MS, mount, prepare, rig, tree, with_layer};
use super::per_job::{job_start, listing};
use super::{Harness, names};

/// The step period of the tests that step on the clock, in milliseconds.
const TICK_MS: u64 = 100;

/// Steps `h` four times and returns every event they gave, as `names` has them.
fn steps(h: &mut Harness) -> Vec<String> {
    (0..4).flat_map(|_| names(&h.life.step())).collect()
}

/// Steps `h` every [`TICK_MS`] for `ms` milliseconds.
fn tick_for(h: &mut Harness, ms: u64) {
    for _ in 0..ms / TICK_MS {
        h.clock.advance(TICK_MS);
        h.life.step();
    }
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

/// The message of the last finding of `code` of `job_id`.
fn finding(h: &Harness, job_id: &str, code: &str) -> String {
    let found = h.life.findings();
    let found = found
        .iter()
        .rev()
        .find(|finding| finding.run_id.as_deref() == Some(job_id) && finding.code == code);
    found.expect("the finding").message.clone()
}

/// The `Prepare` of the source of `j1`, a start of `with_layer`.
fn prepare_j1() -> js_agent::lifecycle::ProviderCall {
    prepare("j1", 0, "k1", MountMode::ReadOnly)
}

/// What a provider that is down answers.
fn down() -> ProviderAnswer {
    ProviderAnswer::Unavailable("socket missing".to_owned())
}

/// The `Prepare` the start of `with_layer` sends for its source, as logged.
const PREPARE: &str = "provider prepare tier j1#0 k1 ReadOnly";

/// The `Mount` of that source, as logged.
const MOUNT: &str = "provider mount tier j1#0";

/// A rig whose provider keeps the sources it takes, has taken the source of `j1` (a start of
/// `with_layer`, admitted at 0 on the test's clock) with its first `Mount` still out, and
/// then restarts: it has lost the source, and answers the directory `t1` of the root `ro`
/// once the source is taken again. The first step is taken: the `Mount` is answered "the
/// source is not prepared here", and the message of the `provider-backoff` line that step
/// gave is returned too.
fn restarted(name: &str) -> (Harness, FakeProviders, String) {
    let (mut h, providers) = rig(name, FakeProviders::keep_sources);
    providers.then("tier", &mount("j1", 0), ProviderAnswer::Pending);
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    h.apply(vec![job_start(with_layer("j1"))]);
    assert_eq!(asked(&h), [PREPARE, MOUNT]);
    providers.restart();
    let events = h.life.step();
    assert_eq!(names(&events), ["finding j1 provider-backoff"]);
    let line = events
        .into_iter()
        .find_map(|event| match event {
            Event::Finding(found) => Some(found.message),
            _ => None,
        })
        .expect("the backoff line");
    (h, providers, line)
}

/// Asserts that a start whose provider restarted between the source's `Prepare` and its
/// `Mount` (the `Mount` still out, then answered "the source is not prepared here") says so
/// in its `provider-backoff` line, sends the source's `Prepare` again once the first wait is
/// over (1 s at most) and the `Mount` right after it, and runs on the directory the provider
/// then answers, 1 s after its admission. Catches a source never asked from the start again
/// after its provider lost it, which waits out `--mount-provider-wait` and fails
/// `input-unavailable` asking only `Mount`.
#[test]
fn a_provider_that_restarted_between_prepare_and_mount_is_asked_to_prepare_again() {
    let (mut h, _providers, line) = restarted("lost-mid");
    assert!(
        line.starts_with(
            "the provider tier did not serve the Mount of source 0 (1 in a row): the source is \
             not prepared here; asked again in "
        ),
        "{line}"
    );
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT],
        "nothing before the wait is over"
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    h.clock.advance(1_000);
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

/// Asserts that a provider still down after its restart (two `Prepare`s answered
/// unavailable, "socket missing") is sent `Prepare` again after each wait and no `Mount`
/// until it takes the source again, the waits growing (two `Prepare`s in the first 3 s, the
/// third later), and that the start then runs well within its wait. Catches a `Mount` sent to
/// a provider that has not taken the source back, which can only answer "not prepared here",
/// and a source given up after a `Prepare` sent again failed.
#[test]
fn a_provider_still_down_after_its_restart_is_asked_to_prepare_until_it_takes_the_source() {
    let (mut h, providers, _) = restarted("lost-down");
    providers.then("tier", &prepare_j1(), down());
    providers.then("tier", &prepare_j1(), down());
    tick_for(&mut h, 3_000);
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, PREPARE, PREPARE],
        "no Mount while the Prepare is not taken"
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    tick_for(&mut h, 5_000);
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, PREPARE, PREPARE, PREPARE, MOUNT]
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
}

/// Asserts that a provider that restarted and never takes the source again fails the start
/// `input-unavailable` once `--mount-provider-wait` has passed since its admission and not a
/// moment before, naming the last reason it gave (its `Prepare`'s), and that the source is
/// released. Catches a start held for ever by a provider that came back without the source,
/// and a failure that names the old `Mount` answer and not why the source is not taken.
#[test]
fn a_provider_that_never_takes_the_source_back_fails_the_start_at_the_wait() {
    let (mut h, providers, _) = restarted("lost-never");
    providers.always("tier", &prepare_j1(), down());
    tick_for(&mut h, WAIT_MS - TICK_MS);
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(
        providers.sent("mount").len(),
        1,
        "no Mount after the restart"
    );
    tick_for(&mut h, TICK_MS);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_UNAVAILABLE));
    assert_eq!(
        finding(&h, "j1", "start-failed"),
        "the provider tier did not mount source 0 within 30 s: socket missing"
    );
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
}

/// Asserts that a provider that restarted and then refuses the source's `Prepare` fails the
/// start `provider-refused` at the step that sends it, with its reason, and releases the
/// source. Catches a refusal of the `Prepare` sent again that is waited out as if the
/// provider were down.
#[test]
fn a_prepare_refused_after_the_restart_fails_the_start_provider_refused() {
    let (mut h, providers, _) = restarted("lost-refused");
    let refused = ProviderAnswer::Refused("no such tree".to_owned());
    providers.always("tier", &prepare_j1(), refused);
    h.clock.advance(1_000);
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::PROVIDER_REFUSED));
    assert_eq!(
        finding(&h, "j1", "start-failed"),
        "the provider tier refused source 0: no such tree"
    );
    steps(&mut h);
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
    assert!(h.fake.created().is_empty());
}

/// Asserts that a step whose `Mount` is still out sends nothing, that a `Mount` answered with
/// no directory is asked again after its wait with no `Prepare`, and that a `Mount` answered
/// unavailable by a provider that did not restart ("not ready": it still holds the source)
/// has the next ask send the source's `Prepare` too, which it takes as before, and that the
/// start runs on the `Mount` after it. Catches a `Prepare` sent again while the `Mount` is
/// out, or for an answer that does not say the source may be lost.
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
    h.clock.advance(1_000);
    h.life.step();
    assert_eq!(
        asked(&h),
        [PREPARE, MOUNT, MOUNT],
        "no directory: the Mount alone again"
    );
    h.clock.advance(2_000);
    assert_eq!(steps(&mut h), ["started j1"]);
    assert_eq!(asked(&h), [PREPARE, MOUNT, MOUNT, PREPARE, MOUNT]);
}
