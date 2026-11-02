use super::*;

fn answer(code: Option<i32>, stderr: &str) -> io::Result<bool> {
    launchctl_print_answer(&PrintAnswer {
        code,
        stderr: stderr.to_owned(),
    })
}

/// Asserts that only `launchctl print`'s documented answers decide: exit 0 is a domain
/// that exists; exit 112 (what macOS 26 answers for the `gui/<uid>` of a user that never
/// logged in, its text here as it was printed) or 113, each with "Could not find
/// domain", one that does not; every other answer (a refusal, another exit with the same
/// words, 112 or 113 without them, a signal) is an error naming what launchctl said. It
/// catches launchd's not-found answer for a pool user's GUI domain read as a failure,
/// which refused every teardown, and a failed look at launchd read as a domain that is
/// gone, which would let a teardown free a uid launchd still serves.
#[test]
fn only_launchds_not_found_answer_is_absence() {
    assert!(answer(Some(0), "").unwrap());
    for (code, stderr) in [
        (
            112,
            "Bad request.\nCould not find domain for user gui: 59125\n",
        ),
        (
            112,
            "Bad request.\nCould not find domain for user user: 501\n",
        ),
        (113, "Could not find domain for user: 501\n"),
    ] {
        assert!(!answer(Some(code), stderr).unwrap(), "{code} {stderr:?}");
    }
    for (code, stderr, want) in [
        (
            Some(1),
            "Operation not permitted\n",
            "launchctl print exited 1: Operation not permitted",
        ),
        (
            Some(1),
            "Could not find domain for user gui: 501\n",
            "launchctl print exited 1: Could not find domain for user gui: 501",
        ),
        (
            Some(112),
            "Bad request.\n",
            "launchctl print exited 112: Bad request.",
        ),
        (
            Some(113),
            "Bad request.\n",
            "launchctl print exited 113: Bad request.",
        ),
        (Some(5), "", "launchctl print exited 5: "),
        (None, "", "launchctl print was ended by a signal: "),
    ] {
        assert_eq!(answer(code, stderr).unwrap_err().to_string(), want);
    }
}
