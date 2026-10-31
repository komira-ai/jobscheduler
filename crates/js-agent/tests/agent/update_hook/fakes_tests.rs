//! The fakes of the ports (tests/update_hook_fakes): what each says and does, and that a
//! second `World` over the same directories sees it, as the hook process will.

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use js_agent::update_hook::{
    Clock, Gate, Lease, LeaseError, Link, NoGate, Params, StateView, Units,
};
use js_agent_wire::Taint;
use js_testkit::dir::TestDir;

use super::fakes::{START_MS, World};

const UNIT: &str = "js-agent-crun.service";
const HOST: &str = "burst-1";

struct Counting(Rc<Cell<u32>>);

impl Gate for Counting {
    fn point(&self) {
        self.0.set(self.0.get() + 1);
    }
}

struct Rig {
    tmp: TestDir,
    world: World,
    points: Rc<Cell<u32>>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let points = Rc::new(Cell::new(0));
        let world = World::new(
            &tmp.path().join("world"),
            &tmp.path().join("control"),
            Box::new(Counting(Rc::clone(&points))),
        );
        Self { tmp, world, points }
    }

    /// Another process's view of the same directories.
    fn again(&self) -> World {
        World::new(
            &self.tmp.path().join("world"),
            &self.tmp.path().join("control"),
            Box::new(NoGate),
        )
    }

    fn points(&self) -> u32 {
        self.points.get()
    }
}

#[test]
fn a_unit_never_run_is_inactive_and_start_then_stop_ends_inactive() {
    let rig = Rig::new("hook-fake-units");
    let w = &rig.world;
    assert_eq!(w.is_active(UNIT), Ok(false));
    assert_eq!(w.show(UNIT).unwrap().active, "inactive");
    w.start(UNIT).unwrap();
    assert_eq!(w.is_active(UNIT), Ok(true));
    let up = w.show(UNIT).unwrap();
    assert_eq!((up.active.as_str(), up.sub.as_str()), ("active", "running"));
    w.stop_no_block(UNIT).unwrap();
    assert_eq!(w.is_active(UNIT), Ok(false));
    let down = w.show(UNIT).unwrap();
    assert_eq!(
        (down.active.as_str(), down.sub.as_str()),
        ("inactive", "dead")
    );
    // Another process reads the same unit.
    assert_eq!(rig.again().is_active(UNIT), Ok(false));
}

#[test]
fn start_and_stop_that_change_nothing_pass_no_kill_point() {
    let rig = Rig::new("hook-fake-units-idem");
    rig.world.seed_unit(UNIT, "active", "running");
    rig.world.start(UNIT).unwrap();
    assert_eq!(rig.points(), 0);
    rig.world.stop_no_block(UNIT).unwrap();
    assert_eq!(rig.points(), 1);
    rig.world.stop_no_block(UNIT).unwrap();
    assert_eq!(rig.points(), 1);
    rig.world.start(UNIT).unwrap();
    assert_eq!(rig.points(), 2);
}

#[test]
fn a_seeded_unit_shows_the_states_it_was_given() {
    let rig = Rig::new("hook-fake-units-seeded");
    rig.world.seed_unit(UNIT, "deactivating", "stop-sigterm");
    let state = rig.again().show(UNIT).unwrap();
    assert_eq!(
        (state.active.as_str(), state.sub.as_str()),
        ("deactivating", "stop-sigterm")
    );
    assert_eq!(rig.world.is_active(UNIT), Ok(false));
}

