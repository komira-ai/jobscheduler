//! How often a start asks a mount provider that cannot serve it now
//! (`docs/design/mount-providers.md`, section 4): each source waits longer after each failure
//! in a row (from about 1 s up to 16 s, from the moment the failed call was sent), a provider
//! that fails `Prepare`s is sent one at a time, a call out is never sent twice, and each step
//! in which a start's calls failed gives one `provider-backoff` line for it. The steps here
//! come every 100 ms of the test's clock, ten times as often as an idle worker's, as on a host
//! whose worker is woken by other work.

use js_agent::lifecycle::{Event, ProviderAnswer};
use js_agent_wire::{HeldState, MountMode, Outcome, refusal};

use super::Harness;
use super::fake_providers::{
    WAIT_MS, mount, prepare, release, rig, run_one, tree, with_layer, with_layer_and_mount,
};
use super::per_job::{job_start, listing};

/// What tierstore answers a `Prepare` when its node holds as many placements as it may.
const FULL: &str = "the tree was not made: too many placements: 1024 are alive";

/// The step period of these tests, in milliseconds.
const TICK_MS: u64 = 100;

fn full() -> ProviderAnswer {
    ProviderAnswer::Unavailable(FULL.to_owned())
}

/// The `Prepare` of the one layer (`k1`) of `job_id`.
fn prepare_of(job_id: &str) -> js_agent::lifecycle::ProviderCall {
    prepare(job_id, 0, "k1", MountMode::ReadOnly)
}

/// Steps `h` every [`TICK_MS`] for `ms` milliseconds, and returns every event of those steps.
fn tick_for(h: &mut Harness, ms: u64) -> Vec<Event> {
    let mut events = Vec::new();
    for _ in 0..ms / TICK_MS {
        h.clock.advance(TICK_MS);
        events.extend(h.life.step());
    }
    events
}

/// The `provider-backoff` lines among `events`, as (run, message).
fn backoffs(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Finding(found) if found.code == "provider-backoff" => Some((
                found.run_id.clone().unwrap_or_default(),
                found.message.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// The message of the finding of `code` of `job_id`, which must be there.
fn finding(h: &Harness, job_id: &str, code: &str) -> String {
    h.life
        .findings()
        .into_iter()
        .find(|found| found.run_id.as_deref() == Some(job_id) && found.code == code)
        .expect("the finding")
        .message
}

/// Asserts that a provider that answers `UNAVAILABLE` to a start's `Prepare` for 10 s, with
/// the worker stepping ten times a second, is sent at most five `Prepare`s in that time (the
/// first at the admission, then after waits of at least 0.5, 1, 2 and 4 s), and that once it
/// serves them the job starts before its wait is over, with no `Prepare` more than the one
/// that succeeded. Catches a start that asks again at every step (a hundred `Prepare`s in
/// those 10 s) and one whose waits do not grow.
#[test]
fn a_provider_unavailable_for_ten_seconds_is_asked_at_most_five_times_and_the_job_starts() {
    let (mut h, providers) = rig("backoff-recovers", |_| {});
    providers.always("tier", &prepare_of("j1"), full());
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    h.apply(vec![job_start(with_layer("j1"))]);
    tick_for(&mut h, 10_000);
    let asked = providers.sent("prepare").len();
    assert!((3..=5).contains(&asked), "{asked} Prepares in 10 s");
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);

    providers.always("tier", &prepare_of("j1"), ProviderAnswer::Done(None));
    let mut waited = 0;
    while listing(&h, "j1").state != HeldState::Running && waited < WAIT_MS - 10_000 {
        tick_for(&mut h, TICK_MS);
        waited += TICK_MS;
    }
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    assert_eq!(providers.sent("prepare").len(), asked + 1);
    assert_eq!(providers.sent("mount"), ["provider mount tier j1#0"]);
}

/// Asserts that a provider that never serves a start's `Prepare` fails the start once,
/// `input-unavailable` at the end of its wait (30 s), naming the provider's own reason, after
/// at most seven `Prepare`s, and that its source is released once. Catches a start held for
/// ever, a failure that loses the provider's reason (for the text of the agent's wait), and a
/// start that asks at every step until its wait is over.
#[test]
fn a_provider_that_never_recovers_fails_the_start_once_with_its_reason() {
    let (mut h, providers) = rig("backoff-never", |_| {});
    providers.always("tier", &prepare_of("j1"), full());
    h.apply(vec![job_start(with_layer("j1"))]);
    let events = tick_for(&mut h, WAIT_MS + 1_000);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_UNAVAILABLE));
    assert_eq!(
        finding(&h, "j1", "start-failed"),
        format!("the provider tier did not mount source 0 within 30 s: {FULL}")
    );
    let failed = events
        .iter()
        .filter(|event| matches!(event, Event::Finding(found) if found.code == "start-failed"));
    assert_eq!(failed.count(), 1, "the start fails once");
    let asked = providers.sent("prepare").len();
    assert!((5..=7).contains(&asked), "{asked} Prepares in 30 s");
    assert!(providers.sent("mount").is_empty());
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
}

