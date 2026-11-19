//! The cordon of a host: it keeps new jobs off a host that is live, renews and lifts,
//! lapses by itself, is kept in the bindings a caller persists, and is not forgotten with
//! a silent host.

mod support;

use std::time::Duration;

use js_registry::{Binding, MIN_LOST_RETENTION_MS, NotHeld};
use std::collections::BTreeMap;

use js_types::cordon::{Asker, Cordon};
use js_types::{CordonHolder, Timestamp};
use support::*;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Lets `n` seconds pass in steps of five, host `a` (epoch 1) heartbeating at each, so
/// that it stays live.
fn live_for(fx: &Fixture, n: u64) {
    for _ in 0..n / 5 {
        fx.clock.advance(secs(5));
        fx.beat("a", 1);
    }
}

fn until(fx: &Fixture, name: &str) -> Option<Timestamp> {
    fx.registry.own_host(&host(name)).unwrap().cordon_until
}

/// Asserts what a cordon shows and how it ends: while it is in force the host's view carries
/// the time it lapses (the job engine reads it there), just before it lapses it still does,
/// and at the time it lapses the view carries none, with no further call. It catches a cordon
/// that is not read when the hosts are listed, and one that never ends.
#[test]
fn a_cordoned_host_shows_its_cordon_until_it_lapses() {
    let fx = fixture(2);
    fx.join("a", "t");
    let placed = fx
        .registry
        .cordon(&host("a"), secs(60), &Asker::Unnamed)
        .unwrap();
    assert!(placed.placed);
    let lapses = Timestamp::from_millis(1_060_000);
    assert_eq!(placed.host.cordon_until, Some(lapses));
    assert_eq!(placed.host.host.cordon_until, Some(lapses));
    assert_eq!(fx.registry.hosts()[0].cordon_until, Some(lapses));
    live_for(&fx, 55);
    assert_eq!(fx.registry.hosts()[0].cordon_until, Some(lapses));
    live_for(&fx, 10);
    assert_eq!(until(&fx, "a"), None);
    assert_eq!(fx.registry.hosts()[0].cordon_until, None);
}

/// Asserts that a cordon cuts nothing and survives the end of the work: a run the host lists
/// as held stays listed (the host is releasing) when the host is cordoned, and once the host
/// lists none the cordon is still in force. It catches a cordon that touches the held list.
#[test]
fn a_cordon_leaves_the_runs_it_finds_and_outlives_them() {
    let fx = fixture(2);
    fx.join("a", "t");
    let mut listing = fx.heartbeat("a", 1);
    listing.held = vec![js_agent_wire::HeldRun {
        run_id: "r1".into(),
        state: js_agent_wire::HeldState::Running,
    }];
    fx.registry.heartbeat(&listing, &cred("t")).unwrap();
    let cordoned = fx
        .registry
        .cordon(&host("a"), secs(600), &Asker::Unnamed)
        .unwrap();
    assert!(cordoned.host.host.releasing);
    fx.beat("a", 1);
    let own = fx.registry.own_host(&host("a")).unwrap();
    assert!(!own.host.releasing);
    assert!(own.cordon_until.is_some());
}

/// Asserts the renewal: a repeat reports that it did not place the cordon, a longer one
/// moves the expiry out from the time of the repeat, a shorter one leaves it where it was,
/// and `uncordon` reports whether it lifted one and leaves the host unmarked, again and
/// again. It catches a renew that shortens, one that reports itself a placement, and an
/// uncordon that fails when there is nothing to lift.
#[test]
fn a_repeat_extends_and_never_shortens_and_an_uncordon_repeats() {
    let fx = fixture(2);
    fx.join("a", "t");
    let first = fx
        .registry
        .cordon(&host("a"), secs(100), &Asker::Unnamed)
        .unwrap();
    assert!(first.placed);
    live_for(&fx, 10);
    let shorter = fx
        .registry
        .cordon(&host("a"), secs(20), &Asker::Unnamed)
        .unwrap();
    assert!(!shorter.placed);
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_100_000)));
    fx.registry
        .cordon(&host("a"), secs(200), &Asker::Unnamed)
        .unwrap();
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_210_000)));

    assert!(fx.registry.uncordon(&host("a"), None).unwrap().lifted);
    assert!(!fx.registry.uncordon(&host("a"), None).unwrap().lifted);
    assert_eq!(until(&fx, "a"), None);
    let again = fx
        .registry
        .cordon(&host("a"), secs(5), &Asker::Unnamed)
        .unwrap();
    assert!(again.placed);
}