#[test]
fn relink_is_a_rename_over_the_old_link_and_idempotent() {
    let rig = Rig::new("hook-fake-link");
    let link = Path::new("/opt/js-agent/current");
    assert_eq!(rig.world.target(link), Ok(None));
    rig.world.relink(link, Path::new("releases/1.0")).unwrap();
    assert_eq!(rig.points(), 1);
    rig.world.relink(link, Path::new("releases/1.0")).unwrap();
    assert_eq!(rig.points(), 1, "a link already there is not a change");
    rig.world.relink(link, Path::new("releases/1.1")).unwrap();
    assert_eq!(rig.points(), 2);
    assert_eq!(rig.again().target(link), Ok(Some("releases/1.1".into())));
    // Nothing is left beside the link in the world.
    let names: Vec<_> = fs::read_dir(rig.tmp.path().join("world/link"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["current"]);
}

#[test]
fn a_link_without_a_file_name_is_refused() {
    let rig = Rig::new("hook-fake-link-bad");
    assert!(rig.world.target(Path::new("/")).is_err());
    assert!(rig.world.relink(Path::new("/"), Path::new("x")).is_err());
}

#[test]
fn the_server_knows_no_host_until_it_is_seeded_or_says_hello() {
    let rig = Rig::new("hook-fake-unknown");
    assert_eq!(rig.world.host().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(rig.world.cordon(60).unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(rig.world.uncordon().unwrap_err(), LeaseError::UnknownHost);
    // A hello is how the server comes to know a host: it makes it.
    rig.world.hello("1.0").unwrap();
    let made = rig.world.host().unwrap();
    assert_eq!((made.epoch, made.state.as_str()), (1, "live"));
    assert_eq!(made.session_started_ms, Some(START_MS));
    assert_eq!(made.agent_version.as_deref(), Some("1.0"));
    assert_eq!(made.cordon, None);
}

#[test]
fn a_cordon_is_placed_for_the_lease_and_cut_to_the_longest_the_server_grants() {
    let rig = Rig::new("hook-fake-cordon");
    rig.world.seed_host(HOST, "1.0");
    let placed = rig.world.cordon(60).unwrap();
    assert!(!placed.already_cordoned);
    assert_eq!(placed.host.cordon.unwrap().expires_at_ms, START_MS + 60_000);
    assert_eq!(rig.points(), 1);
    let long = Rig::new("hook-fake-cordon-long");
    long.world.seed_host(HOST, "1.0");
    let cut = long.world.cordon(999_999).unwrap();
    assert_eq!(cut.host.cordon.unwrap().expires_at_ms, START_MS + 3_600_000);
    assert_eq!(
        long.world.cordon(0).unwrap_err(),
        LeaseError::Refused("invalid-cordon".to_owned())
    );
}

#[test]
fn a_repeat_cordon_extends_and_never_shortens() {
    let rig = Rig::new("hook-fake-cordon-repeat");
    let w = &rig.world;
    w.seed_host(HOST, "1.0");
    w.cordon(100).unwrap();
    w.pause(Duration::from_secs(30));
    let longer = w.cordon(300).unwrap();
    assert!(longer.already_cordoned);
    assert_eq!(
        longer.host.cordon.unwrap().expires_at_ms,
        START_MS + 330_000
    );
    w.pause(Duration::from_secs(10));
    let shorter = w.cordon(5).unwrap();
    assert!(shorter.already_cordoned);
    assert_eq!(
        shorter.host.cordon.unwrap().expires_at_ms,
        START_MS + 330_000
    );
    assert_eq!(
        w.host().unwrap().cordon.unwrap().expires_at_ms,
        START_MS + 330_000
    );
}

#[test]
fn a_cordon_that_has_expired_is_lifted_and_a_new_one_is_not_a_repeat() {
    let rig = Rig::new("hook-fake-cordon-expiry");
    let w = &rig.world;
    w.seed_host(HOST, "1.0");
    w.cordon(60).unwrap();
    w.pause(Duration::from_secs(59));
    assert!(w.host().unwrap().cordon.is_some());
    w.pause(Duration::from_secs(1));
    assert_eq!(w.host().unwrap().cordon, None);
    assert!(!w.uncordon().unwrap().lifted);
    assert!(!w.cordon(60).unwrap().already_cordoned);
}

#[test]
fn uncordon_lifts_once_and_a_second_call_lifts_nothing() {
    let rig = Rig::new("hook-fake-uncordon");
    let w = &rig.world;
    w.seed_host(HOST, "1.0");
    assert!(!w.uncordon().unwrap().lifted);
    assert_eq!(rig.points(), 0, "nothing to lift is not a change");
    w.cordon(60).unwrap();
    let lifted = w.uncordon().unwrap();
    assert!(lifted.lifted);
    assert_eq!(lifted.host.cordon, None);
    assert!(!w.uncordon().unwrap().lifted);
    assert_eq!(rig.again().host().unwrap().cordon, None);
}

#[test]
fn the_epoch_goes_up_at_each_hello_with_the_new_session_version_and_runtime() {
    let rig = Rig::new("hook-fake-hello");
    let w = &rig.world;
    w.seed_host(HOST, "1.0");
    let first = w.host().unwrap();
    assert_eq!((first.epoch, first.id.as_str()), (1, HOST));
    w.pause(Duration::from_secs(5));
    w.hello("1.1").unwrap();
    let second = rig.again().host().unwrap();
    assert_eq!(second.epoch, 2);
    assert_eq!(second.agent_version.as_deref(), Some("1.1"));
    assert_eq!(second.runtime.as_deref(), Some("crun"));
    assert_eq!(second.session_started_ms, Some(START_MS + 5_000));
    assert!(second.session_started_ms > first.session_started_ms);
}

#[test]
fn starting_the_unit_makes_the_agent_of_the_linked_release_say_hello() {
    let rig = Rig::new("hook-fake-start-hello");
    let w = &rig.world;
    w.seed_host(HOST, "1.0");
    // The version is the params', not the name of the directory.
    w.register_release(Path::new("/opt/js-agent/releases/build-77"), "1.2");
    w.relink(
        Path::new("/opt/js-agent/current"),
        Path::new("/opt/js-agent/releases/build-77"),
    )
    .unwrap();
    w.start(UNIT).unwrap();
    let host = w.host().unwrap();
    assert_eq!(host.epoch, 2);
    assert_eq!(host.agent_version.as_deref(), Some("1.2"));
}

#[test]
fn a_first_install_starts_cleanly_and_the_hello_makes_the_host() {
    let rig = Rig::new("hook-fake-first-start");
    let w = &rig.world;
    let link = Path::new("/opt/js-agent/current");
    let first = params(false);
    w.seed_update(link, HOST, &first);
    w.relink(link, &first.staged_release).unwrap();
    w.start(UNIT).unwrap();
    assert_eq!(w.is_active(UNIT), Ok(true));
    let host = w.host().unwrap();
    assert_eq!((host.epoch, host.state.as_str()), (1, "live"));
    assert_eq!(host.agent_version.as_deref(), Some("1.4.0"));
    assert_eq!(host.session_started_ms, Some(START_MS));
}

#[test]
fn a_server_that_restarted_forgets_the_host_and_the_next_hello_makes_it_again_from_epoch_1() {
    let rig = Rig::new("hook-fake-server-restart");
    let w = &rig.world;
    let link = Path::new("/opt/js-agent/current");
    let update = params(true);
    w.seed_update(link, HOST, &update);
    w.hello("1.3.0").unwrap();
    w.cordon(60).unwrap();
    w.pause(Duration::from_secs(5));
    w.forget_host();
    assert_eq!(w.host().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(w.cordon(60).unwrap_err(), LeaseError::UnknownHost);
    w.relink(link, &update.staged_release).unwrap();
    w.start(UNIT).unwrap();
    let host = w.host().unwrap();
    assert_eq!(
        (host.epoch, host.agent_version.as_deref()),
        (1, Some("1.4.0"))
    );
    assert_eq!(host.session_started_ms, Some(START_MS + 5_000));
    assert_eq!(host.cordon, None, "the new server holds no cordon");
    assert_eq!(host.id, HOST, "the host keeps its id");
}

#[test]
fn a_host_reading_carries_the_leases_taint_and_release_state_the_server_holds() {
    let rig = Rig::new("hook-fake-reading");
    let w = &rig.world;
    w.seed_host(HOST, "1.0");
    w.alter_host(|host| {
        host.leases = 1;
        host.lease = Some("lease-9".to_owned());
        host.releasing = true;
        host.taint = Some(Taint {
            reason: "residue".to_owned(),
            run_id: None,
            check: None,
        });
    });
    let host = w.host().unwrap();
    assert_eq!(host.leases, 1);
    assert_eq!(host.lease.as_deref(), Some("lease-9"));
    assert!(host.releasing);
    assert_eq!(host.taint.unwrap().reason, "residue");
    assert_eq!(host.state, "live");
}

#[test]
fn pausing_moves_the_clock_on_and_counts_without_waiting() {
    let rig = Rig::new("hook-fake-clock");
    let w = &rig.world;
    assert_eq!((w.now_ms(), w.ticks()), (START_MS, 0));
    w.pause(Duration::from_secs(3600));
    w.pause(Duration::from_millis(250));
    assert_eq!(w.now_ms(), START_MS + 3_600_250);
    assert_eq!(w.ticks(), 2);
    assert_eq!(rig.again().now_ms(), START_MS + 3_600_250);
    assert_eq!(rig.points(), 0, "time is not a change of the world");
}

#[test]
fn the_state_root_view_reads_the_taint_and_the_owed_records() {
    let rig = Rig::new("hook-fake-state");
    let root = rig.world.state_root();
    assert_eq!(StateView::taint(&root), None);
    assert_eq!(StateView::owed(&root), Ok(vec![]));
    root.owe("run-2").unwrap();
    root.owe("run-1").unwrap();
    root.write(&Taint {
        reason: "check-owed".to_owned(),
        run_id: Some("run-1".to_owned()),
        check: None,
    })
    .unwrap();
    let again = rig.again().state_root();
    assert_eq!(StateView::taint(&again).unwrap().reason, "check-owed");
    assert_eq!(
        StateView::owed(&again),
        Ok(vec!["run-1".to_owned(), "run-2".to_owned()])
    );
    assert!(rig.tmp.path().join("world/state").is_dir());
}

fn params(previous: bool) -> Params {
    let previous = match previous {
        true => concat!(
            "\"updater.previous_release\" = \"/opt/js-agent/releases/1.3.0\"\n",
            "\"updater.previous_version\" = \"1.3.0\"\n",
        ),
        false => "",
    };
    Params::parse(&format!(
        "{}{previous}",
        concat!(
            "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
            "\"updater.staged_release\" = \"/opt/js-agent/releases/1.4.0\"\n",
            "\"updater.target_version\" = \"1.4.0\"\n",
        )
    ))
    .unwrap()
}

#[test]
fn an_update_starts_from_the_previous_release_exactly_as_the_params_spell_it() {
    let rig = Rig::new("hook-fake-seed-update");
    let link = Path::new("/opt/js-agent/current");
    rig.world.seed_update(link, HOST, &params(true));
    assert_eq!(
        rig.world.target(link),
        Ok(Some("/opt/js-agent/releases/1.3.0".into()))
    );
    let host = rig.world.host().unwrap();
    assert_eq!(host.agent_version.as_deref(), Some("1.3.0"));
    assert_eq!(rig.points(), 0, "seeding passes no kill point");
}

#[test]
fn a_first_install_has_no_link_and_the_server_knows_no_host() {
    let rig = Rig::new("hook-fake-seed-first");
    let link = Path::new("/opt/js-agent/current");
    rig.world.seed_update(link, HOST, &params(false));
    assert_eq!(rig.world.target(link), Ok(None));
    assert_eq!(rig.world.host().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(rig.points(), 0);
}
