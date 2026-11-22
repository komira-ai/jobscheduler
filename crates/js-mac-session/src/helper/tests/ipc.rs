//! The System V IPC objects of the uid in the deletion of its account: each is removed by
//! its kind and id, an object that stays (or a listing that cannot be had) fails the delete,
//! and the lease stays live with its uid held.

use super::*;
use crate::autologin::CommandOutput;

const IPCRM_Q: &str = "/usr/bin/ipcrm -q 65536";

/// A rig of ONE uid holding `lease`, with objects of the account (owned by its name, created
/// by its uid), and one of root's.
fn with_objects(name: &str, lease: &str) -> (Rig, u32) {
    let (rig, uid) = one_uid(name, lease);
    let owner = crate::lease::user_name(parse_lease(lease).unwrap());
    {
        let mut state = rig.host.state();
        state.ipc.insert(65536, ('q', owner));
        state.ipc.insert(65537, ('m', uid.to_string()));
        state.ipc.insert(65538, ('s', "root".to_owned()));
    }
    (rig, uid)
}

/// Catches: an object left of the uid (shared memory outlives every process), one removed
/// with the wrong kind or id, one of another owner removed, or a relisting that is skipped
/// (the removal is judged by what `ipcs` lists afterwards, and the proof lists once more).
#[test]
fn the_uid_s_ipc_objects_are_removed_by_kind_and_id_and_no_others() {
    let (rig, _) = with_objects("ipc-removed", "90.1");
    assert_eq!(rig.helper.user_delete("90.1"), Ok(true));
    assert_eq!(
        rig.host.state().ipc.keys().copied().collect::<Vec<_>>(),
        [65538]
    );
    let calls = log(&rig);
    assert!(calls.contains(&format!("command {IPCRM_Q}")), "{calls:#?}");
    assert!(calls.contains(&"command /usr/bin/ipcrm -m 65537".to_owned()));
    assert!(!calls.iter().any(|c| c.contains("65538")), "{calls:#?}");
    // Listed to find them, to judge the removal, and once more for the proof.
    let lists = calls
        .iter()
        .filter(|c| c.starts_with(&format!("command {IPCS} ")))
        .count();
    assert_eq!(lists, 3, "{calls:#?}");
}

/// Catches: an object that `ipcrm` refuses (or cannot be run for) taken as removed: the
/// outcome decides, so the delete fails naming it, the lease stays live and its uid held,
/// and the delete repeated once it can be removed completes.
#[test]
fn an_object_that_cannot_be_removed_fails_the_delete_and_holds_the_uid() {
    let (rig, uid) = with_objects("ipc-stuck", "91.1");
    rig.host.state().ipc_stuck.insert(65536);
    assert_eq!(
        rig.helper.user_delete("91.1"),
        Err(format!("IPC objects of uid {uid} remain: -q 65536"))
    );
    assert!(rig.helper.lock().held_uids().contains(&uid));
    uid_held(&rig);
    rig.host.state().ipc_stuck.clear();
    assert_eq!(rig.helper.user_delete("91.1"), Ok(true));
    assert_eq!(
        rig.host.state().ipc.keys().copied().collect::<Vec<_>>(),
        [65538]
    );

    // `ipcrm` cannot be run at all: the first of its calls fails (the listing is the
    // first call, the removals follow, then the second listing).
    let (rig, uid) = with_objects("ipc-no-ipcrm", "91.2");
    rig.host
        .state()
        .fail_script
        .insert("command", [false, true, false, false].into());
    let error = rig.helper.user_delete("91.2").unwrap_err();
    assert!(
        error.starts_with(&format!("IPC objects of uid {uid} remain: ")),
        "{error}"
    );
    assert!(rig.helper.lock().held_uids().contains(&uid));
}

/// Catches: a listing that fails (the program refuses, or cannot be run) read as no objects
/// (shared memory would be left for the next owner of the uid), or a failure that does not
/// say why.
#[test]
fn a_listing_that_fails_refuses_the_delete_naming_why() {
    let (rig, uid) = one_uid("ipc-list", "92.1");
    rig.host.state().answers.insert(
        format!("{IPCS} -a"),
        CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "ipcs: not today\n".to_owned(),
        },
    );
    assert_eq!(
        rig.helper.user_delete("92.1"),
        Err(format!(
            "listing the IPC objects of uid {uid}: ipcs failed: ipcs: not today"
        ))
    );
    rig.host.state().answers.clear();
    rig.host.state().fail.insert("command");
    assert_eq!(
        rig.helper.user_delete("92.1"),
        Err(format!(
            "listing the IPC objects of uid {uid}: command failed"
        ))
    );
    assert!(rig.helper.lock().held_uids().contains(&uid));
}
