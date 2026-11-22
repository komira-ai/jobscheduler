//! The command line of `__mac-job`, and its refusal off macOS.

use std::ffi::OsString;
use std::path::PathBuf;

use super::profile::Kind;
use super::{Args, AuditSession, Spawn, VERB, parse, run, status};
use crate::mac_accounts::Slots;

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
            job: Some(PathBuf::from("j.json")),
            job_inline: None,
            accounts: None,
            account_range: None,
            user: Some("u".to_owned()),
            ready: 1,
            spawn: Spawn::Ordinary,
            root: PathBuf::from("/r"),
            out: PathBuf::from("/o"),
            profile: Kind::ConfinedWrites,
            audit_session: AuditSession::New,
            profile_add: None,
            mount_provider: None,
            mount_provider_user: "0".to_owned(),
            mount_root: None,
            agent_id: "mac-job".to_owned(),
            put_ingest: None,
            put_user: "_tierstore".to_owned(),
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
    assert!(err.contains("--root"), "{err}");
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

#[test]
fn the_job_may_come_inline_and_exactly_one_of_the_two_is_needed() {
    let rest = ["--user", "u", "--root", "/r", "--out", "/o"];
    let mut inline = vec!["--job-inline", "{}"];
    inline.extend(rest);
    let args = parse(&words(&inline)).expect("parses");
    assert_eq!(args.job, None);
    assert_eq!(args.job_inline.as_deref(), Some("{}"));
    let mut both = vec!["--job", "j.json", "--job-inline", "{}"];
    both.extend(rest);
    assert!(parse(&words(&both)).is_err());
    assert!(parse(&words(&rest)).is_err());
}

#[test]
fn the_user_is_lent_by_the_pool_or_named_and_exactly_one_is_given() {
    let rest = ["--job", "j.json", "--root", "/r", "--out", "/o"];
    let mut pooled = vec![
        "--accounts",
        "/s",
        "--account-range",
        "700-799",
        "--ready",
        "3",
        "--spawn",
        "disclaimed",
    ];
    pooled.extend(rest);
    let args = parse(&words(&pooled)).expect("parses");
    assert_eq!(args.accounts, Some(PathBuf::from("/s")));
    assert_eq!(args.user, None);
    assert_eq!(args.ready, 3);
    assert_eq!(args.spawn, Spawn::Disclaimed);
    let mut both = vec![
        "--accounts",
        "/s",
        "--account-range",
        "700-799",
        "--user",
        "u",
    ];
    both.extend(rest);
    assert!(parse(&words(&both)).is_err());
    assert!(parse(&words(&rest)).is_err());
}

#[test]
fn the_mount_provider_and_its_root_come_together() {
    let mut list = MINIMAL.to_vec();
    list.extend([
        "--mount-provider",
        "/p.sock",
        "--mount-root",
        "/fs",
        "--mount-provider-user",
        "_tierstore",
        "--agent-id",
        "mac-1",
    ]);
    let args = parse(&words(&list)).expect("parses");
    assert_eq!(args.mount_provider, Some(PathBuf::from("/p.sock")));
    assert_eq!(args.mount_root, Some(PathBuf::from("/fs")));
    assert_eq!(args.mount_provider_user, "_tierstore");
    assert_eq!(args.agent_id, "mac-1");
    for alone in [["--mount-provider", "/p.sock"], ["--mount-root", "/fs"]] {
        let mut list = MINIMAL.to_vec();
        list.extend(alone);
        assert!(parse(&words(&list)).is_err(), "{alone:?}");
    }
}

#[test]
fn the_put_ingest_names_its_root_and_the_nodes_user() {
    let mut list = MINIMAL.to_vec();
    list.extend([
        "--put-ingest",
        "/usr/local/tierstore/put",
        "--put-user",
        "601",
    ]);
    let args = parse(&words(&list)).expect("parses");
    assert_eq!(
        args.put_ingest,
        Some(PathBuf::from("/usr/local/tierstore/put"))
    );
    assert_eq!(args.put_user, "601");
}

#[test]
fn only_the_mode_word_is_dispatched() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert!(super::dispatch(&words(&["js-agent", "cordon"]), &mut out, &mut err).is_none());
    assert!(super::dispatch(&words(&["js-agent"]), &mut out, &mut err).is_none());
    assert!(out.is_empty() && err.is_empty());
}

const POOLED: [&str; 6] = ["--job", "j.json", "--root", "/r", "--out", "/o"];

#[test]
fn the_account_range_is_the_slots_given_and_passed_to_the_pool() {
    let mut list = vec!["--accounts", "/s", "--account-range", "700-799"];
    list.extend(POOLED);
    let args = parse(&words(&list)).expect("parses");
    assert_eq!(
        args.account_range,
        Some(Slots::new(700, 100).expect("slots"))
    );
    assert_eq!(args.account_range.expect("given").range(), 700..=799);
}

#[test]
fn accounts_without_account_range_is_a_usage_error_naming_the_flag() {
    let mut list = vec!["js-agent", VERB, "--accounts", "/s"];
    list.extend(POOLED);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(&list), &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(status::USAGE));
    let err = String::from_utf8(err).expect("utf-8");
    assert!(err.contains("--account-range"), "{err}");
    assert!(out.is_empty());
}

#[test]
fn an_account_range_slots_may_not_take_is_a_usage_error_naming_it() {
    for (wrong, why) in [
        ("700", "is not <first>-<last>, two uids"),
        ("799-700", "the first uid, 799, is above the last, 700"),
        ("500-600", "is below 501"),
        ("700-60001", "is above 60000"),
    ] {
        let mut list = vec![
            "js-agent",
            VERB,
            "--accounts",
            "/s",
            "--account-range",
            wrong,
        ];
        list.extend(POOLED);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&words(&list), &mut out, &mut err);
        assert_eq!(code, std::process::ExitCode::from(status::USAGE), "{wrong}");
        let err = String::from_utf8(err).expect("utf-8");
        assert!(err.contains("--account-range"), "{err}");
        assert!(err.contains(why), "{err}");
        assert!(out.is_empty());
    }
}

#[test]
fn an_account_range_is_for_a_pool_and_not_for_a_named_user() {
    let mut list = MINIMAL.to_vec();
    list.extend(["--account-range", "700-799"]);
    assert!(parse(&words(&list)).is_err());
}

#[test]
fn the_start_mode_takes_exactly_its_start_file_and_refuses_off_macos() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for bad in [
        vec!["js-agent", super::start::VERB],
        vec!["js-agent", super::start::VERB, "--start"],
        vec!["js-agent", super::start::VERB, "--file", "f"],
        vec!["js-agent", super::start::VERB, "--start", "f", "extra"],
    ] {
        err.clear();
        let code = super::dispatch(&words(&bad), &mut out, &mut err);
        assert_eq!(
            code,
            Some(std::process::ExitCode::from(status::USAGE)),
            "{bad:?}"
        );
        assert_eq!(
            String::from_utf8(err.clone()).expect("utf-8"),
            "usage: js-agent __mac-job-start --start <file>\n"
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        err.clear();
        let code = super::dispatch(
            &words(&["js-agent", super::start::VERB, "--start", "f"]),
            &mut out,
            &mut err,
        );
        assert_eq!(code, Some(std::process::ExitCode::from(status::USAGE)));
        assert_eq!(
            String::from_utf8(err.clone()).expect("utf-8"),
            "js-agent: __mac-job-start runs on macOS only\n"
        );
    }
    assert!(out.is_empty());
}
