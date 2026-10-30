//! The lease journal and what rests on it: a lease is written before the helper hears of
//! it, leases an earlier process left are ended when a runtime starts to work (and by the
//! rescue), what cannot be ended is kept, and a helper that restarted under the runtime is
//! asked for its boot reset again.

use std::fs;
use std::io::Write as _;
use std::time::Duration;

use js_agent::lifecycle::{Runtime, Status};
use js_agent::native::{Journal, NativeRuntime};
use js_mac_session::helper::{RESET_ALREADY, RESET_NEEDED};

use super::{Rig, TERM, config, native};
use crate::support::Scratch;

fn live(rig: &Rig) -> Vec<String> {
    Journal::new(&rig.dir.path("native-leases")).live().unwrap()
}

/// A runtime of the same helper and journal as `rig`'s, as a later process of the agent
/// makes one.
fn later(rig: &Rig) -> NativeRuntime {
    NativeRuntime::open(rig.fake.client(), config(&rig.fake), TERM + 1).unwrap()
}

fn seed(rig: &Rig, text: &str) {
    fs::write(rig.dir.path("native-leases"), text).unwrap();
}

/// Asserts the journal's own rules: a file that is not there is empty, an added lease is
/// live until it is marked done, a line that is neither an addition nor a removal (a torn
/// one) is ignored, a rewrite holds exactly what it is given, and every way the file cannot
/// be read, written or replaced is an error. It catches a journal that forgets a lease or
/// keeps one that was ended.
#[test]
fn the_journal_keeps_what_was_added_and_not_marked_done() {
    let dir = Scratch::new("journal-file");
    let journal = Journal::new(&dir.path("j"));
    assert_eq!(journal.live().unwrap(), Vec::<String>::new());
    journal.add("1.1").unwrap();
    journal.add("1.2").unwrap();
    journal.done("1.1").unwrap();
    assert_eq!(journal.live().unwrap(), ["1.2"]);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(dir.path("j"))
        .unwrap();
    write!(file, "torn\nx 9.9\n+ 1.3").unwrap();
    assert_eq!(journal.live().unwrap(), ["1.2", "1.3"]);
    journal.rewrite(&["2.2".to_owned()]).unwrap();
    assert_eq!(journal.live().unwrap(), ["2.2"]);

    let nowhere = Journal::new(&dir.path("no-such-dir").join("j"));
    assert!(nowhere.add("1.1").is_err());
    assert!(nowhere.rewrite(&[]).is_err());
    let unreadable = Journal::new(&dir.path(""));
    assert!(unreadable.live().is_err());
}

/// Asserts that a lease is in the journal when the helper is asked to make its user, and
/// out of it once the user is removed. It catches a journal written after the request,
/// which a crash between the two would leave a user the agent never heard of.
#[test]
fn a_lease_is_journalled_before_the_helper_hears_of_it() {
    let mut rig = Rig::new("journal-order");
    rig.fake.watch_journal(&rig.dir.path("native-leases"));
    let id = rig.start("r1", "/bin/true", &[]);
    let create = rig
        .fake
        .seen()
        .into_iter()
        .find(|s| s.verb == "user-create");
    assert!(
        create.unwrap().journaled,
        "the lease was not journalled first"
    );
    assert_eq!(live(&rig), ["7.0"]);
    rig.rt.remove(&id).unwrap();
    assert!(live(&rig).is_empty());
}

/// Asserts that a start that cannot be undone keeps its lease: the helper refuses the user
/// and then the kill and delete, so the lease stays in the journal, and a later runtime
/// ends it. It catches a lease user that nothing will ever end.
#[test]
fn a_lease_that_could_not_be_undone_is_ended_by_a_later_runtime() {
    let mut rig = Rig::new("journal-undone");
    rig.fake.refuse("user-create", "range full");
    rig.fake.refuse("kill-uid", "stuck");
    rig.fake.refuse("user-delete", "busy");
    let spec = rig.spec("r1", native("/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(live(&rig), ["7.0"]);
    rig.fake.allow("kill-uid");
    rig.fake.allow("user-delete");
    let lines = later(&rig).reclaim();
    assert_eq!(lines, ["ended the lease user 7.0 an earlier process left"]);
    assert!(live(&rig).is_empty());
    let verbs = rig.fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 7.0", "user-delete 7.0"]
    );
}

/// Asserts the crash path: a run is started and its runtime is dropped without a stop (the
/// agent died, stopped or was rescued), and the runtime of the next process, over a helper
/// that has reset already, kills and deletes the lease user and empties the journal; a
/// second look finds nothing. It catches runs left behind by every end of the agent.
#[test]
fn a_run_an_earlier_process_left_is_ended_when_the_next_runtime_reclaims() {
    let mut rig = Rig::new("journal-crash");
    rig.start("r1", "/bin/true", &[]);
    assert_eq!(live(&rig), ["7.0"]);
    rig.fake.refuse("boot-reset", RESET_ALREADY);
    let Rig { fake, rt, dir } = rig;
    drop(rt);
    let mut next = NativeRuntime::open(fake.client(), config(&fake), TERM + 1).unwrap();
    let lines = next.reclaim();
    assert_eq!(lines, ["ended the lease user 7.0 an earlier process left"]);
    let verbs = fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 7.0", "user-delete 7.0"]
    );
    assert!(
        Journal::new(&dir.path("native-leases"))
            .live()
            .unwrap()
            .is_empty()
    );
    assert!(next.reclaim().is_empty());
}

