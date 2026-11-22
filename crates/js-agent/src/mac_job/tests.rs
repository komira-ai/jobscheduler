//! The command line of `__mac-job`, and its refusal off macOS.

use std::ffi::OsString;
use std::path::PathBuf;

use super::profile::Kind;
use super::{Args, AuditSession, VERB, parse, run, status};

fn words(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

const MINIMAL: [&str; 8] = [
    "--job", "j.json", "--user", "u", "--root", "/r", "--out", "/o",
];

#[test]
fn the_minimal_command_line_takes_a_new_audit_session_and_adds_no_rules() {
    let args = parse(&words(&MINIMAL)).expect("parses");
    assert_eq!(
        args,
        Args {
            job: PathBuf::from("j.json"),
            user: "u".to_owned(),
            root: PathBuf::from("/r"),
            out: PathBuf::from("/o"),
            profile: Kind::ConfinedWrites,
            audit_session: AuditSession::New,
            profile_add: None,
        }
    );
}

#[test]
fn the_audit_session_may_be_inherited_the_profile_chosen_and_rules_added() {
    let mut list = MINIMAL.to_vec();
    list.extend([
        "--audit-session",
        "inherit",
        "--profile-add",
        "more.sb",
        "--profile",
        "deny-default",
    ]);
    let args = parse(&words(&list)).expect("parses");
    assert_eq!(args.profile, Kind::DenyDefault);
    assert_eq!(args.audit_session, AuditSession::Inherit);
    assert_eq!(args.profile_add, Some(PathBuf::from("more.sb")));
}

#[test]
fn an_unknown_audit_session_value_is_refused() {
    let mut list = MINIMAL.to_vec();
    list.extend(["--audit-session", "shared"]);
    assert!(parse(&words(&list)).is_err());
}

#[test]
fn a_missing_flag_is_a_usage_error_naming_it() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        &words(&["js-agent", VERB, "--job", "j"]),
        &mut out,
        &mut err,
    );
    assert_eq!(code, std::process::ExitCode::from(status::USAGE));
    let err = String::from_utf8(err).expect("utf-8");
    assert!(err.contains("--user"), "{err}");
    assert!(out.is_empty());
}

#[test]
fn a_command_line_without_the_mode_word_is_a_usage_error() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(&["js-agent"]), &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(status::USAGE));
    assert!(!err.is_empty());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_a_well_formed_command_line_is_refused_by_name() {
    let mut list = vec!["js-agent", VERB];
    list.extend(MINIMAL);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(&list), &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(status::USAGE));
    assert_eq!(
        String::from_utf8(err).expect("utf-8"),
        "js-agent: __mac-job runs on macOS only\n"
    );
    assert!(out.is_empty());
}

#[test]
fn the_agent_answers_the_mode_word_before_anything_else() {
    let install = || -> std::io::Result<crate::SystemSleeper> {
        Err(std::io::Error::other("the mode installs no signal handler"))
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::main_with(&install, words(&["js-agent", VERB]), &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(status::USAGE));
    let err = String::from_utf8(err).expect("utf-8");
    assert!(err.contains("--job"), "{err}");
}