/// Asserts that twenty starts that wait on one provider that turns every `Prepare` away send
/// it at most four `Prepare`s together in 5 s, not four each: once it failed one, the
/// `Prepare`s of the starts not yet sent go one at a time, a new start's included; and that
/// once it serves them every start runs before its wait is over. Catches a provider in
/// trouble asked once for each waiting start at each try, which is how a host that starts
/// fewer than a hundred jobs a minute asks a provider a thousand times a second.
#[test]
fn many_starts_waiting_on_a_failing_provider_send_it_one_prepare_at_a_time() {
    let jobs: Vec<String> = (0..20).map(|n| format!("j{n:02}")).collect();
    let (mut h, providers) = rig("backoff-many", |_| {});
    for job in &jobs {
        providers.always("tier", &prepare_of(job), full());
        providers.mounts_at("tier", job, 0, &tree(&h, "ro", job));
    }
    let starts = jobs.iter().map(|job| job_start(with_layer(job))).collect();
    h.apply(starts);
    assert_eq!(
        providers.sent("prepare").len(),
        1,
        "the others wait their turn"
    );
    tick_for(&mut h, 5_000);
    let asked = providers.sent("prepare").len();
    assert!((3..=4).contains(&asked), "{asked} Prepares in 5 s");

    for job in &jobs {
        providers.always("tier", &prepare_of(job), ProviderAnswer::Done(None));
    }
    tick_for(&mut h, WAIT_MS - 5_000 - TICK_MS);
    for job in &jobs {
        assert_eq!(listing(&h, job).state, HeldState::Running, "{job}");
    }
}

/// Asserts that a second start's `Prepare` is not sent while the first start's failed
/// `Prepare` holds the provider (its wait is at least 0.5 s), that it is sent once that wait
/// is over, and that once a `Prepare` succeeds both starts go on to their `Mount`. Catches a
/// new start that is sent to a provider in trouble at once, and a provider kept shut after it
/// serves again.
#[test]
fn a_new_start_waits_its_turn_behind_a_failed_prepare() {
    let (mut h, providers) = rig("backoff-turn", |_| {});
    providers.then("tier", &prepare_of("j1"), full());
    h.apply(vec![job_start(with_layer("j1"))]);
    h.apply(vec![job_start(with_layer("j2"))]);
    tick_for(&mut h, 400);
    assert_eq!(
        providers.sent("prepare"),
        ["provider prepare tier j1#0 k1 ReadOnly"],
        "j2 waits its turn"
    );
    tick_for(&mut h, 1_100);
    let sent = providers.sent("prepare");
    assert!(
        sent.contains(&"provider prepare tier j2#0 k1 ReadOnly".to_owned()),
        "{sent:?}"
    );
    tick_for(&mut h, 1_000);
    let sent = providers.sent("prepare");
    assert_eq!(sent.len(), 3, "{sent:?}");
    let mounts = providers.sent("mount");
    for job in ["j1", "j2"] {
        let line = format!("provider mount tier {job}#0");
        assert!(mounts.contains(&line), "{job}: {mounts:?}");
    }
}