/// Asserts that what cannot be ended stays in the journal and is told, and is ended by the
/// next look when the helper allows it. It catches a journal emptied by a failed sweep.
#[test]
fn a_lease_that_cannot_be_ended_stays_in_the_journal() {
    let rig = Rig::new("journal-keep");
    seed(&rig, "+ 5.1\n");
    rig.fake.refuse("user-delete", "busy");
    let mut next = later(&rig);
    let lines = next.reclaim();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("cannot end the lease user 5.1: user-delete: "),
        "{lines:?}"
    );
    assert!(lines[0].contains("busy"));
    assert_eq!(live(&rig), ["5.1"]);
    rig.fake.allow("user-delete");
    assert_eq!(
        next.reclaim(),
        ["ended the lease user 5.1 an earlier process left"]
    );
    assert!(live(&rig).is_empty());
}

/// Asserts that a helper that says the lease has no user, or that its user was deleted, has
/// given the end that was wanted: both leases are taken as ended. It catches a sweep that
/// keeps a lease for ever because the helper already forgot it.
#[test]
fn a_lease_the_helper_already_ended_is_taken_as_ended() {
    let rig = Rig::new("journal-gone");
    seed(&rig, "+ 6.0\n+ 6.1\n");
    rig.fake.refuse("kill-uid", "lease 6.0's user was deleted");
    rig.fake.refuse("user-delete", "lease 6.0 has no user");
    let lines = later(&rig).reclaim();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("ended the lease user")),
        "{lines:?}"
    );
    assert!(live(&rig).is_empty());

    seed(&rig, "+ 6.2\n");
    rig.fake.refuse("kill-uid", "lease 6.2 has no user");
    rig.fake.allow("user-delete");
    assert_eq!(later(&rig).reclaim().len(), 1);
}

/// Asserts that a journal that cannot be read or rewritten is told and nothing else is
/// done, and that the rest of the sweep still ran. It catches a sweep that fails silently.
#[test]
fn a_journal_that_cannot_be_read_or_rewritten_is_told() {
    let rig = Rig::new("journal-broken");
    fs::create_dir(rig.dir.path("native-leases")).unwrap();
    let lines = later(&rig).reclaim();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("native: cannot read the lease journal"),
        "{lines:?}"
    );

    fs::remove_dir(rig.dir.path("native-leases")).unwrap();
    seed(&rig, "+ 4.4\n");
    fs::create_dir(rig.dir.path("native-leases.new")).unwrap();
    let lines = later(&rig).reclaim();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("ended the lease user 4.4")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("cannot rewrite the lease journal")),
        "{lines:?}"
    );
}

/// Asserts that a start whose lease cannot be journalled goes no further: the helper hears
/// nothing. It catches a user made that no journal names.
#[test]
fn a_journal_that_cannot_be_written_stops_the_start_before_the_helper() {
    let mut rig = Rig::in_dir("journal-nowrite", |config, dir| {
        config.journal = dir.path("no-such-dir").join("native-leases");
    });
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("the lease journal: "), "{}", error.0);
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
}

/// Asserts that a helper that restarted under the runtime (it refuses every verb until its
/// boot reset) is asked for the reset again before the next start, stop or removal, that a
/// reset that fails is told and tried again, and that a removal of a lease the helper
/// already deleted is done. It catches an agent wedged for good by a helper restart.
#[test]
fn a_helper_that_restarted_is_asked_for_its_boot_reset_again() {
    let mut rig = Rig::new("journal-reset");
    let id = rig.start("r1", "/bin/true", &[]);
    rig.fake.refuse("kill-uid", RESET_NEEDED);
    assert!(rig.rt.stop(&id, Duration::ZERO).is_err());
    rig.fake.allow("kill-uid");
    rig.fake.refuse("boot-reset", "no");
    let error = rig.rt.remove(&id).unwrap_err();
    assert!(error.0.starts_with("the boot reset: "), "{}", error.0);
    rig.fake.allow("boot-reset");
    rig.fake.refuse("kill-uid", "lease 7.0's user was deleted");
    rig.rt.remove(&id).unwrap();
    let resets = rig
        .fake
        .verbs()
        .iter()
        .filter(|v| *v == "boot-reset")
        .count();
    assert_eq!(
        resets, 3,
        "the open, the failed ask and the ask that worked"
    );
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Gone);

    rig.fake.allow("kill-uid");
    rig.fake.refuse("user-create", RESET_NEEDED);
    let spec = rig.spec("r2", native("/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap_err();
    rig.fake.allow("user-create");
    rig.start("r3", "/bin/true", &[]);
    let resets = rig
        .fake
        .verbs()
        .iter()
        .filter(|v| *v == "boot-reset")
        .count();
    assert_eq!(resets, 4);
}
