use super::*;

/// Asserts that only `launchctl print`'s documented answers decide: exit 0 is a domain
/// that exists, exit 113 with "Could not find domain" one that does not, and every
/// other answer (a refusal, another exit, 113 without the text, a signal) is an error
/// naming what launchctl said. It catches a failed look at launchd read as a domain
/// that is gone, which would let a teardown free a uid launchd still serves.
#[test]
fn only_launchds_not_found_answer_is_absence() {
    assert!(launchctl_print_answer(Some(0), "").unwrap());
    assert!(!launchctl_print_answer(Some(113), "Could not find domain for user: 501\n").unwrap());
    for (code, stderr, want) in [
        (
            Some(1),
            "Operation not permitted\n",
            "launchctl print exited 1: Operation not permitted",
        ),
        (
            Some(113),
            "Bad request.\n",
            "launchctl print exited 113: Bad request.",
        ),
        (Some(5), "", "launchctl print exited 5: "),
        (None, "", "launchctl print was ended by a signal: "),
    ] {
        assert_eq!(
            launchctl_print_answer(code, stderr)
                .unwrap_err()
                .to_string(),
            want
        );
    }
}
