//! The program's dispatch of `js-lease-server job-store` (its commands are tested in
//! `src/cutover_tests.rs`).

use std::ffi::OsString;
use std::process::ExitCode;

/// Asserts that the program answers the verb `job-store` itself, before the server's flags are
/// read: its help is printed on standard output with status 0, where the server would refuse
/// the word as an unknown argument with status 2. It catches the verb not being dispatched.
#[test]
fn the_program_answers_the_job_store_verb_before_its_own_flags() {
    let args: Vec<OsString> = ["js-lease-server", "job-store", "--help"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_lease_server::run(args, &mut out, &mut err);
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
    assert_eq!(String::from_utf8(err).unwrap(), "");
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("Usage: js-lease-server job-store <COMMAND>"),
        "{out}"
    );
}
