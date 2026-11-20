//! `js-agent update-hook drain` and `resume` against check 9 of the conformance harness of the
//! host updater's hook contract (`hostkeeper_hook::conformance`, at the revision the workspace
//! pins): each step run twice ends the same and changes nothing the second time; `resume` with
//! no `drain` before it changes nothing; `drain` then `resume` leaves the world as it was
//! (with no deadline and with a deadline that has passed while no work is left); and a `drain`
//! whose deadline has passed with work on the host exits 75, cuts no work and leaves the host
//! resumed.
//!
//! The harness starts `js-update-hook-fake` (tests/bin/update_hook_fake.rs), which runs the two
//! steps over the world's lease server, units and host facts, the hold file in the world's state
//! root and the harness's kill points. The work in flight is [`AgentHost::start_work`]: a lease
//! on the host that nothing ends. Each check runs in both forms the updater uses, the bare word
//! (`js-agent drain`) and behind the verb (`js-agent update-hook drain`), and the two must give
//! the same result.

use hostkeeper_hook::conformance::{FakeService, Sandbox};
use js_agent::update_hook::{Lease, LeaseError};
use js_testkit::dir::TestDir;

use super::conformance::{AgentHost, run};

/// Asserts that check 9 passes on an agent that runs and a host the lease server knows, in both
/// forms, and that it ran both scenarios (a `drain` and a `resume` with no work, and a `drain`
/// past its deadline with work in flight) and skipped none. It catches a `drain` or a `resume`
/// whose second run changes the world, a `resume` that leaves the hold or the cordon behind, a
/// `drain` past its deadline that cuts the lease or leaves the host cordoned, and a scenario the
/// fake silently could not make.
#[test]
fn drain_and_resume_pass_check_9_in_both_forms_with_every_scenario_run() {
    let (ran, skipped) = run("hook-conf-drain", AgentHost::update(), |harness| {
        harness.drain_resume()
    })
    .unwrap();
    assert_eq!(ran, ["drain,resume", "drain=deadline"]);
    assert_eq!(skipped, Vec::<String>::new());
}

/// Asserts that on a first install, where the lease server knows no host, the fake puts no work
/// in flight, and the harness says so: the `drain` past its deadline reports that it could not
/// be made. It catches a fake that claims work it did not start, which would let the scenario
/// pass without having run.
#[test]
fn a_first_install_has_no_work_to_put_in_flight_so_the_deadline_scenario_is_not_made() {
    let made = run(
        "hook-conf-drain-first",
        AgentHost::first_install(),
        |harness| harness.drain_past_deadline(),
    )
    .unwrap();
    assert!(!made);
}

/// Asserts that `start_work` gives the host a lease the lease server shows (one lease, named
/// `lease-1`) where it showed none before, and that on a first install it starts nothing and the
/// server still knows no host. It catches a work that leaves the host idle, which a `drain` would
/// then pass whatever its deadline.
#[test]
fn start_work_puts_a_lease_on_the_host_and_on_a_first_install_starts_nothing() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-work");
    let sandbox = Sandbox::create(tmp.path().join("run")).unwrap();
    let mut fake = AgentHost::update();
    fake.init(&sandbox).unwrap();
    let idle = AgentHost::world(&sandbox).host().unwrap();
    assert_eq!((idle.leases, idle.lease), (0, None));

    assert!(fake.start_work(&sandbox).unwrap());

    let busy = AgentHost::world(&sandbox).host().unwrap();
    assert_eq!((busy.leases, busy.lease.as_deref()), (1, Some("lease-1")));

    let first = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-work-first");
    let sandbox = Sandbox::create(first.path().join("run")).unwrap();
    let mut fake = AgentHost::first_install();
    fake.init(&sandbox).unwrap();
    assert!(!fake.start_work(&sandbox).unwrap());
    assert!(matches!(
        AgentHost::world(&sandbox).host(),
        Err(LeaseError::UnknownHost)
    ));
}
