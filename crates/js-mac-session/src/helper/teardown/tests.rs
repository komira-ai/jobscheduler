use super::*;
use crate::testing::{EMPTY_DOMAIN, found};

fn answer(code: Option<i32>, stderr: &str) -> io::Result<bool> {
    printed(&PrintAnswer {
        code,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    })
}

/// Asserts that `launchctl print` is read by whether it printed, never by its exit
/// status: exit 0 is a domain that prints; every other exit status (launchd's varying
/// answers for a domain it does not have, 112, 113 and 125 as macOS 26 has given them
/// for the `gui/<uid>` of a user that never logged in, and any other) is one that does
/// not, whatever the words; a signal is no answer, an error naming what launchctl said.
/// It catches an exit status matched as the only "not found" (125 refused every
/// teardown on a Mac) and a killed `launchctl` read as a domain that is gone.
#[test]
fn print_is_read_as_printed_or_not_and_a_signal_is_no_answer() {
    assert!(answer(Some(0), "").unwrap());
    for (code, stderr) in [
        (
            112,
            "Bad request.\nCould not find domain for user gui: 59125\n",
        ),
        (113, "Could not find domain for user: 501\n"),
        (
            125,
            "Could not print domain: 125: Domain does not support specified action\n",
        ),
        (1, "Operation not permitted\n"),
        (5, ""),
    ] {
        assert!(!answer(Some(code), stderr).unwrap(), "{code} {stderr:?}");
    }
    assert_eq!(
        answer(None, "killed\n").unwrap_err().to_string(),
        "launchctl print was ended by a signal: killed"
    );
}

/// Asserts which domains a teardown judges: only `user/<uid>` for a uid with no GUI
/// login session, and `gui/<uid>` first, then `user/<uid>`, for one with a session.
#[test]
fn the_gui_domain_is_judged_only_with_a_gui_session() {
    assert_eq!(domains(7, false), ["user/7"]);
    assert_eq!(domains(7, true), ["gui/7", "user/7"]);
}

/// Asserts that the text `launchctl print user/<uid>` prints on macOS 27 for a user
/// domain that holds nothing (the exact text of an idle lease user, of a uid after its
/// lease, and of a uid with no account) is read as holding nothing, and that exit 0
/// with it is a domain that is gone, while the same text with any other exit status is
/// too, and a signal is still no answer. Catches the judgement that a domain printing
/// at exit 0 always stays, which never let a lease end.
#[test]
fn an_empty_user_domain_printed_at_exit_0_is_gone() {
    assert!(holds_nothing(EMPTY_DOMAIN));
    let mut empty = PrintAnswer {
        code: Some(0),
        stdout: EMPTY_DOMAIN.to_owned(),
        stderr: String::new(),
    };
    assert!(!printed(&empty).unwrap());
    empty.code = Some(125);
    assert!(!printed(&empty).unwrap());
    empty.code = None;
    assert!(printed(&empty).is_err());
    assert!(printed(&found()).unwrap());
}

/// Asserts that a domain stays when its own `services` block or its own `unmanaged
/// processes` block holds anything, when either block cannot be found, and when the
/// text is empty. Catches an emptiness check that reads only one of the two blocks or
/// reads a missing block as an empty one (a domain launchd printed in a shape not known
/// would be freed).
#[test]
fn a_domain_with_a_service_or_an_unmanaged_process_or_no_blocks_stays() {
    let services = EMPTY_DOMAIN.replace(
        "\tservices = {\n\t}",
        "\tservices = {\n\t\t 123 - com.apple.job\n\t}",
    );
    let unmanaged = EMPTY_DOMAIN.replace(
        "\tunmanaged processes = {\n\t}",
        "\tunmanaged processes = {\n\t\t 4242 - sleep\n\t}",
    );
    let no_services = EMPTY_DOMAIN.replace("\tservices = {\n\t}\n", "");
    let no_unmanaged = EMPTY_DOMAIN.replace("\tunmanaged processes = {\n\t}\n", "");
    assert_ne!(services, EMPTY_DOMAIN);
    assert_ne!(unmanaged, EMPTY_DOMAIN);
    assert_ne!(no_services, EMPTY_DOMAIN);
    assert_ne!(no_unmanaged, EMPTY_DOMAIN);
    for text in [&services, &unmanaged, &no_services, &no_unmanaged, ""] {
        assert!(!holds_nothing(text), "{text:?}");
        let answer = PrintAnswer {
            code: Some(0),
            stdout: (*text).to_owned(),
            stderr: String::new(),
        };
        assert!(printed(&answer).unwrap(), "{text:?}");
    }
}

/// Asserts that only the domain's own blocks, one tab deep, are read: an empty
/// `services`-like block inside an endpoint does not make a domain whose own `services`
/// holds a service empty, a populated one inside an endpoint does not keep a domain
/// whose own blocks are empty, and a text with such nested blocks only (no block of the
/// domain's own) stays. Catches a search that takes the first line mentioning
/// `services = {` at any depth.
#[test]
fn only_the_domains_own_blocks_are_read_not_an_endpoints() {
    let nested_empty =
        "\tendpoints = {\n\t\tcom.apple.x = {\n\t\t\tservices = {\n\t\t\t}\n\t\t}\n\t}\n";
    let nested_full = "\tendpoints = {\n\t\tcom.apple.x = {\n\t\t\tservices = {\n\t\t\t\ta = b\n\t\t\t}\n\t\t}\n\t}\n";
    let busy = format!(
        "user/1 = {{\n{nested_empty}\tservices = {{\n\t\t1 - job\n\t}}\n\tunmanaged processes = {{\n\t}}\n}}\n"
    );
    assert!(!holds_nothing(&busy));
    let idle = format!(
        "user/1 = {{\n{nested_full}\tservices = {{\n\t}}\n\tunmanaged processes = {{\n\t}}\n}}\n"
    );
    assert!(holds_nothing(&idle));
    let nested_only = format!("user/1 = {{\n{nested_empty}}}\n");
    assert!(!holds_nothing(&nested_only));
}
