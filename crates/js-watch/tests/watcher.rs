//! The watcher against fake clouds, fake status pages and the fake sink, on a clock that
//! moves only when a test moves it. Nothing here dials anything.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_alert::FakeSink;
use js_watch::fake::{FakeCloud, FakeStatus};
use js_watch::{Seen, Settings, Status, Stray, StrayKind, Watcher};

const STALE: u64 = 300;
const SWEEP: u64 = 1800;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

struct Rig {
    cloud: Arc<FakeCloud>,
    status: Arc<FakeStatus>,
    sink: Arc<FakeSink>,
    watcher: Watcher,
}

/// A rig whose heartbeat is never stale in the time a test spans.
fn rig() -> Rig {
    rig_at(100_000)
}

/// A rig that alarms on a heartbeat unchanged for `stale` seconds.
fn rig_at(stale: u64) -> Rig {
    let cloud = Arc::new(FakeCloud::new());
    let status = Arc::new(FakeStatus::new(shows(Some(1), &[])));
    let sink = Arc::new(FakeSink::new());
    let settings = Settings {
        stale_after: secs(stale),
        confirm_ticks: 2,
        sweep_every: secs(SWEEP),
    };
    let watcher = Watcher::new((cloud.clone(), status.clone()), sink.clone(), settings);
    Rig {
        cloud,
        status,
        sink,
        watcher,
    }
}

fn shows(beat: Option<u64>, machines: &[&str]) -> Status {
    Status {
        beat,
        machines: machines
            .iter()
            .map(|m| (*m).to_owned())
            .collect::<BTreeSet<_>>(),
    }
}

fn seen(id: &str, name: Option<&str>, region: &str) -> Seen {
    Seen {
        id: id.to_owned(),
        name: name.map(str::to_owned),
        region: region.to_owned(),
    }
}

fn stray(kind: StrayKind, id: &str, region: &str, cents: Option<u64>) -> Stray {
    Stray {
        kind,
        id: id.to_owned(),
        region: region.to_owned(),
        cents_per_hour: cents,
    }
}

/// Asserts the heartbeat alarm on a manual clock: while the server's tick time moves the
/// sink hears nothing; when it stops moving the alarm is raised at `stale_after` exactly
/// (not one second before), once however long it lasts; it clears when the time moves
/// again and is raised again when it freezes again. It catches a watcher that cannot tell
/// a dead server from a quiet one, one that alarms early, and one that floods.
#[test]
fn a_stale_heartbeat_raises_the_alarm_once_and_again_after_it_clears() {
    let mut r = rig_at(STALE);
    for i in 0..5 {
        r.status.show(shows(Some(100 + i), &[]));
        r.watcher.tick(secs(i * 60));
    }
    assert!(r.sink.sent().is_empty());
    // The tick time moved last at 240 s and is frozen from there.
    r.watcher.tick(secs(240 + STALE - 1));
    assert!(r.sink.sent().is_empty());
    r.watcher.tick(secs(240 + STALE));
    r.watcher.tick(secs(240 + STALE + 60));
    assert_eq!(r.sink.keys(), ["heartbeat"]);
    let text = r.sink.sent()[0].text.clone();
    assert!(text.contains("300 seconds"), "{text}");
    assert!(text.contains("does not move"), "{text}");

    r.status.show(shows(Some(999), &[]));
    r.watcher.tick(secs(900));
    r.watcher.tick(secs(900 + STALE));
    assert_eq!(r.sink.keys(), ["heartbeat", "heartbeat"]);
}

/// Asserts that a status page that cannot be read counts as silence from the first moment
/// it could not be read, and that the alarm names the reason. It catches a watcher that
/// treats "cannot ask the server" as "nothing wrong", which is the case it exists for.
#[test]
fn an_unreadable_status_page_is_silence_too() {
    let mut r = rig_at(STALE);
    r.status.go_down();
    r.watcher.tick(secs(0));
    r.watcher.tick(secs(STALE - 1));
    assert!(r.sink.sent().is_empty());
    r.watcher.tick(secs(STALE));
    let text = r.sink.sent()[0].text.clone();
    assert_eq!(r.sink.keys(), ["heartbeat"]);
    assert!(text.contains("the status page is down"), "{text}");
}

