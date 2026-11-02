use super::*;

fn answer(code: Option<i32>, stderr: &str) -> io::Result<bool> {
    printed(&PrintAnswer {
        code,
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