/// Asserts that a host the registry does not hold is `NotHeld` to all three calls, and
/// that the read has what the hello said: the runtime and the agent version. It catches a
/// call that makes up a host.
#[test]
fn an_unknown_host_is_not_held_and_the_read_has_the_hello() {
    let fx = fixture(2);
    assert_eq!(
        fx.registry.cordon(&host("a"), secs(5), &Asker::Unnamed),
        Err(NotHeld)
    );
    assert_eq!(fx.registry.uncordon(&host("a"), None), Err(NotHeld));
    assert_eq!(fx.registry.own_host(&host("a")), None);

    let mut said = hello("a");
    said.capabilities
        .insert("runtime".into(), "container".into());
    fx.hello(&said, &cred("t")).unwrap();
    let read = fx.registry.own_host(&host("a")).unwrap();
    assert_eq!(read.runtime.as_deref(), Some("container"));
    assert_eq!(read.host.agent_version.as_deref(), Some("0.1.0"));
    assert_eq!(read.now, Timestamp::from_millis(1_000_000));
    fx.join("b", "t");
    assert_eq!(fx.registry.own_host(&host("b")).unwrap().runtime, None);
}

/// Asserts that a cordon is part of what is persisted: the bindings carry it while it is in
/// force and not after it lapsed or was lifted, a registry started over them puts a
/// cordoned host back awaiting with the same expiry, a binding with a lapsed cordon puts
/// nothing back, and the host keeps the cordon through its hello. It catches a restart that
/// lifts a cordon and one that puts back a cordon that is over.
#[test]
fn a_cordon_survives_what_is_persisted() {
    let config = config(3)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let before = fixture_of(config.clone());
    for name in ["a", "b", "c"] {
        before.join(name, "t");
    }
    before
        .registry
        .cordon(&host("a"), secs(100), &Asker::Unnamed)
        .unwrap();
    before
        .registry
        .cordon(&host("b"), secs(100), &Asker::Unnamed)
        .unwrap();
    before.registry.uncordon(&host("b"), None).unwrap();
    let saved = before.registry.bindings();
    assert_eq!(saved[&host("a")].cordon_until_ms(), Some(1_100_000));
    assert_eq!(saved[&host("b")].cordon_until_ms(), None);
    assert_eq!(saved[&host("c")].cordon_until_ms(), None);

    let after = fixture_of(config);
    after
        .registry
        .restore_tainted(&host("b"), saved[&host("b")].clone());
    assert!(after.registry.hosts().is_empty());
    let lapsed = Binding::new(cred("t"), None).with_cordon(Some(999_999));
    after.registry.restore_tainted(&host("c"), lapsed);
    assert!(after.registry.hosts().is_empty());
    after
        .registry
        .restore_tainted(&host("a"), saved[&host("a")].clone());
    let listed = after.registry.hosts();
    assert_eq!(listed.len(), 1);
    assert_eq!(until(&after, "a"), Some(Timestamp::from_millis(1_100_000)));

    after.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(until(&after, "a"), Some(Timestamp::from_millis(1_100_000)));
    assert_eq!(
        after.registry.hosts()[0].cordon_until,
        Some(Timestamp::from_millis(1_100_000))
    );
}

/// Asserts that putting back a host the registry already knows leaves it as it is: a cordon
/// the binding carries does not replace the one in force. It catches a restore that overwrites
/// the record of a host that has already joined.
#[test]
fn restoring_a_host_the_registry_knows_leaves_its_cordon_as_it_is() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.registry
        .cordon(&host("a"), secs(100), &Asker::Unnamed)
        .unwrap();
    let later = Binding::new(cred("t"), None).with_cordon(Some(9_000_000));
    fx.registry.restore_tainted(&host("a"), later);
    assert_eq!(fx.registry.hosts().len(), 1);
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_100_000)));
}

/// Asserts that a cordoned host that falls silent is not forgotten while its cordon is in
/// force, however long the retention, and is forgotten like any lost host once the cordon
/// has lapsed and the retention has passed. It catches a cordon that a silent host loses
/// with its record.
#[test]
fn a_cordoned_silent_host_is_kept_until_the_cordon_lapses() {
    let config = config(2)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let fx = fixture_of(config);
    fx.join("a", "t");
    fx.registry
        .cordon(&host("a"), secs(3600), &Asker::Unnamed)
        .unwrap();
    fx.clock.advance(Duration::from_millis(
        js_agent_wire::DEFAULT_GRACE_MS + MIN_LOST_RETENTION_MS + 1,
    ));
    assert_eq!(fx.registry.hosts().len(), 1);

    fx.clock.advance(secs(3600));
    assert!(fx.registry.hosts().is_empty());
}

fn holder(name: &str) -> CordonHolder {
    CordonHolder::new(name).unwrap()
}