/// Asserts that a server that never shows a tick time (it has not ticked once) is silent
/// from the watcher's start. It catches a watcher that waits for a first beat before it
/// counts, which a server wedged at boot would never give.
#[test]
fn a_server_that_never_ticks_is_stale_from_the_watchers_start() {
    let mut r = rig_at(STALE);
    r.status.show(shows(None, &[]));
    r.watcher.tick(secs(10));
    r.watcher.tick(secs(10 + STALE));
    assert_eq!(r.sink.keys(), ["heartbeat"]);
}

/// Asserts the orphan path on a manual clock: an instance the provider lists and the
/// status does not show reaches the sink on the second tick (`confirm_ticks` is 2, so a
/// launch that shows in the list a moment before the records is not an alarm), once, and
/// again after it is gone and comes back. Machines the status shows by provider id, or
/// only by name, are never reported. It catches a watcher that reports the instances the
/// server owns, one that never reports the one it does not, and one that alarms on a
/// single sighting.
#[test]
fn an_orphan_in_the_fake_cloud_reaches_the_sink_within_two_ticks() {
    let mut r = rig();
    r.status.show(shows(Some(1), &["i-1", "js-named"]));
    r.cloud.set_instances(vec![
        seen("i-1", Some("js-a"), "region-a"),
        seen("i-2", Some("js-named"), "region-a"),
        seen("i-9", None, "region-b"),
    ]);
    let report = r.watcher.tick(secs(0));
    assert!(r.sink.sent().is_empty());
    assert_eq!(report.instances, 3);
    assert_eq!(report.unexplained, 0);
    let report = r.watcher.tick(secs(60));
    assert_eq!(r.sink.keys(), ["unexplained:fakecloud:i-9"]);
    let text = r.sink.sent()[0].text.clone();
    assert!(
        text.contains("region-b") && text.contains("name none"),
        "{text}"
    );
    assert_eq!(report.unexplained, 1);
    r.watcher.tick(secs(120));
    assert_eq!(r.sink.keys().len(), 1);

    // Gone for a tick, then back: counted from one again, said again at the second sighting.
    r.cloud.set_instances(Vec::new());
    r.watcher.tick(secs(180));
    r.cloud.set_instances(vec![seen("i-9", None, "region-b")]);
    r.watcher.tick(secs(240));
    assert_eq!(r.sink.keys().len(), 1);
    r.watcher.tick(secs(300));
    assert_eq!(r.sink.keys().len(), 2);
}

/// Asserts that an instance the status begins to show before the confirm count is reached
/// is never reported. It catches a counter that is not reset when the server catches up.
#[test]
fn an_instance_the_server_catches_up_on_is_never_reported() {
    let mut r = rig();
    r.cloud
        .set_instances(vec![seen("i-5", Some("js-new"), "region-a")]);
    r.watcher.tick(secs(0));
    r.status.show(shows(Some(2), &["js-new"]));
    r.watcher.tick(secs(60));
    r.watcher.tick(secs(120));
    assert!(r.sink.sent().is_empty());
}

/// Asserts that when the status was never readable the watcher reports no instance as
/// unexplained (it has no account to compare with; the heartbeat alarm speaks), and that
/// it counts the instances all the same. It catches a watcher that calls every instance an
/// orphan whenever the server is down, burying the one alarm that matters.
#[test]
fn without_any_status_there_are_no_orphans_only_the_heartbeat() {
    let mut r = rig();
    r.status.go_down();
    r.cloud.set_instances(vec![seen("i-5", None, "region-a")]);
    let report = r.watcher.tick(secs(0));
    r.watcher.tick(secs(60));
    r.watcher.tick(secs(120));
    assert!(r.sink.sent().is_empty());
    assert_eq!(report.instances, 1);
    assert_eq!(report.unexplained, 0);
}

/// Asserts that a list that fails is an alert of its own, said once, that nothing is
/// compared while it fails, and that it clears when the list works. It catches a failed
/// list read as an empty cloud (no orphan can be found in nothing).
#[test]
fn a_list_that_fails_is_an_alert_and_never_an_empty_cloud() {
    let mut r = rig();
    r.cloud.fail_instances(true);
    let report = r.watcher.tick(secs(0));
    r.watcher.tick(secs(60));
    assert_eq!(r.sink.keys(), ["list:fakecloud"]);
    assert!(r.sink.sent()[0].text.contains("the instance list is down"));
    assert_eq!(report.instances, 0);
    assert_eq!(report.alerts, 1);
    r.cloud.fail_instances(false);
    let report = r.watcher.tick(secs(120));
    assert_eq!(report.alerts, 0);
}

