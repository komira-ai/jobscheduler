//! Tests of how a uid's System V IPC objects are found and removed.

use super::*;
use crate::mac_accounts::tests::rig::Rig;

/// Asserts that only the uid's objects are listed: an object of another owner and creator
/// is not, one the uid created and gave away is, a header or a section title is not read as
/// an object, and the uid's spelling as a number (after its record went) is found.
#[test]
fn only_the_uids_objects_are_listed() {
    let listing = "IPC status from <running system> as of Thu Nov  5 10:00:00 2026\n\
         T     ID     KEY        MODE       OWNER    GROUP\n\
         Message Queues:\n\
         q  65536 0x00000001 --rw-------  kbf-acct-3    staff\n\
         Shared Memory:\n\
         m  65537 0x00000002 --rw-------     root    wheel\n\
         m  65538 0x00000003 --rw-------      701    staff\n\
         m  65541 0x00000006 --rw-------   nobody   nobody kbf-acct-3 staff\n\
         m  65542 0x00000007 --rw-------   nobody   nobody     root  wheel\n\
         m  bad   0x00000004 --rw-------      701    staff\n\
         m  65540\n\
         Semaphores:\n\
         s  65539 0x00000005 --ra-------  kbf-acct-3    staff\n";
    let object = |kind, id: &str| Object {
        kind,
        id: id.to_owned(),
    };
    assert_eq!(
        owned(listing, "kbf-acct-3", 701),
        [
            object("-q", "65536"),
            object("-m", "65538"),
            object("-m", "65541"),
            object("-s", "65539"),
        ]
    );
    assert!(owned(listing, "kbf-acct-4", 702).is_empty());
}

/// Asserts that a uid's objects are removed by kind and id, that nothing is removed or
/// looked at again when there are none, and that the objects of others are left.
#[test]
fn the_uids_objects_are_removed_and_others_are_left() {
    let rig = Rig::new("ipc-remove", 1, 0);
    rig.fake.add_ipc('m', 100, "kbf-acct-3", "kbf-acct-3");
    rig.fake.add_ipc('s', 101, "702", "702");
    rig.fake.add_ipc('q', 102, "root", "root");
    remove(&*rig.fake, "kbf-acct-3", 701).unwrap();
    assert_eq!(
        rig.fake.take_calls(),
        [
            "command /usr/bin/ipcs -a",
            "command /usr/bin/ipcrm -m 100",
            "command /usr/bin/ipcs -a",
        ]
    );
    let left: Vec<u64> = rig.fake.state().ipc.iter().map(|o| o.id).collect();
    assert_eq!(left, [101, 102]);

    remove(&*rig.fake, "kbf-acct-3", 701).unwrap();
    assert_eq!(rig.fake.take_calls(), ["command /usr/bin/ipcs -a"]);
}

/// Asserts that an `ipcrm` that fails decides nothing: it is logged, and the step fails only
/// if the object is still listed afterwards, naming it; and the listing that fails is the
/// step's failure, whether it could not be run or exited non-zero.
#[test]
fn an_ipcrm_that_fails_decides_nothing_and_the_listing_decides() {
    let rig = Rig::new("ipc-fail", 1, 0);
    rig.fake.add_ipc('m', 100, "kbf-acct-3", "kbf-acct-3");
    rig.fake.state().ipcrm_fails = true;
    let why = remove(&*rig.fake, "kbf-acct-3", 701).unwrap_err();
    assert_eq!(
        why,
        "IPC objects of uid 701 remain after their removal: -m 100"
    );
    let text = rig.log.text();
    assert!(
        text.contains("ipcrm -m 100 (the outcome decides): ipcrm: not permitted"),
        "{text}"
    );

    rig.fake.fail("command /usr/bin/ipcrm -m 100");
    let why = remove(&*rig.fake, "kbf-acct-3", 701).unwrap_err();
    assert_eq!(
        why,
        "IPC objects of uid 701 remain after their removal: -m 100"
    );
    let text = rig.log.text();
    assert!(
        text.contains("ipcrm -m 100 (the outcome decides): command /usr/bin/ipcrm -m 100 failed"),
        "{text}"
    );

    rig.fake.heal("command /usr/bin/ipcrm -m 100");
    rig.fake.state().ipcrm_fails = false;
    rig.fake.state().ipcs_exits_nonzero = true;
    assert_eq!(
        remove(&*rig.fake, "kbf-acct-3", 701).unwrap_err(),
        "ipcs failed: ipcs: kernel is not answering"
    );
    assert_eq!(
        left(&*rig.fake, "kbf-acct-3", 701).unwrap_err(),
        "ipcs failed: ipcs: kernel is not answering"
    );
    rig.fake.state().ipcs_exits_nonzero = false;
    rig.fake.fail("command");
    assert_eq!(
        left(&*rig.fake, "kbf-acct-3", 701).unwrap_err(),
        "running ipcs: command /usr/bin/ipcs -a failed"
    );
}