/// Asserts that a `Prepare` that is out is never sent again while it is out, however long
/// it stays out, and that when it then fails after more than its wait it is sent again at the
/// next step: the wait is measured from the send. Catches a pace that sends a call that is
/// still out, and one that waits again after a call the provider held longer than the wait.
#[test]
fn a_call_out_is_sent_once_and_a_late_failure_is_asked_again_at_once() {
    let (mut h, providers) = rig("backoff-out", |_| {});
    let call = prepare_of("j1");
    providers.then("tier", &call, ProviderAnswer::Pending);
    h.apply(vec![job_start(with_layer("j1"))]);
    for _ in 0..25 {
        providers.then("tier", &call, ProviderAnswer::Pending);
    }
    tick_for(&mut h, 2_500);
    assert_eq!(providers.sent("prepare").len(), 1, "out: not sent again");
    providers.then("tier", &call, full());
    h.clock.advance(20_000);
    h.life.step();
    assert_eq!(
        providers.sent("prepare").len(),
        1,
        "the failure is handed over"
    );
    h.life.step();
    assert_eq!(
        providers.sent("prepare").len(),
        2,
        "held 22 s, longer than any wait: asked again at once"
    );
}

/// Asserts that a `Mount` the provider fails at once is paced as a `Prepare` is (at most
/// five in 10 s of steps ten times a second), and that one answered with no directory is too.
/// Catches a `Mount` loop left at the step rate after the `Prepare` loop is paced.
#[test]
fn a_mount_that_fails_at_once_is_paced() {
    let answers = [full(), ProviderAnswer::Done(None)];
    for (n, answer) in answers.into_iter().enumerate() {
        let (mut h, providers) = rig(&format!("backoff-mount-{n}"), |_| {});
        providers.always("tier", &mount("j1", 0), answer);
        h.apply(vec![job_start(with_layer("j1"))]);
        tick_for(&mut h, 10_000);
        let asked = providers.sent("mount").len();
        assert!((3..=5).contains(&asked), "{n}: {asked} Mounts in 10 s");
        assert_eq!(providers.sent("prepare").len(), 1, "{n}");
    }
}

/// Asserts that a release the provider fails is paced (at most five in 10 s of steps ten
/// times a second, each failure one `provider-backoff` line of the run), and that the source
/// is released, and the record removed, once the provider answers. Catches a release asked
/// at every step for its minute.
#[test]
fn a_failed_release_is_paced_and_ends_when_answered() {
    let (mut h, providers) = rig("backoff-release", |_| {});
    run_one(&mut h, &providers, "j1");
    providers.always("tier", &release("j1", 0), full());
    h.fake.exit("j1", 0);
    let events = tick_for(&mut h, 10_000);
    let asked = providers.sent("release").len();
    assert!((3..=5).contains(&asked), "{asked} Releases in 10 s");
    let lines = backoffs(&events);
    assert_eq!(lines.len(), asked, "one line for each failure: {lines:?}");
    assert!(lines.iter().all(|(run, _)| run == "j1"));
    providers.always("tier", &release("j1", 0), ProviderAnswer::Done(None));
    tick_for(&mut h, 17_000);
    assert_eq!(providers.sent("release").len(), asked + 1);
    assert!(!h.dir.path("state/providers/j1.json").exists());
}

