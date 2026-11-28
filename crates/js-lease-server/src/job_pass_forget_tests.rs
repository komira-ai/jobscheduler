//! The job pass forgets the `node/` rows of hosts that fell silent, so a pool's cap counts only
//! rows not forgotten (design `multi-instance.md`, G5): the rows come from hellos through the
//! route, time moves on the injected clock, and nothing sleeps.

use std::time::Duration;

use js_types::HostId;

use crate::agent_api::job_rig::{FakeAgent, Rig};
use crate::agent_api::pool_tests::{hello, refusal, two_servers};
use crate::api::rig::{Lines, process_sink};

const SECOND: Duration = Duration::from_secs(1);

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// How long a host must be silent for its row to be forgotten, and a second more.
fn past_forgetting(rig: &Rig) -> Duration {
    rig.agents.registry.forget_after() + SECOND
}

/// The hosts that have a row, in host order.
fn rowed(rig: &Rig) -> Vec<String> {
    let (_, rows) = rig.jobs.nodes().unwrap();
    rows.into_iter()
        .map(|row| row.record.host.to_string())
        .collect()
}

/// Asserts that hosts that said hello through the route and then fell silent past the grace
/// plus the lost-host retention have their rows forgotten by the tick (with a line saying
/// which), the server's note of their epochs dropped, and that a pool full over their rows
/// then takes a new host on another server. It catches rows that live for ever, so that a
/// pool's cap counts hosts long gone and refuses a new host.
#[tokio::test]
async fn silent_hosts_rows_are_forgotten_and_the_pool_has_room_again() {
    let lines = Lines::default();
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (mut first, second) = two_servers("forget_room");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.join(name, false).await;
    }
    let full = hello(&second, "host-5", None).await;
    assert_eq!(refusal(full), (403, "pool-full"));

    first.advance(past_forgetting(&first));
    first.tick();

    assert_eq!(rowed(&first), Vec::<String>::new());
    let text = lines.take();
    assert!(
        text.contains("the rows of silent hosts were forgotten"),
        "{text}"
    );
    assert!(text.contains("host-1") & text.contains("host-4"), "{text}");
    let admitted = hello(&second, "host-5", None).await.unwrap();
    assert_eq!(admitted.epoch, 1);
}

/// Asserts that a silent host that holds a live allocation keeps its row while one that holds
/// nothing loses it. It catches a forget that deletes the row under a job the host still
/// holds, which would lose the host from every server's placement and its job from the
/// pool's count.
#[tokio::test]
async fn a_silent_host_holding_a_live_allocation_keeps_its_row() {
    let (mut first, _second) = two_servers("forget_holds");
    first.join("host-1", false).await;
    first.join("host-2", false).await;
    first.place("job-a", "host-1");

    first.advance(past_forgetting(&first));
    first.tick();

    assert_eq!(rowed(&first), ["host-1"]);
}

/// Asserts that a host heard within the time keeps its row while a host silent past it loses
/// its own: the host beats a minute before the others' time is out, and its row is refreshed
/// by that beat. It catches a forget that goes by the row's creation and not its last hearing.
#[tokio::test]
async fn a_host_heard_within_the_time_keeps_its_row() {
    let (mut first, _second) = two_servers("forget_heard");
    first.join("host-1", false).await;
    first.join("host-2", false).await;
    let limit = first.agents.registry.forget_after();
    first.advance(limit - 30 * SECOND);
    first.beat(&FakeAgent::new("host-1")).await.unwrap();
    first.advance(31 * SECOND);

    first.tick();

    assert_eq!(rowed(&first), ["host-1"]);
}

/// Asserts that a forget the store fails is logged as a warning that the next period tries
/// again, deletes nothing, and that the next period forgets the row. It catches a failure that
/// is swallowed, or one that stops the rows from ever being forgotten.
#[tokio::test]
async fn a_forget_that_fails_is_logged_and_the_next_period_forgets_it() {
    let lines = Lines::default();
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (mut first, _second) = two_servers("forget_fails");
    first.join("host-1", false).await;
    first.advance(past_forgetting(&first));
    let jobs = first.jobs.clone();

    first.store.fail_read_after(0);
    first.agents.forget_silent_rows(&jobs);
    let text = lines.take();
    assert!(
        text.contains("silent hosts' rows could not be forgotten; the next period tries again"),
        "{text}"
    );
    assert_eq!(rowed(&first), ["host-1"]);

    first.tick();
    assert_eq!(rowed(&first), Vec::<String>::new());
}

/// Asserts that a server whose pass finds the period claimed by another server forgets no row,
/// even with a host its own registry no longer holds (the pass no longer runs unclaimed for
/// it), and that its pass in the next period, which it claims, does. It catches every server
/// forgetting in every pass, instead of one in each period.
#[tokio::test]
async fn a_pass_in_a_period_another_server_claimed_forgets_no_row() {
    let (mut first, _second) = two_servers("forget_unclaimed");
    first.agents.set_duty_period(SECOND);
    first.join("host-1", false).await;
    first.advance(past_forgetting(&first));
    first.agents.registry.advance();
    assert!(first.agents.registry.own_host(&host("host-1")).is_none());

    assert!(
        first.jobs.claim_duty("job-pass", SECOND).unwrap(),
        "another server claims the period"
    );
    assert_eq!(first.agents.job_pass(), None);
    assert_eq!(rowed(&first), ["host-1"]);

    first.advance(SECOND);
    first.agents.job_pass();
    assert_eq!(rowed(&first), Vec::<String>::new());
}