fn by(name: &str) -> Asker {
    Asker::Holder(holder(name))
}

fn holder_of(fx: &Fixture, name: &str) -> Option<String> {
    let own = fx.registry.own_host(&host(name)).unwrap();
    own.cordon_holder.map(|held| held.to_string())
}

fn left(n: u64) -> Cordon<Duration> {
    Cordon {
        until: secs(n),
        holder: Some(holder("run-m")),
    }
}

/// Asserts a named request through the registry: it places a cordon held by its holder, the
/// holder's own repeat renews it without shortening, another holder's request leaves the
/// cordon and its time as they were and says so, and a host that is not cordoned reports no
/// holder. It catches a registry that drops the holder or lets a second holder extend a
/// cordon.
#[test]
fn a_named_request_places_and_renews_and_another_holder_leaves_it() {
    let fx = fixture(2);
    fx.join("a", "t");
    assert_eq!(holder_of(&fx, "a"), None);
    let placed = fx
        .registry
        .cordon(&host("a"), secs(100), &by("run-a"))
        .unwrap();
    assert!(placed.placed && !placed.held_by_other);
    assert_eq!(placed.host.cordon_holder, Some(holder("run-a")));
    let renewed = fx
        .registry
        .cordon(&host("a"), secs(200), &by("run-a"))
        .unwrap();
    assert!(!renewed.placed && !renewed.held_by_other);
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_200_000)));
    let kept = fx
        .registry
        .cordon(&host("a"), secs(900), &by("run-b"))
        .unwrap();
    assert!(!kept.placed && kept.held_by_other);
    assert_eq!(kept.host.cordon_holder, Some(holder("run-a")));
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_200_000)));
}

/// Asserts that an unnamed request extends a named cordon and leaves its holder, and that a
/// take-over makes the cordon the asker's. It catches an unnamed request that strips the
/// holder and a take-over that is refused.
#[test]
fn an_unnamed_request_keeps_the_holder_and_a_take_over_changes_it() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.registry
        .cordon(&host("a"), secs(100), &by("run-a"))
        .unwrap();
    let extended = fx
        .registry
        .cordon(&host("a"), secs(300), &Asker::Unnamed)
        .unwrap();
    assert!(!extended.held_by_other);
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_300_000)));
    assert_eq!(holder_of(&fx, "a").as_deref(), Some("run-a"));
    let taken = fx
        .registry
        .cordon(&host("a"), secs(10), &Asker::TakeOver(holder("run-b")))
        .unwrap();
    assert!(!taken.placed && !taken.held_by_other);
    assert_eq!(holder_of(&fx, "a").as_deref(), Some("run-b"));
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_300_000)));
}

/// Asserts each lift through the registry: a lift naming another holder leaves the cordon
/// and says so, one naming the holder ends it and drops the holder, an unnamed lift ends a
/// named cordon, and a lift of nothing is not held by another. It catches a holder that
/// lifts another's cordon and a lift that leaves the holder behind.
#[test]
fn a_lift_by_a_holder_ends_only_its_own_cordon() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.registry
        .cordon(&host("a"), secs(100), &by("run-a"))
        .unwrap();
    let other = fx
        .registry
        .uncordon(&host("a"), Some(&holder("run-b")))
        .unwrap();
    assert!(!other.lifted && other.held_by_other);
    assert_eq!(holder_of(&fx, "a").as_deref(), Some("run-a"));
    let own = fx
        .registry
        .uncordon(&host("a"), Some(&holder("run-a")))
        .unwrap();
    assert!(own.lifted && !own.held_by_other);
    assert_eq!((until(&fx, "a"), holder_of(&fx, "a")), (None, None));
    let none = fx
        .registry
        .uncordon(&host("a"), Some(&holder("run-a")))
        .unwrap();
    assert!(!none.lifted && !none.held_by_other);
    fx.registry
        .cordon(&host("a"), secs(100), &by("run-a"))
        .unwrap();
    assert!(fx.registry.uncordon(&host("a"), None).unwrap().lifted);
    assert_eq!(holder_of(&fx, "a"), None);
}

/// Asserts that the holder is kept through a new hello of the host and dropped when the
/// cordon lapses, so a cordon placed after the lapse by an unnamed request is unnamed. It
/// catches a hello that forgets the holder and a lapsed cordon whose holder is still
/// reported.
#[test]
fn the_holder_is_kept_through_a_hello_and_dropped_when_the_cordon_lapses() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.registry
        .cordon(&host("a"), secs(3600), &by("run-a"))
        .unwrap();
    fx.pass_grace();
    fx.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(holder_of(&fx, "a").as_deref(), Some("run-a"));
    fx.clock.advance(secs(3600));
    assert_eq!(until(&fx, "a"), None);
    assert_eq!(holder_of(&fx, "a"), None);
    fx.registry
        .cordon(&host("a"), secs(60), &Asker::Unnamed)
        .unwrap();
    assert_eq!(holder_of(&fx, "a"), None);
}

