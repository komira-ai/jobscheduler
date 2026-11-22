//! Tests of how a uid's launchd domains are booted out and judged.

use std::sync::Arc;

use super::*;
use crate::mac_accounts::tests::fake::{Fake, Print};
use crate::mac_accounts::tests::rig::Rig;

fn answer(code: Option<i32>, stdout: &str) -> PrintAnswer {
    PrintAnswer {
        code,
        stdout: stdout.to_owned(),
        stderr: "stderr text ".to_owned(),
    }
}

/// Asserts that a uid's domains are its login session's and its user domain, in that order.
#[test]
fn the_domains_of_a_uid_are_its_session_and_its_user_domain() {
    assert_eq!(domains(701), ["gui/701".to_owned(), "user/701".to_owned()]);
}

/// Asserts that a domain that holds nothing is told from one that holds a service or an
/// unmanaged process, by its own top-level blocks and not an endpoint's or a nested one's,
/// and that a block that cannot be found is a domain that stays.
#[test]
fn a_domain_that_holds_nothing_is_one_with_empty_service_and_process_blocks() {
    let empty = "system = {\n\tservices = {\n\t}\n\tunmanaged processes = {\n\t}\n}\n";
    assert!(holds_nothing(empty));
    assert!(holds_nothing(
        "\tservices = {\n\t}  \n\tunmanaged processes = {\n\t}\n"
    ));
    let service = "\tservices = {\n\t\t12 - com.example\n\t}\n\tunmanaged processes = {\n\t}\n";
    assert!(!holds_nothing(service));
    let process = "\tservices = {\n\t}\n\tunmanaged processes = {\n\t\t5 foo\n\t}\n";
    assert!(!holds_nothing(process));
    let nested = "\tservices = {\n\t}\n\t\tunmanaged processes = {\n\t\t}\n";
    assert!(
        !holds_nothing(nested),
        "a nested block is not the domain's own"
    );
    assert!(!holds_nothing("\tservices = {\n\t}\n"));
    assert!(!holds_nothing(""));
    assert!(
        !holds_nothing("\tservices = {"),
        "a block cut at the end of the text"
    );
}

/// Asserts how a print is read: any non-zero exit is a domain that is not there (112, 113
/// and 125 are all seen), exit 0 is one that stays unless it holds nothing, and a signal is
/// no answer, so it is an error and never read as gone.
#[test]
fn a_print_is_read_by_its_exit_status_and_what_it_holds() {
    let empty = "\tservices = {\n\t}\n\tunmanaged processes = {\n\t}\n";
    let busy = "\tservices = {\n\t\t1 x\n\t}\n\tunmanaged processes = {\n\t}\n";
    for code in [112, 113, 125, 1] {
        assert!(!printed(&answer(Some(code), busy)).unwrap(), "{code}");
    }
    assert!(!printed(&answer(Some(0), empty)).unwrap());
    assert!(printed(&answer(Some(0), busy)).unwrap());
    let signalled = printed(&answer(None, "")).unwrap_err();
    assert_eq!(
        signalled.to_string(),
        "launchctl print was ended by a signal: stderr text"
    );
}

/// Asserts that every domain is booted out, a bootout that fails is returned with its domain
/// and why (and logged, as one that decides nothing), and one that works is not.
#[test]
fn every_domain_is_booted_out_and_a_failure_is_returned_with_its_reason() {
    let rig = Rig::new("bootout", 1, 0);
    rig.fake
        .state()
        .print
        .insert("user/701".to_owned(), Print::Stays);
    let domains = ["gui/701".to_owned(), "user/701".to_owned()];
    let refused = boot_out(&*rig.fake, &domains);
    assert_eq!(
        refused,
        [(
            "gui/701".to_owned(),
            "Boot-out failed: 5: Input/output error".to_owned()
        )]
    );
    assert_eq!(
        rig.fake.take_calls(),
        ["bootout gui/701", "bootout user/701"]
    );
    assert!(rig.fake.state().print.is_empty(), "user/701 was booted out");
    let text = rig.log.text();
    assert!(
        text.contains("launchctl bootout gui/701 (the outcome decides): Boot-out failed"),
        "{text}"
    );
}

/// Asserts which domains are reported as staying: one that prints (with why its bootout
/// failed, when it did), one that gave no answer or whose launchctl could not be run (with
/// why), and not one that is gone or empty.
#[test]
fn the_domains_that_stay_are_named_with_why() {
    let fake: Arc<Fake> = Fake::new();
    {
        let mut state = fake.state();
        state.print.insert("a".to_owned(), Print::Stays);
        state.print.insert("b".to_owned(), Print::Stays);
        state.print.insert("c".to_owned(), Print::Signal);
        state.print.insert("d".to_owned(), Print::Empty);
    }
    fake.fail("launchctl_print e");
    let domains: Vec<String> = ["a", "b", "c", "d", "gone", "e"]
        .map(str::to_owned)
        .to_vec();
    let refused = [("a".to_owned(), "no".to_owned())];
    assert_eq!(
        still_printing(&*fake, &domains, &refused),
        [
            "a (bootout: no)",
            "b",
            "c (launchctl print: launchctl print was ended by a signal: Could not find service)",
            "e (launchctl print: launchctl_print e failed)",
        ]
    );
}
