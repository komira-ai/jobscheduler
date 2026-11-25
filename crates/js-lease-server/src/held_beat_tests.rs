use std::time::Duration;

use js_types::HostId;

use super::{
    MAX_HOLD, STARTING_HOLD, STARTING_WINDOW_MS, Starts, Wakes, Watch, hold_bound, starting_hold,
};

/// Whether `watch` holds a wake already: its wait ends without yielding.
async fn woken_now(watch: &Watch) -> bool {
    tokio::select! {
        biased;
        () = watch.woken() => true,
        () = std::future::ready(()) => false,
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Asserts that a reply is held for the heartbeat interval, and never longer than five
/// seconds, whatever the interval. It catches a hold that outlasts the agent's request limit
/// (the agent then counts the beat as failed) and one that stretches the gap between beats past
/// the interval, which the server's silence counting assumes.
#[test]
fn a_reply_is_held_for_the_interval_up_to_five_seconds() {
    assert_eq!(hold_bound(Duration::from_secs(1)), Duration::from_secs(1));
    assert_eq!(hold_bound(Duration::from_secs(30)), Duration::from_secs(5));
    assert_eq!(MAX_HOLD, Duration::from_secs(5));
}

/// Asserts that a reply to a beat that lists a job `starting` is held for 200 ms, and never
/// longer than the normal hold of the same interval. It catches a short hold that outlasts a
/// short interval, and one that is not short next to the interval agents beat at.
#[test]
fn a_reply_to_a_starting_beat_is_held_for_the_short_bound_or_the_normal_one_if_shorter() {
    assert_eq!(STARTING_HOLD, Duration::from_millis(200));
    assert_eq!(starting_hold(Duration::from_secs(1)), STARTING_HOLD);
    assert_eq!(starting_hold(Duration::from_secs(30)), STARTING_HOLD);
    let short = Duration::from_millis(50);
    assert_eq!(starting_hold(short), short);
}

/// Asserts that a wake that comes before the hold begins is kept, so the hold that follows
/// ends at once, and that a wake for another host, or for a host no heartbeat watches, wakes
/// nothing. It catches a placement committed between the store's read and the hold that would
/// be lost, leaving the start to the next heartbeat.
#[tokio::test]
async fn a_wake_before_the_hold_is_kept_and_another_hosts_is_not() {
    let wakes = Wakes::default();
    let watch = wakes.watch(&host("h1"));
    wakes.wake(&host("h2"));
    wakes.wake(&host("h1"));
    watch.woken().await;
    assert!(!woken_now(&watch).await, "one wake ends one hold");
}

/// Asserts that a second watch of the same host takes the wakes over, that dropping the first
/// leaves the second watched, and that dropping the last leaves the host unwatched (a wake then
/// is not kept for a later heartbeat). It catches an older heartbeat's end that unwatches the
/// newer one, and wakes kept for heartbeats that are gone.
#[tokio::test]
async fn the_newest_watch_of_a_host_is_woken_and_a_dropped_one_is_forgotten() {
    let wakes = Wakes::default();
    let first = wakes.watch(&host("h1"));
    let second = wakes.watch(&host("h1"));
    drop(first);
    wakes.wake(&host("h1"));
    second.woken().await;
    drop(second);
    wakes.wake(&host("h1"));
    let third = wakes.watch(&host("h1"));
    assert!(!woken_now(&third).await, "a wake with no watch is not kept");
}

/// Asserts that each wait of a held reply is counted, and seen by every receiver, also one
/// taken after the waits. It catches a count that a test waiting for a held reply never sees.
#[test]
fn each_wait_of_a_held_reply_is_counted() {
    let wakes = Wakes::default();
    let early = wakes.waits();
    wakes.waiting();
    wakes.waiting();
    assert_eq!((*early.borrow(), *wakes.waits().borrow()), (2, 2));
}

/// Asserts what [`Starts::young`] says of a host's listing over time: a job first seen
/// `starting` is young until the window has passed since then, and not from that moment on; a
/// second job newly listed is young while the first is not; a listing without `starting` jobs is
/// not young; and a job left out of a listing is forgotten, so listed again it is young again.
/// It catches a window counted from each beat rather than from the job's first sight (a start
/// that waits for minutes then keeps its host beating five times a second), one off by one at
/// its end, and a job that is never forgotten.
#[test]
fn a_start_takes_the_short_hold_for_the_window_from_when_a_hold_first_saw_it() {
    let starts = Starts::default();
    let (a, b) = (host("host-a"), host("host-b"));
    assert!(!starts.young(&a, &[], 100));
    assert!(starts.young(&a, &["job-1"], 100));
    assert!(starts.young(&a, &["job-1"], 100 + STARTING_WINDOW_MS - 1));
    assert!(!starts.young(&a, &["job-1"], 100 + STARTING_WINDOW_MS));
    // Another host's starts are its own.
    assert!(starts.young(&b, &["job-1"], 100 + STARTING_WINDOW_MS));
    let later = 100 + STARTING_WINDOW_MS + 500;
    assert!(starts.young(&a, &["job-1", "job-2"], later));
    assert!(!starts.young(&a, &["job-1", "job-2"], later + STARTING_WINDOW_MS));
    // job-1 is left out, then listed again: it counts from then.
    let again = later + 2 * STARTING_WINDOW_MS;
    assert!(!starts.young(&a, &["job-2"], again));
    assert!(starts.young(&a, &["job-1", "job-2"], again));
    // A listing with none forgets the host.
    assert!(!starts.young(&a, &[], again));
    assert!(starts.young(&a, &["job-2"], again + 1));
}

/// Asserts how a host's beats that list a young start are paced: the first of a host is
/// answered at once; after an answer at `t`, one at `t` waits the whole spacing, one 300 ms later
/// the rest of it, and one a whole spacing later or more nothing; another host is paced on its
/// own; a later answer restarts the spacing; the record keeps only the hosts whose spacing still
/// lasts. It catches pacing that holds the first start of an idle host (its `running` then waits
/// for the hold), pacing that lets a burst of starts answer every beat at once, one host's
/// answers spacing another's, and a record that grows with every host that ever started a job.
#[test]
fn beats_with_a_start_are_answered_at_most_once_per_spacing_per_host() {
    let starts = Starts::default();
    let spacing = Duration::from_millis(1_000);
    let (a, b) = (host("host-a"), host("host-b"));
    assert_eq!(starts.paced(&a, 5_000, spacing), Duration::ZERO);
    starts.answered(&a, 5_000, spacing);
    assert_eq!(starts.paced(&a, 5_000, spacing), spacing);
    assert_eq!(starts.paced(&a, 5_300, spacing), Duration::from_millis(700));
    assert_eq!(starts.paced(&a, 6_000, spacing), Duration::ZERO);
    assert_eq!(starts.paced(&a, 9_000, spacing), Duration::ZERO);
    assert_eq!(starts.paced(&b, 5_000, spacing), Duration::ZERO);
    starts.answered(&a, 5_600, spacing);
    assert_eq!(starts.paced(&a, 6_000, spacing), Duration::from_millis(600));
    assert_eq!(
        starts.paced(&a, 0, Duration::MAX),
        Duration::from_millis(u64::MAX)
    );
    assert_eq!(starts.paced_hosts(), 1);
    starts.answered(&b, 6_599, spacing);
    assert_eq!(starts.paced_hosts(), 2);
    starts.answered(&b, 6_600, spacing);
    assert_eq!(starts.paced_hosts(), 1);
    assert_eq!(starts.paced(&a, 6_600, spacing), Duration::ZERO);
    starts.answered(&a, 0, Duration::MAX);
    assert_eq!(starts.paced_hosts(), 2);
}