/// Asserts that a step in which two sources of one start fail gives one `provider-backoff`
/// line for the start, naming the first source, the provider's reason, the wait and that one
/// more source waits, with the agent's counts; that the steps while they wait give none; and
/// that the counts go up with the next failures. Catches a line for each call, a line at each
/// step while a source waits, and counts that do not count.
#[test]
fn each_backoff_step_of_a_start_is_one_line_with_the_agents_counts() {
    let (mut h, providers) = rig("backoff-lines", |_| {});
    providers.always("tier", &mount("j1", 0), full());
    providers.always("tier", &mount("j1", 1), full());
    let events = h.apply(vec![job_start(with_layer_and_mount("j1"))]);
    let lines = backoffs(&events);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (run, first) = &lines[0];
    assert_eq!(run, "j1");
    let wait = first
        .split("asked again in ")
        .nth(1)
        .and_then(|rest| rest.split(" ms").next())
        .and_then(|ms| ms.parse::<u64>().ok())
        .expect("a wait in milliseconds");
    assert!((500..=1_000).contains(&wait), "{first}");
    assert_eq!(
        *first,
        format!(
            "the provider tier did not serve the Mount of source 0 (1 in a row): {FULL}; asked \
             again in {wait} ms; 1 more source waits too; this agent: 0 Prepares not served, 0 \
             calls sent again"
        )
    );
    h.clock.advance(499);
    assert!(
        backoffs(&h.life.step()).is_empty(),
        "both sources wait at least 500 ms: no line"
    );
    let events = tick_for(&mut h, 2_000);
    let lines = backoffs(&events);
    assert!(!lines.is_empty());
    let (_, second) = &lines[0];
    assert!(
        second.starts_with("the provider tier did not serve the Mount of source "),
        "{second}"
    );
    assert!(second.contains(" (2 in a row): "), "{second}");
    assert!(
        second.ends_with("this agent: 0 Prepares not served, 1 calls sent again")
            || second.ends_with("this agent: 0 Prepares not served, 2 calls sent again"),
        "{second}"
    );
}

/// `message` with the number of milliseconds after `asked again in ` written `_`, and that
/// number.
fn masked(message: &str) -> (String, u64) {
    let (head, rest) = message.split_once("asked again in ").expect("a wait");
    let (ms, tail) = rest.split_once(" ms").expect("in milliseconds");
    let wait = ms.parse().expect("a number");
    (format!("{head}asked again in _ ms{tail}"), wait)
}

/// Asserts that the counts of a backoff line count the `Prepare`s the providers did not serve
/// and the calls sent again: a start whose `Prepare` fails twice says `1 Prepares not served,
/// 0 calls sent again` at the first failure and `2 Prepares not served, 1 calls sent again`
/// at the second, one line each, and the second line's wait is of the second step (1 to 2 s).
/// Catches counts that do not count, a retry counted at the first send, and a wait that did
/// not grow.
#[test]
fn the_counts_count_the_prepares_not_served_and_the_calls_sent_again() {
    let (mut h, providers) = rig("backoff-counts", |_| {});
    providers.then("tier", &prepare_of("j1"), full());
    providers.then("tier", &prepare_of("j1"), full());
    let events = h.apply(vec![job_start(with_layer("j1"))]);
    let lines = backoffs(&events);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (first, wait) = masked(&lines[0].1);
    assert_eq!(
        first,
        format!(
            "the provider tier did not serve the Prepare of source 0 (1 in a row): {FULL}; \
             asked again in _ ms; this agent: 1 Prepares not served, 0 calls sent again"
        )
    );
    assert!((500..=1_000).contains(&wait), "{wait}");
    let lines = backoffs(&tick_for(&mut h, 1_000));
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (second, wait) = masked(&lines[0].1);
    assert_eq!(
        second,
        format!(
            "the provider tier did not serve the Prepare of source 0 (2 in a row): {FULL}; \
             asked again in _ ms; this agent: 2 Prepares not served, 1 calls sent again"
        )
    );
    assert!((1_000..=2_000).contains(&wait), "{wait}");
}
