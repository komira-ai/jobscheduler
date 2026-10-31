use super::*;

/// Catches a summary line misread: the passed count is taken from `ok.` and `FAILED.`
/// lines wherever it sits among the fields, and any other line counts nothing.
#[test]
fn the_passed_count_is_read_from_a_summary_line_only() {
    assert_eq!(
        passed_in("test result: ok. 12 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out"),
        Some(12)
    );
    assert_eq!(
        passed_in("test result: FAILED. 4 passed; 2 failed; 0 ignored"),
        Some(4)
    );
    assert_eq!(passed_in("test result: ok. 0 passed; 0 failed"), Some(0));
    for other in [
        "",
        "running 3 tests",
        "test a::b ... ok",
        "3 passed; 0 failed",
        "test result: ok. many passed; 0 failed",
        "test result: ok",
    ] {
        assert_eq!(passed_in(other), None, "{other:?}");
    }
}

/// Catches a failed child's code lost or turned into success.
#[test]
fn a_failed_child_keeps_its_exit_code() {
    use std::os::unix::process::ExitStatusExt;
    let code = |raw: i32| exit_code(&std::process::ExitStatus::from_raw(raw));
    assert_eq!(code(101 << 8), 101);
    assert_eq!(code(9), 137);
}
