//! The tests of the module: a fake host ([`fake`]), a pool over it ([`rig`]), and the tests
//! that belong to no one part of the code.

pub(super) mod fake;
pub(super) mod os;
pub(super) mod rig;

use std::io;
use std::path::PathBuf;

use super::ledger::valid_step;
use super::{Account, AccountError, Gone, Held, Step};

/// Asserts the words of the steps and the items of the proof, as the ledger's `held` record
/// keeps them (kebab-case, which the ledger accepts).
#[test]
fn steps_and_items_are_spelled_as_the_ledger_keeps_them() {
    let items = [
        (Gone::Record, "record"),
        (Gone::Processes, "processes"),
        (Gone::Home, "home"),
        (Gone::Folders, "folders"),
        (Gone::Places, "places"),
        (Gone::Ipc, "ipc"),
    ];
    for (item, word) in items {
        assert_eq!(item.to_string(), word);
        assert_eq!(Step::Proof(item).to_string(), format!("proof-{word}"));
        assert!(valid_step(&Step::Proof(item).to_string()));
    }
    let steps = [
        (Step::Kill, "kill"),
        (Step::Domains, "domains"),
        (Step::Schedules, "schedules"),
        (Step::Ipc, "ipc"),
        (Step::Sweep, "sweep"),
        (Step::Record, "record"),
        (Step::Home, "home"),
        (Step::Folder, "folder"),
        (Step::Ledger, "ledger"),
    ];
    for (step, word) in steps {
        assert_eq!(step.to_string(), word);
        assert!(valid_step(word));
    }
}

/// Asserts what each refusal of the pool says: the ledger it names, the account and the step
/// of a making, the uids that are not free, the job id, and what the host was asked.
#[test]
fn the_errors_name_what_failed() {
    let ledger = AccountError::Ledger {
        path: PathBuf::from("/var/state/accounts"),
        why: "line 3: bad".to_owned(),
    };
    assert_eq!(
        ledger.to_string(),
        "the account ledger in /var/state/accounts: line 3: bad"
    );
    let make = AccountError::Make {
        account: "kbf-acct-4".to_owned(),
        step: Step::Proof(Gone::Home),
        why: "the home exists".to_owned(),
    };
    assert_eq!(
        make.to_string(),
        "making account kbf-acct-4: proof-home: the home exists"
    );
    let free = AccountError::NoFreeUid {
        range: "600-601".to_owned(),
        reasons: vec![
            "600: a user record has it".to_owned(),
            "601: held by kbf-acct-2".to_owned(),
        ],
    };
    assert_eq!(
        free.to_string(),
        "no free uid in 600-601: 600: a user record has it; 601: held by kbf-acct-2"
    );
    let host = AccountError::Host {
        what: "listing the accounts",
        source: io::Error::other("refused"),
    };
    assert_eq!(host.to_string(), "listing the accounts: refused");
    let held = Held {
        account: Account {
            name: "kbf-acct-4".to_owned(),
            uid: 604,
            gid: 20,
            generated_uid: super::generated_uid(604),
        },
        step: Step::Folder,
        why: "stuck".to_owned(),
    };
    assert_eq!(
        held.to_string(),
        "account kbf-acct-4 (uid 604) is held at folder: stuck"
    );
}
