//! The `full-disk-access` verb: the helper's answer for each way a listing of the privacy
//! database's directory can end, and that it is answered before the boot reset.

use std::io;

use super::*;
use crate::proto::FullDiskAccess;

/// What the helper answers to the verb, as the reply.
fn ask(rig: &Rig) -> Reply {
    match rig.helper.handle(Request::FullDiskAccess, Vec::new()) {
        Outcome::Reply(reply) => reply,
        other => panic!("not one reply: {other:?}"),
    }
}

/// Asserts that a listing that works answers `granted`, a refusal for want of permission
/// answers `not-granted`, and any other failure answers `undetermined`; each asks the
/// host to list the privacy directory once and does nothing else. Catches: a verb that
/// answers from a cached state (a grant given or taken away after the helper started
/// would go unseen), or maps a failure that is not a refusal to `not-granted`.
#[test]
fn each_way_a_listing_ends_has_its_answer() {
    let rig = rig("fda-states", 1);
    for (kind, want) in [
        (None, FullDiskAccess::Granted),
        (
            Some(io::ErrorKind::PermissionDenied),
            FullDiskAccess::NotGranted,
        ),
        (Some(io::ErrorKind::NotFound), FullDiskAccess::Undetermined),
    ] {
        rig.host.state().privacy_dir = kind;
        assert_eq!(ask(&rig), Reply::FullDiskAccess { state: want });
        assert_eq!(log(&rig), ["list_privacy_dir"]);
    }
}

/// Asserts that the verb is answered on a helper whose boot reset has not run, and that
/// it touches nothing but the privacy directory (no lease user is made, no ledger line
/// written). Catches: the verb refused as `RESET_NEEDED`, which would leave the agent's
/// start-up question unanswered on a Mac that has just restarted.
#[test]
fn the_verb_is_answered_before_the_boot_reset() {
    let rig = rig_with("fda-early", UidRange::new(me(), me()).unwrap(), None);
    rig.host.state().privacy_dir = Some(io::ErrorKind::PermissionDenied);
    assert_eq!(
        ask(&rig),
        Reply::FullDiskAccess {
            state: FullDiskAccess::NotGranted
        }
    );
    assert_eq!(log(&rig), ["list_privacy_dir"]);
    let early = Request::UserCreate {
        lease: "1.1".to_owned(),
        grant: None,
    };
    assert!(matches!(
        rig.helper.handle(early, Vec::new()),
        Outcome::Reply(Reply::Refused { reason }) if reason == RESET_NEEDED
    ));
}

/// Asserts that a request carrying descriptors is refused as any other verb is. Catches:
/// the verb answering a request that smuggles descriptors in (only `run` and `role-run`
/// take them).
#[test]
fn the_verb_takes_no_descriptors() {
    let rig = rig("fda-fds", 1);
    let fd = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    assert!(matches!(
        rig.helper.handle(Request::FullDiskAccess, vec![fd]),
        Outcome::Reply(Reply::Refused { reason }) if reason.contains("take descriptors")
    ));
}
