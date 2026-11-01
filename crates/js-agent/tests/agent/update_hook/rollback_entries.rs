//! The hosts a `rollback` is run on: what `prepare`, `cutover` and `verify` leave behind at
//! each point a rollback is entered from, and what a finished rollback looks like.

use std::path::PathBuf;
use std::time::Duration;

use js_agent::update_hook::{Clock, Link, Outcome, Reason, Units};

use super::step_rig::{Rig, Spy, UNIT, killable, running};

/// Time passes between one step and the next: the sessions of two agents differ.
pub const WAIT: Duration = Duration::from_secs(5);

pub const RELINK: &str = "link.relink";

pub fn stop() -> String {
    format!("units.stop({UNIT})")
}

pub fn start() -> String {
    format!("units.start({UNIT})")
}

pub fn previous(rig: &Rig) -> PathBuf {
    rig.params.previous_release.clone().unwrap()
}

pub fn staged(rig: &Rig) -> PathBuf {
    rig.params.staged_release.clone()
}

pub fn unit_state(rig: &Rig) -> String {
    let state = rig.world.show(UNIT).unwrap();
    format!("{} {}", state.active, state.sub)
}

pub fn link_target(rig: &Rig) -> Option<PathBuf> {
    rig.world.target(&rig.link).unwrap()
}

/// `rb.drained`, `rb.stop_begun`, `rb.link_restored`, `rb.start_issued`, `rb.hello_ok` on disk.
pub fn rb_marks(rig: &Rig) -> [bool; 5] {
    let rb = rig.record().unwrap().rb;
    [
        rb.drained,
        rb.stop_begun,
        rb.link_restored,
        rb.start_issued,
        rb.hello_ok,
    ]
}

/// The host after a `cutover` that was killed at the kill point `pick` chooses among the
/// `(act, kill points passed)` of an uninterrupted one.
fn cut_off(name: &str, pick: impl Fn(&[(String, u32)]) -> u32) -> Rig {
    let base = running(Rig::new(&format!("{name}-base")));
    let spy = Spy::new(&base);
    assert_eq!(spy.cutover(&base).outcome, Outcome::Success);
    let acts: Vec<(String, u32)> = spy
        .acts
        .borrow()
        .iter()
        .map(|act| (act.call.clone(), act.points))
        .collect();
    assert_eq!(acts.len(), 3, "stop, link, start");
    let rig = running(Rig::new(name));
    rig.kill.arm(pick(&acts));
    assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none());
    rig.kill.disarm();
    rig.world.pause(WAIT);
    rig
}

/// The unit is stopped, `link_moved` is marked and the link has not moved: it still names the
/// previous release.
pub fn stopped(name: &str) -> Rig {
    let rig = cut_off(name, |acts| acts[1].1);
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    assert_eq!(unit_state(&rig), "inactive dead");
    rig
}

/// The unit is stopped and the link names the staged release; `start_issued` is not marked.
pub fn link_moved(name: &str) -> Rig {
    let rig = cut_off(name, |acts| acts[2].1 - 1);
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    assert_eq!(unit_state(&rig), "inactive dead");
    assert!(!rig.record().unwrap().start_issued);
    rig
}

/// `start_issued` is marked and the start failed: the unit is `failed`, the link is staged.
pub fn start_failed(name: &str) -> Rig {
    let rig = running(Rig::new(name));
    rig.world.pause(WAIT);
    {
        let spy = Spy::new(&rig);
        *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
        spy.fail("start");
        let done = spy.cutover(&rig);
        assert_eq!(done.report.reason, Some(Reason::StartFailed), "{done:?}");
    }
    rig.kill.disarm();
    rig.world.pause(WAIT);
    assert_eq!(unit_state(&rig), "failed failed");
    assert!(rig.record().unwrap().start_issued);
    rig
}

/// The new agent runs and has not said hello; `verify` waited its bound and failed.
pub fn verify_failed(name: &str) -> Rig {
    verify_failed_on(running(Rig::new(name)))
}

/// The same on a host `prepare` has been run on.
pub fn verify_failed_on(rig: Rig) -> Rig {
    rig.world.pause(WAIT);
    {
        let spy = Spy::new(&rig);
        *spy.start_leaves.borrow_mut() = Some(("active", "running"));
        let done = spy.cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    }
    let done = Spy::new(&rig).verify(&rig);
    assert_eq!(done.report.reason, Some(Reason::NoHello), "{done:?}");
    rig.kill.disarm();
    rig.world.pause(WAIT);
    assert!(!rig.record().unwrap().verify_ok);
    rig
}

/// The new agent runs and `verify` succeeded.
pub fn verified(name: &str) -> Rig {
    verified_on(running(Rig::new(name)))
}

/// The same on a host `prepare` has been run on.
pub fn verified_on(rig: Rig) -> Rig {
    rig.world.pause(WAIT);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(Spy::new(&rig).verify(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    rig.world.pause(WAIT);
    assert!(rig.record().unwrap().verify_ok);
    rig
}

/// A first install after its `prepare`.
pub fn first_prepared(name: &str) -> Rig {
    let rig = Rig::first_install(name);
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    rig
}

/// The machine is the previous release's, running and serving, and nothing of the update is
/// left: no record, no cordon.
pub fn assert_restored(rig: &Rig) {
    assert_eq!(rig.record(), None, "the record is deleted");
    assert_eq!(link_target(rig), Some(previous(rig)));
    assert_eq!(unit_state(rig), "active running");
    let host = rig.host();
    assert_eq!(host.state, "live");
    assert_eq!(host.agent_version.as_deref(), Some("1.3.0"));
    assert!(host.cordon.is_none(), "the cordon of this update is lifted");
}

/// A `cutover` was killed while the unit stopped: it is `deactivating`, the link still names the
/// previous release.
pub fn deactivating(name: &str) -> Rig {
    let rig = running(Rig::new(name));
    {
        let killed = Spy::new(&rig);
        *killed.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
        killed.on_pause(|_, ticks| assert!(ticks < 3, "killed during the wait"));
        assert!(killable(|| killed.cutover(&rig)).is_none());
    }
    rig.kill.disarm();
    rig.world.pause(WAIT);
    assert_eq!(unit_state(&rig), "deactivating stop-sigterm");
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    rig
}
