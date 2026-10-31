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

/// The wrapper's own failures: an empty command is a usage error, a program that does not
/// exist and a count file that cannot be written are failures naming what failed (the test
/// binary itself, asked to list its tests, is the program that runs).
#[test]
fn the_wrapper_reports_what_it_could_not_do() {
    let dir = std::env::temp_dir().join(format!("xtask-count-run-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    let file = dir.join("n.count");
    assert!(matches!(run(&file, &[]), Err(Error::Usage(_))));
    let missing = run(&file, &[dir.join("no-such-program").into_os_string()]);
    assert!(
        matches!(&missing, Err(e) if e.to_string().contains("cannot start")),
        "{missing:?}"
    );
    assert!(!file.exists());
    let me = std::env::current_exe()
        .expect("test binary")
        .into_os_string();
    let list = vec![me, OsString::from("--list")];
    let unwritable = run(&dir.join("no-such-dir/n.count"), &list);
    assert!(
        matches!(&unwritable, Err(e) if e.to_string().contains("cannot write")),
        "{unwritable:?}"
    );
    assert_eq!(run(&file, &list), Ok("0 tests ran".to_owned()));
    assert_eq!(std::fs::read_to_string(&file).expect("count"), "0\n");
    let _ = std::fs::remove_dir_all(&dir);
}