/// Asserts that `set_cordon` sets the cordon exactly as given, its time left from now and its
/// holder, replaces it with a shorter one, and clears it with `None`; and that a host the
/// registry does not hold is `NotHeld`. It catches a set that extends instead of replacing.
#[test]
fn set_cordon_sets_replaces_and_clears() {
    let fx = fixture(2);
    fx.join("a", "t");
    let set = fx
        .registry
        .set_cordon(&host("a"), Some(&left(100)))
        .unwrap();
    assert_eq!(set.cordon_until, Some(Timestamp::from_millis(1_100_000)));
    assert_eq!(set.cordon_holder, Some(holder("run-m")));
    let shorter = Cordon {
        until: secs(10),
        holder: None,
    };
    let set = fx.registry.set_cordon(&host("a"), Some(&shorter)).unwrap();
    assert_eq!(set.cordon_until, Some(Timestamp::from_millis(1_010_000)));
    assert_eq!(set.cordon_holder, None);
    let cleared = fx.registry.set_cordon(&host("a"), None).unwrap();
    assert_eq!(cleared.cordon_until, None);
    assert_eq!(fx.registry.set_cordon(&host("b"), None), Err(NotHeld));
}

/// Asserts that `mirror_cordons` makes the registry's cordons the map's: it sets a cordon
/// the registry did not have, replaces one it had, clears the cordon of a host the map does
/// not list, and ignores a host the registry does not hold. It catches a mirror that only
/// adds, or that makes a host up.
#[test]
fn mirror_cordons_sets_replaces_clears_and_ignores_unknown_hosts() {
    let fx = fixture(3);
    for name in ["a", "b", "c"] {
        fx.join(name, "t");
    }
    fx.registry
        .cordon(&host("b"), secs(500), &by("run-b"))
        .unwrap();
    fx.registry
        .cordon(&host("c"), secs(500), &Asker::Unnamed)
        .unwrap();
    let rows = BTreeMap::from([
        (host("a"), left(100)),
        (
            host("b"),
            Cordon {
                until: secs(20),
                holder: None,
            },
        ),
        (host("unknown"), left(100)),
    ]);
    fx.registry.mirror_cordons(&rows);
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_100_000)));
    assert_eq!(holder_of(&fx, "a").as_deref(), Some("run-m"));
    assert_eq!(until(&fx, "b"), Some(Timestamp::from_millis(1_020_000)));
    assert_eq!(holder_of(&fx, "b"), None);
    assert_eq!(until(&fx, "c"), None);
    assert_eq!(fx.registry.own_host(&host("unknown")), None);
    assert_eq!(fx.registry.hosts().len(), 3);
}

/// Asserts that `cordons` lists only the cordons in force, each with its time left and its
/// holder: a lapsed cordon and an uncordoned host are not there, and the time left shrinks
/// as the clock moves. It catches a listing that keeps a lapsed cordon or reports the
/// time it was set for.
#[test]
fn cordons_lists_those_in_force_with_their_time_left() {
    let fx = fixture(3);
    for name in ["a", "b", "c"] {
        fx.join(name, "t");
    }
    assert!(fx.registry.cordons().is_empty());
    fx.registry
        .cordon(&host("a"), secs(100), &by("run-a"))
        .unwrap();
    fx.registry
        .cordon(&host("b"), secs(5), &Asker::Unnamed)
        .unwrap();
    fx.clock.advance(secs(10));
    fx.beat("a", 1);
    assert_eq!(
        fx.registry.cordons(),
        BTreeMap::from([(
            host("a"),
            Cordon {
                until: secs(90),
                holder: Some(holder("run-a"))
            }
        )])
    );
}

/// Asserts that the listing's view, which placement reads, follows a mirrored cordon: a host
/// mirrored as cordoned carries the time it lapses and a host mirrored clear carries none.
/// It catches a mirror that does not reach the view.
#[test]
fn a_mirrored_cordon_is_what_the_placement_view_reads() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.registry
        .mirror_cordons(&BTreeMap::from([(host("a"), left(60))]));
    assert_eq!(
        fx.registry.hosts()[0].cordon_until,
        Some(Timestamp::from_millis(1_060_000))
    );
    fx.registry.mirror_cordons(&BTreeMap::new());
    assert_eq!(fx.registry.hosts()[0].cordon_until, None);
}