/// Asserts the sweep with fakes holding strays in a second region: leftovers of several
/// kinds in region-b reach the sink with their kind, id, region and hourly cost, one alert
/// each, on the first tick; they are counted in the report; nothing is released (the
/// watcher has no call that could). It catches a sweep that reads only the home region and
/// one that reports a leftover without saying where it is.
#[test]
fn the_sweep_reports_strays_in_a_second_region() {
    let mut r = rig();
    r.cloud.set_strays(vec![
        stray(StrayKind::Volume, "vol-1", "region-b", Some(1234)),
        stray(StrayKind::FileSystem, "fs-1", "region-b", None),
    ]);
    let report = r.watcher.tick(secs(0));
    assert_eq!(
        r.sink.keys(),
        [
            "stray:file-system:region-b:fs-1",
            "stray:volume:region-b:vol-1"
        ]
    );
    let texts: Vec<String> = r.sink.sent().into_iter().map(|a| a.text).collect();
    assert!(texts[1].contains("12.34 USD per hour"), "{}", texts[1]);
    assert!(texts[1].contains("region-b") && texts[1].contains("vol-1"));
    assert!(
        texts[0].contains("price the cloud did not say"),
        "{}",
        texts[0]
    );
    assert_eq!(report.strays, 2);
    assert_eq!(report.alerts, 2);
}

/// Asserts the period of the sweep: a leftover that appears between sweeps is not seen
/// until the next one is due, a leftover said stays said across ticks, and one that is
/// cleaned up clears its alert. It catches a sweep that runs every tick (a list call per
/// kind per minute) and one that never runs again.
#[test]
fn the_sweep_runs_when_due_and_a_cleaned_leftover_clears() {
    let mut r = rig();
    r.watcher.tick(secs(0));
    r.cloud
        .set_strays(vec![stray(StrayKind::Address, "ip-1", "region-a", None)]);
    r.watcher.tick(secs(SWEEP - 1));
    assert!(r.sink.sent().is_empty());
    r.watcher.tick(secs(SWEEP));
    r.watcher.tick(secs(SWEEP + 60));
    assert_eq!(r.sink.keys(), ["stray:address:region-a:ip-1"]);
    r.cloud.set_strays(Vec::new());
    r.watcher.tick(secs(2 * SWEEP));
    r.cloud
        .set_strays(vec![stray(StrayKind::Address, "ip-1", "region-a", None)]);
    r.watcher.tick(secs(3 * SWEEP));
    assert_eq!(r.sink.keys().len(), 2);
}

/// Asserts that a sweep that fails is an alert of its own, that the leftovers of the last
/// good sweep are kept while it fails, and that it clears when a sweep works. It catches a
/// failed sweep read as "no leftovers".
#[test]
fn a_failed_sweep_is_an_alert_and_keeps_what_it_knew() {
    let mut r = rig();
    r.cloud
        .set_strays(vec![stray(StrayKind::Disk, "disk-1", "region-a", None)]);
    r.watcher.tick(secs(0));
    r.cloud.fail_strays(true);
    let report = r.watcher.tick(secs(SWEEP));
    assert_eq!(
        r.sink.keys(),
        ["stray:disk:region-a:disk-1", "sweep:fakecloud"]
    );
    assert_eq!(report.strays, 1);
    assert_eq!(report.alerts, 2);
    r.cloud.fail_strays(false);
    let report = r.watcher.tick(secs(2 * SWEEP));
    assert_eq!(report.alerts, 1);
}

/// Asserts that every kind of leftover has its own word, the same in `as_str` and in
/// `Display`. It catches two kinds with one key (one alert hiding another).
#[test]
fn every_kind_of_stray_has_its_own_word() {
    let words: BTreeSet<&str> = StrayKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(words.len(), StrayKind::ALL.len());
    for kind in StrayKind::ALL {
        assert_eq!(kind.to_string(), kind.as_str());
    }
}

/// Asserts that `drive` ticks with the clock it is given, in order, pauses between ticks,
/// and stops when the pause says so. It catches a loop that ignores the pause's answer
/// (it would never end) and one that ticks at the wrong time.
#[test]
fn drive_ticks_on_the_given_clock_until_the_pause_says_stop() {
    let mut r = rig();
    r.cloud.fail_instances(true);
    let mut now = 0;
    let mut pauses = 0;
    r.watcher.drive(
        || {
            now += 60;
            secs(now)
        },
        || {
            pauses += 1;
            pauses < 3
        },
    );
    assert_eq!((now, pauses), (180, 3));
    assert_eq!(r.sink.keys(), ["list:fakecloud"]);
}
