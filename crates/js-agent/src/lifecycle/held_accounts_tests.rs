//! What a helper's answer means for an account, and the set of held leases.

use std::io;

use js_mac_session::client::ClientError;
use js_mac_session::helper::RESET_NEEDED;
use js_mac_session::proto::Reply;

use super::{Ending, HeldAccounts, Retried, ending, gone};

fn refused(why: &str) -> ClientError {
    ClientError::Refused(why.to_owned())
}

/// Asserts that only the two refusals that say the lease has no user, or that its user was
/// deleted, are the end that was wanted: a refusal that names a step that did not hold, a
/// socket error and an answer of another shape are not. Proves the texts the agent has always
/// taken for success are still taken for it.
#[test]
fn only_a_refusal_that_the_user_is_gone_is_gone() {
    assert!(gone(&refused("lease 7.0 has no user")));
    assert!(gone(&refused("the user was deleted")));
    assert!(!gone(&refused("not shown gone: a record has uid 612")));
    assert!(!gone(&ClientError::Io(io::Error::other("hung up"))));
    assert!(!gone(&ClientError::Unexpected(Reply::Killed)));
}

/// Asserts what each failure of `kill-uid` or `user-delete` means for the account: a refusal
/// that names a step is a hold, the refusals that say nothing is left are gone, and a failed
/// socket, a reply of another shape and a helper that needs its boot reset again decide
/// nothing. Proves a socket error is not taken for a hold, and a held account is never taken
/// for a gone one.
#[test]
fn a_refusal_holds_the_account_and_nothing_else_does() {
    for held in [
        "not shown gone: a record has uid 612",
        "kill-uid: a process of uid 612 is left",
    ] {
        assert_eq!(ending(&refused(held)), Ending::Held, "{held}");
    }
    for gone in ["lease 7.0 has no user", "the user was deleted"] {
        assert_eq!(ending(&refused(gone)), Ending::Gone, "{gone}");
    }
    for failed in [
        refused(RESET_NEEDED),
        ClientError::Io(io::Error::other("hung up")),
        ClientError::Unexpected(Reply::Killed),
    ] {
        assert_eq!(ending(&failed), Ending::Failed, "{failed}");
    }
}

/// Asserts that holding counts leases once each, in the set's order, and that releasing gives
/// one up: a lease held twice is one account, and the count after a release is the accounts
/// left. Catches a count that grows with each hold of the same lease, which would taint a
/// host for one account.
#[test]
fn a_lease_is_held_once_however_often_it_is_held() {
    let mut held = HeldAccounts::default();
    assert_eq!(held.count(), 0);
    assert_eq!(held.hold("7.1"), 1);
    assert_eq!(held.hold("7.1"), 1);
    assert_eq!(held.hold("7.0"), 2);
    assert_eq!(held.count(), 2);
    held.release("7.1");
    held.release("7.9");
    assert_eq!(held.count(), 1);
}

/// Asserts that the error a removal gives for a held account carries what the helper said and
/// the accounts held now, this one among them, and that it is the kind the lifecycle reads as a
/// held account and not as a failed removal.
#[test]
fn the_error_of_a_held_account_says_how_many_are_held() {
    let mut held = HeldAccounts::default();
    held.hold("7.0");
    let error = held.error("user-delete: kbf-mac-session refused: a record has uid 612");
    assert_eq!(error.held_accounts(), Some(1));
    assert_eq!(
        error.0,
        "user-delete: kbf-mac-session refused: a record has uid 612"
    );
    held.hold("7.1");
    assert_eq!(held.error("again").held_accounts(), Some(2));
}

/// Asserts that a retry ends every lease the helper now shows gone and keeps the others held
/// with the reason each gave, in the order the leases sort, and that a second retry asks only
/// those still held. Catches a retry that gives up a lease whose end failed (its uid would be
/// forgotten while the helper still holds it), and one that stops at the first failure.
#[test]
fn a_retry_gives_up_what_ended_and_keeps_what_did_not() {
    let mut held = HeldAccounts::default();
    for lease in ["7.2", "7.0", "7.1"] {
        held.hold(lease);
    }
    let mut asked = Vec::new();
    let retried = held.retry(|lease| {
        asked.push(lease.to_owned());
        match lease {
            "7.1" => Err("user-delete: still there".to_owned()),
            _ => Ok(()),
        }
    });
    assert_eq!(asked, ["7.0", "7.1", "7.2"]);
    assert_eq!(
        retried,
        Retried {
            ended: vec!["7.0".to_owned(), "7.2".to_owned()],
            failed: vec![("7.1".to_owned(), "user-delete: still there".to_owned())],
        }
    );
    assert_eq!(held.count(), 1);
    let mut asked = Vec::new();
    let second = held.retry(|lease| {
        asked.push(lease.to_owned());
        Ok(())
    });
    assert_eq!(asked, ["7.1"]);
    assert_eq!(second.ended, ["7.1"]);
    assert_eq!(held.count(), 0);
    assert_eq!(
        held.retry(|_| Err("never asked".to_owned())),
        Retried::default()
    );
}
