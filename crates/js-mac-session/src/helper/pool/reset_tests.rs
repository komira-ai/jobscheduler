//! A reset in place that cannot read what it must check: the account, or its record, cannot
//! be looked up or is not there. Each leaves the slot broken, naming why, and lends it to no
//! one until a later reset succeeds.

use super::tests::slots;
use crate::helper::tests::{log, rig};

/// What `user-delete` answers when the reset of slot 0's user fails for `why`.
fn broken(why: &str) -> String {
    format!(
        "slot 0's user kbf-lease-pool-0 could not be reset in place, so the slot is broken \
         and lent to no one until pool-create resets it again: {why}"
    )
}

/// Catches a reset that takes a directory it could not ask as an answer: the account's
/// lookup failing breaks the slot with the reason, and the next `pool-create` that can look
/// frees it, making the account again if it is gone.
#[test]
fn a_reset_whose_account_cannot_be_looked_up_breaks_the_slot() {
    let rig = rig("pool-reset-account", 1);
    rig.helper.user_create("91.1", None).unwrap();
    rig.host
        .state()
        .fail_script
        .insert("account", [true].into());
    let error = rig.helper.user_delete("91.1").unwrap_err();
    assert_eq!(
        error,
        broken("looking up user kbf-lease-pool-0: account failed")
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));

    // A later reset that can look frees the slot; an account gone by then is made again.
    rig.host.state().users.remove("kbf-lease-pool-0");
    log(&rig);
    assert_eq!(rig.helper.pool_create().free, slots(&[0]));
    let calls = log(&rig);
    assert!(
        calls.iter().any(|c| c == "create_user kbf-lease-pool-0"),
        "{calls:#?}"
    );
}

/// Catches a reset that sets a record back without reading it, or reads a record that is
/// not there as the slot's: the record's lookup failing, then no record answered for an
/// account that is there, each break the slot with the reason, and no attribute is
/// written; the next reset that can read it frees the slot.
#[test]
fn a_reset_whose_record_cannot_be_read_or_is_not_there_breaks_the_slot() {
    let rig = rig("pool-reset-record", 1);
    rig.helper.user_create("92.1", None).unwrap();
    rig.host
        .state()
        .fail_script
        .insert("user_record", [true].into());
    let error = rig.helper.user_delete("92.1").unwrap_err();
    assert_eq!(
        error,
        broken("setting back the record of user kbf-lease-pool-0: user_record failed")
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));

    rig.host
        .state()
        .recordless
        .insert("kbf-lease-pool-0".to_owned());
    log(&rig);
    let created = rig.helper.pool_create();
    assert_eq!(
        created.errors,
        [format!(
            "slot 0: {}",
            broken("setting back the record of user kbf-lease-pool-0: there is no record")
        )]
    );
    let calls = log(&rig);
    assert!(
        !calls.iter().any(|c| c.starts_with("restore_user")),
        "{calls:#?}"
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));

    rig.host.state().recordless.clear();
    assert_eq!(rig.helper.pool_create().free, slots(&[0]));
}
