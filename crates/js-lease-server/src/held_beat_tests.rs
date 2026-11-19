use std::time::Duration;

use js_types::HostId;

use super::{MAX_HOLD, STARTING_HOLD, Wakes, Watch, hold_bound, starting_hold};

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
