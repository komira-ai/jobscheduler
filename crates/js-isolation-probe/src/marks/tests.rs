use std::io;
use std::path::Path;

use js_testkit::dir::TestDir;

use super::*;
use crate::host::test_who;
use crate::tools::fake::Fake;

const DSCL_READ: &str = "/usr/bin/dscl . -read /Users/pool-1 dsAttrTypeNative:js-isolation-probe";

fn ctx<'a>(tools: &'a Fake, who: &'a Who) -> Ctx<'a> {
    Ctx {
        tools,
        who,
        linger_secs: 60,
    }
}

/// A token no other test or run uses.
fn token(test: &str) -> String {
    format!("{test}{}", std::process::id())
}

fn home(name: &str) -> (TestDir, Who) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    fs::create_dir(dir.join("exec")).unwrap();
    let who = test_who(&dir);
    (dir, who)
}

fn missing() -> Ran {
    Ran::unstarted(io::Error::from(io::ErrorKind::NotFound))
}

#[test]
fn every_kind_has_its_name() {
    let names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(
        names,
        [
            "named-semaphore",
            "sysv-semaphore",
            "sysv-shared-memory",
            "preferences",
            "launch-services",
            "pasteboard",
            "keychain",
            "background-process",
            "crontab",
            "at-job",
            "launchd-user",
            "launchd-gui",
            "account-attribute"
        ]
    );
}

#[test]
fn a_named_semaphore_is_left_found_and_removed_on_its_own_slot() {
    let (_dir, who) = home("marks-sem");
    let tools = Fake::default();
    let ctx = ctx(&tools, &who);
    let tag = token("ns");
    let _sweep = crate::ipc::Sweep(tag.clone());
    let line = leave(&ctx, Kind::NamedSemaphore, &tag);
    assert_eq!(line.outcome, Outcome::Allowed, "{line}");
    let other = find(&ctx, Kind::NamedSemaphore, &tag, &who, false);
    assert_eq!(
        other.to_string(),
        "find:named-semaphore: found; opened it, left for its own slot"
    );
    let same = find(&ctx, Kind::NamedSemaphore, &tag, &who, true);
    assert_eq!(
        same.to_string(),
        "find:named-semaphore: found; opened it, removed"
    );
    assert_eq!(
        find(&ctx, Kind::NamedSemaphore, &tag, &who, true).outcome,
        Outcome::Gone
    );
    // A second make of the same name is refused (EEXIST): not a denial.
    let made = leave(&ctx, Kind::NamedSemaphore, &tag);
    assert_eq!(made.outcome, Outcome::Allowed);
    let again = leave(&ctx, Kind::NamedSemaphore, &tag);
    assert_eq!(again.errno, Some(libc::EEXIST));
    assert_eq!(again.outcome, Outcome::Error);
    assert_eq!(remove(&ctx, Kind::NamedSemaphore, &tag, &who, None), Ok(()));
    assert!(remove(&ctx, Kind::NamedSemaphore, &tag, &who, None).is_err());
}

#[test]
fn sysv_objects_are_left_found_and_removed_by_id() {
    let (_dir, who) = home("marks-sysv");
    let tools = Fake::default();
    let ctx = ctx(&tools, &who);
    for kind in [Kind::SysvSemaphore, Kind::SysvSharedMemory] {
        let tag = token(&kind.name()[5..8]);
        let _sweep = crate::ipc::Sweep(tag.clone());
        let (line, handle) = make(&ctx, kind, "leave", &tag);
        assert_eq!(line.outcome, Outcome::Allowed, "{line}");
        assert!(handle.is_some());
        let (again, none) = make(&ctx, kind, "leave", &tag);
        assert_eq!(again.errno, Some(libc::EEXIST), "{again}");
        assert_eq!(none, None);
        let found = find(&ctx, kind, &tag, &who, true);
        assert_eq!(found.outcome, Outcome::Found, "{found}");
        assert!(found.detail.ends_with(", removed"), "{found}");
        assert_eq!(find(&ctx, kind, &tag, &who, true).outcome, Outcome::Gone);
        assert_eq!(
            remove(&ctx, kind, &tag, &who, None),
            Err("no id".to_owned())
        );
        assert!(remove(&ctx, kind, &tag, &who, Some("-1")).is_err());
    }
}

#[test]
fn an_ipc_lookup_tells_gone_another_users_and_cannot_tell() {
    assert_eq!(
        ipc_look(&io::Error::from_raw_os_error(libc::ENOENT), "x"),
        Look::Gone
    );
    assert_eq!(
        ipc_look(&io::Error::from_raw_os_error(libc::EACCES), "theirs"),
        Look::found("theirs")
    );
    assert!(matches!(
        ipc_look(&io::Error::from_raw_os_error(libc::EINVAL), "x"),
        Look::CannotLook(_)
    ));
}

#[test]
fn a_preference_is_written_read_back_and_deleted() {
    let (_dir, who) = home("marks-prefs");
    let read = "/usr/bin/defaults read com.komira.js-isolation-probe marker";
    let tools = Fake::default()
        .exit(
            "/usr/bin/defaults write com.komira.js-isolation-probe marker t1",
            0,
            "",
            "",
        )
        .exit(read, 0, "t1\n", "")
        .exit(read, 0, "t1\n", "")
        .exit(
            read,
            1,
            "",
            "The domain/default pair of (com.komira.js-isolation-probe, marker) does not exist",
        )
        .exit(
            "/usr/bin/defaults delete com.komira.js-isolation-probe",
            0,
            "",
            "",
        );
    let ctx = ctx(&tools, &who);
    assert_eq!(
        leave(&ctx, Kind::Preferences, "t1").outcome,
        Outcome::Allowed
    );
    assert_eq!(
        find(&ctx, Kind::Preferences, "t1", &who, true).to_string(),
        "find:preferences: found; removed"
    );
    assert_eq!(
        find(&ctx, Kind::Preferences, "t1", &who, true).outcome,
        Outcome::Gone
    );
}

#[test]
fn a_tool_that_says_yes_but_keeps_nothing_is_a_denial() {
    let (_dir, who) = home("marks-accepted");
    let tools = Fake::default()
        .exit(
            "/usr/bin/defaults write com.komira.js-isolation-probe marker t1",
            0,
            "",
            "",
        )
        .exit(
            "/usr/bin/defaults read com.komira.js-isolation-probe marker",
            0,
            "other\n",
            "",
        );
    let ctx = ctx(&tools, &who);
    assert_eq!(
        leave(&ctx, Kind::Preferences, "t1").to_string(),
        "leave:preferences: denied; accepted, but not there right after"
    );
}

#[test]
fn a_refused_preference_write_is_a_denial_and_not_looked_back() {
    let (_dir, who) = home("marks-prefs-denied");
    let tools = Fake::default().exit(
        "/usr/bin/defaults write com.komira.js-isolation-probe marker t1",
        1,
        "",
        "Could not write domain",
    );
    let ctx = ctx(&tools, &who);
    let line = leave(&ctx, Kind::Preferences, "t1");
    assert_eq!(
        line.to_string(),
        "leave:preferences: denied; exit 1: Could not write domain"
    );
    assert_eq!(tools.lines().len(), 1);
}

#[test]
fn a_look_that_cannot_tell_is_not_applicable_and_keeps_an_allowed_leave() {
    let (_dir, who) = home("marks-cannot");
    let tools = Fake::default()
        .exit(
            "/usr/bin/defaults write com.komira.js-isolation-probe marker t1",
            0,
            "",
            "",
        )
        .exit(
            "/usr/bin/defaults read com.komira.js-isolation-probe marker",
            5,
            "",
            "odd",
        );
    let ctx = ctx(&tools, &who);
    assert_eq!(
        leave(&ctx, Kind::Preferences, "t1").to_string(),
        "leave:preferences: allowed; cannot look back: exit 5: odd"
    );
    assert_eq!(
        find(&ctx, Kind::Preferences, "t1", &who, false).to_string(),
        "find:preferences: n/a; cannot look: exit 5: odd"
    );
}

#[test]
fn launch_services_registers_an_app_in_the_run_dir_and_finds_it_in_the_dump() {
    let (_dir, who) = home("marks-ls");
    let app = who
        .cwd
        .join("js-isolation-probe-t1.app")
        .display()
        .to_string();
    let tools = Fake::default()
        .exit(&format!("{LSREGISTER} -f {app}"), 0, "", "")
        .exit(
            &format!("{LSREGISTER} -dump"),
            0,
            "bundle id: com.komira.js-isolation-probe.t1\n",
            "",
        )
        .exit(&format!("{LSREGISTER} -dump"), 0, "nothing\n", "")
        .exit(&format!("{LSREGISTER} -dump"), 1, "", "no database")
        .exit(&format!("{LSREGISTER} -u {app}"), 0, "", "");
    let ctx = ctx(&tools, &who);
    assert_eq!(
        leave(&ctx, Kind::LaunchServices, "t1").outcome,
        Outcome::Allowed
    );
    let info = fs::read_to_string(
        who.cwd
            .join("js-isolation-probe-t1.app/Contents/Info.plist"),
    )
    .unwrap();
    assert!(
        info.contains("<string>com.komira.js-isolation-probe.t1</string>"),
        "{info}"
    );
    assert_eq!(
        find(&ctx, Kind::LaunchServices, "t1", &who, true).outcome,
        Outcome::Gone
    );
    assert_eq!(
        find(&ctx, Kind::LaunchServices, "t1", &who, true).outcome,
        Outcome::NotApplicable
    );
    assert_eq!(remove(&ctx, Kind::LaunchServices, "t1", &who, None), Ok(()));
}

#[test]
fn an_app_or_plist_or_crontab_that_cannot_be_written_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "marks-unwritable");
    fs::write(dir.join("exec"), b"a file where the run dir would be").unwrap();
    let who = test_who(&dir);
    let tools = Fake::default().exit(
        "/usr/bin/crontab -l",
        1,
        "",
        "crontab: no crontab for pool-1",
    );
    let ctx = ctx(&tools, &who);
    for kind in [Kind::LaunchServices, Kind::LaunchdUser, Kind::Crontab] {
        let (line, _) = make(&ctx, kind, "leave", "t1");
        assert_eq!(line.outcome, Outcome::Error, "{line}");
        assert!(line.errno.is_some(), "{line}");
    }
}

#[test]
fn the_pasteboard_holds_the_text_or_cannot_be_read() {
    let (_dir, who) = home("marks-paste");
    let tools = Fake::default()
        .exit("/usr/bin/pbcopy", 0, "", "")
        .exit("/usr/bin/pbpaste", 0, "js-isolation-probe t1", "")
        .exit("/usr/bin/pbpaste", 0, "something else", "")
        .exit("/usr/bin/pbpaste", 1, "", "no pasteboard server");
    let ctx = ctx(&tools, &who);
    assert_eq!(
        leave(&ctx, Kind::Pasteboard, "t1").outcome,
        Outcome::Allowed
    );
    assert_eq!(tools.asked.borrow()[0].1, "js-isolation-probe t1");
    assert_eq!(
        find(&ctx, Kind::Pasteboard, "t1", &who, false).outcome,
        Outcome::Gone
    );
    assert_eq!(
        find(&ctx, Kind::Pasteboard, "t1", &who, false).outcome,
        Outcome::NotApplicable
    );
    assert_eq!(remove(&ctx, Kind::Pasteboard, "t1", &who, None), Ok(()));
    assert_eq!(tools.asked.borrow().last().unwrap().1, "");
}

#[test]
fn a_keychain_item_is_found_or_said_to_be_absent() {
    let (_dir, who) = home("marks-keychain");
    let find_line =
        "/usr/bin/security find-generic-password -s com.komira.js-isolation-probe -a t1";
    let tools = Fake::default()
        .exit(find_line, 0, "keychain: ...", "")
        .exit(find_line, 44, "", "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.")
        .exit(
            "/usr/bin/security delete-generic-password -s com.komira.js-isolation-probe -a t1",
            1,
            "",
            "denied",
        );
    let ctx = ctx(&tools, &who);
    let line = find(&ctx, Kind::Keychain, "t1", &who, true);
    assert_eq!(
        line.to_string(),
        "find:keychain: found; could not remove: exit 1: denied"
    );
    assert_eq!(
        find(&ctx, Kind::Keychain, "t1", &who, true).outcome,
        Outcome::Gone
    );
}

#[test]
fn the_background_process_is_started_found_by_its_token_and_killed() {
    let (_dir, who) = home("marks-linger");
    let ps = "/bin/ps -axo pid=,pgid=,uid=,command=";
    let tools = Fake::default()
        .exit(
            ps,
            0,
            "  1     1     0 /sbin/launchd\n 4242  4242   601 /b/js-isolation-probe linger --token t1 --secs 60\n",
            "",
        )
        .exit(ps, 0, " 77  70   600 /b/js-isolation-probe linger --token t1 --secs 60\n", "")
        .exit(ps, 0, " 4242 4242 601 /b/js-isolation-probe linger --token t2 --secs 60\n", "")
        .exit(ps, 1, "", "ps: not allowed");
    let ctx = ctx(&tools, &who);
    let line = leave(&ctx, Kind::BackgroundProcess, "t1");
    assert_eq!(
        line.to_string(),
        "leave:background-process: allowed; pid 4242"
    );
    assert_eq!(*tools.lingered.borrow(), [("t1".to_owned(), 60)]);
    let line = find(&ctx, Kind::BackgroundProcess, "t1", &who, true);
    assert_eq!(
        line.to_string(),
        "find:background-process: found; pid 77 uid 600, removed"
    );
    assert_eq!(*tools.killed.borrow(), [77]);
    assert_eq!(
        find(&ctx, Kind::BackgroundProcess, "t1", &who, true).outcome,
        Outcome::Gone
    );
    assert_eq!(
        find(&ctx, Kind::BackgroundProcess, "t1", &who, true).outcome,
        Outcome::NotApplicable
    );
    assert_eq!(
        remove(&ctx, Kind::BackgroundProcess, "t1", &who, None),
        Err("no pid".to_owned())
    );
}

#[test]
fn a_background_process_that_cannot_start_or_be_killed_says_so() {
    let (_dir, who) = home("marks-linger-refused");
    let tools = Fake {
        linger_refused: true,
        kill_refused: true,
        ..Fake::default()
    };
    let ctx = ctx(&tools, &who);
    let line = leave(&ctx, Kind::BackgroundProcess, "t1");
    assert_eq!(
        line.to_string(),
        "leave:background-process: denied (EPERM); starting it"
    );
    assert!(remove(&ctx, Kind::BackgroundProcess, "t1", &who, Some("5")).is_err());
}

#[test]
fn only_the_lingers_row_with_the_token_is_taken() {
    let row = |r: &str| linger_row(r, "t1");
    assert_eq!(
        row("5 5 600 /p linger --token t1 --secs 9"),
        Some((5, 5, 600))
    );
    for bad in [
        "",
        "x",
        "5",
        "5 x",
        "5 5",
        "5 5 x",
        "5 5 600",
        "5 5 600 /p probe --token t1",
        "5 5 600 /p linger --token t11",
        "5 5 600 /p linger --secs 9",
    ] {
        assert_eq!(row(bad), None, "{bad:?}");
    }
}

#[test]
fn a_crontab_is_installed_from_a_file_found_and_removed() {
    let (_dir, who) = home("marks-cron");
    let file = who.cwd.join("com.komira.js-isolation-probe.t1.crontab");
    let tools = Fake::default()
        .exit(
            "/usr/bin/crontab -l",
            1,
            "",
            "crontab: no crontab for pool-1",
        )
        .exit("/usr/bin/crontab -l", 0, "# js-isolation-probe t1\n", "")
        .exit("/usr/bin/crontab -l", 0, "", "")
        .exit("/usr/bin/crontab -l", 0, "# js-isolation-probe t1\n", "")
        .exit(
            "/usr/bin/crontab -l",
            1,
            "",
            "crontab: no crontab for pool-1",
        )
        .exit(&format!("/usr/bin/crontab {}", file.display()), 0, "", "")
        .exit("/usr/bin/crontab -r", 0, "", "");
    let ctx = ctx(&tools, &who);
    assert_eq!(leave(&ctx, Kind::Crontab, "t1").outcome, Outcome::Allowed);
    assert!(!file.exists(), "the file is removed once installed");
    // An empty crontab is no crontab: the probe installs its own and removes it.
    assert_eq!(
        try_and_undo(&ctx, Kind::Crontab, "t1").to_string(),
        "priv:crontab: allowed; undone"
    );
    assert_eq!(
        find(&ctx, Kind::Crontab, "t1", &who, true).to_string(),
        "find:crontab: found; removed"
    );
    assert_eq!(
        find(&ctx, Kind::Crontab, "t1", &who, true).outcome,
        Outcome::Gone
    );
}

#[test]
fn a_crontab_that_cannot_be_read_is_never_replaced() {
    let (_dir, who) = home("marks-cron-unread");
    let tools = Fake::default().exit("/usr/bin/crontab -l", 1, "", "crontab: permission denied");
    let ctx = ctx(&tools, &who);
    let line = try_and_undo(&ctx, Kind::Crontab, "t1");
    assert_eq!(
        line.to_string(),
        "priv:crontab: n/a; the account has a crontab, or it cannot be read (exit 1: crontab: permission denied); it is left as it is"
    );
    assert_eq!(tools.lines(), ["/usr/bin/crontab -l"]);
}

#[test]
fn an_existing_crontab_is_never_touched() {
    let (_dir, who) = home("marks-cron-kept");
    let tools = Fake::default().exit("/usr/bin/crontab -l", 0, "0 * * * * someone's\n", "");
    let ctx = ctx(&tools, &who);
    let line = try_and_undo(&ctx, Kind::Crontab, "t1");
    assert_eq!(line.outcome, Outcome::NotApplicable);
    assert_eq!(tools.lines(), ["/usr/bin/crontab -l"]);
}

#[test]
fn an_at_job_is_queued_far_ahead_found_by_its_text_and_removed() {
    let (_dir, who) = home("marks-at");
    let tools = Fake::default()
        .exit(
            "/usr/bin/at -t 210001010000",
            0,
            "",
            "job 12 at Fri Jan  1 00:00:00 2100\n",
        )
        .exit(
            "/usr/bin/atq",
            0,
            "11\tFri Jan  1 00:00:00 2100\n12\tFri Jan  1 00:00:00 2100\n",
            "",
        )
        .exit(
            "/usr/bin/at -c 11",
            0,
            "/usr/bin/true # js-isolation-probe other\n",
            "",
        )
        .exit(
            "/usr/bin/at -c 12",
            0,
            "/usr/bin/true # js-isolation-probe t1\n",
            "",
        )
        .exit("/usr/bin/atrm 12", 0, "", "");
    let ctx = ctx(&tools, &who);
    let line = try_and_undo(&ctx, Kind::AtJob, "t1");
    assert_eq!(line.to_string(), "priv:at-job: allowed; undone");
    assert_eq!(
        tools.asked.borrow()[0].1,
        "/usr/bin/true # js-isolation-probe t1\n"
    );
    let line = find(&ctx, Kind::AtJob, "t1", &who, true);
    assert_eq!(line.to_string(), "find:at-job: found; job 12, removed");
}

#[test]
fn an_at_job_without_a_number_cannot_be_undone_and_an_unreadable_queue_cannot_be_looked_at() {
    let (_dir, who) = home("marks-at-odd");
    let tools = Fake::default()
        .exit("/usr/bin/at -t 210001010000", 0, "", "queued\n")
        .exit("/usr/bin/atq", 1, "", "not allowed")
        .exit("/usr/bin/atq", 0, "3\tsoon\n", "")
        .exit("/usr/bin/at -c 3", 1, "", "no");
    let ctx = ctx(&tools, &who);
    assert_eq!(
        try_and_undo(&ctx, Kind::AtJob, "t1").to_string(),
        "priv:at-job: allowed; could not undo: no job number"
    );
    assert_eq!(
        find(&ctx, Kind::AtJob, "t1", &who, true).outcome,
        Outcome::NotApplicable
    );
    assert_eq!(
        find(&ctx, Kind::AtJob, "t1", &who, true).outcome,
        Outcome::Gone
    );
    assert_eq!(at_job_number(b"job x at"), None);
    assert_eq!(
        at_job_number(b"warning\njob 7 at now"),
        Some("7".to_owned())
    );
}

#[test]
fn a_launchd_job_is_bootstrapped_in_the_users_domains_and_booted_out() {
    let (_dir, who) = home("marks-launchd");
    let label = "com.komira.js-isolation-probe.t1";
    let user_plist = who
        .cwd
        .join(format!("{label}-launchd-user.plist"))
        .display()
        .to_string();
    let gui_plist = who
        .cwd
        .join(format!("{label}-launchd-gui.plist"))
        .display()
        .to_string();
    let tools = Fake::default()
        .exit(
            &format!("/bin/launchctl bootstrap user/601 {user_plist}"),
            0,
            "",
            "",
        )
        .exit(
            &format!("/bin/launchctl bootout user/601/{label}"),
            0,
            "",
            "",
        )
        .exit(
            &format!("/bin/launchctl bootstrap gui/601 {gui_plist}"),
            5,
            "",
            "Bootstrap failed: 5: Input/output error",
        )
        .exit(
            &format!("/bin/launchctl print user/600/{label}"),
            113,
            "",
            "Could not find service",
        )
        .exit(
            &format!("/bin/launchctl print gui/600/{label}"),
            0,
            "state = waiting",
            "",
        );
    let ctx = ctx(&tools, &who);
    assert_eq!(
        try_and_undo(&ctx, Kind::LaunchdUser, "t1").to_string(),
        "priv:launchd-user: allowed; undone"
    );
    assert!(
        !Path::new(&user_plist).exists(),
        "the probe removes the plist it wrote"
    );
    let (_, _) = make(&ctx, Kind::LaunchdUser, "leave", "t1");
    let plist = fs::read_to_string(&user_plist).unwrap();
    assert!(
        plist.contains(&format!("<string>{label}</string>")),
        "{plist}"
    );
    assert_eq!(
        try_and_undo(&ctx, Kind::LaunchdGui, "t1").to_string(),
        "priv:launchd-gui: denied; exit 5: Bootstrap failed: 5: Input/output error"
    );
    let them = who.leaver(600, "pool-0", Path::new("/Users/pool-0"));
    assert_eq!(
        find(&ctx, Kind::LaunchdUser, "t1", &them, false).outcome,
        Outcome::Gone
    );
    assert_eq!(
        find(&ctx, Kind::LaunchdGui, "t1", &them, false).to_string(),
        "find:launchd-gui: found; left for its own slot"
    );
}

#[test]
fn an_attribute_on_the_own_record_is_read_on_the_leavers_record() {
    let (_dir, who) = home("marks-attr");
    let tools = Fake::default()
        .exit(
            "/usr/bin/dscl . -create /Users/pool-1 dsAttrTypeNative:js-isolation-probe t1",
            40,
            "",
            "<main> attribute status: eDSPermissionError",
        )
        .exit(
            DSCL_READ,
            0,
            "dsAttrTypeNative:js-isolation-probe: t1\n",
            "",
        )
        .exit(
            DSCL_READ,
            181,
            "",
            "No such key: dsAttrTypeNative:js-isolation-probe",
        )
        .exit(
            DSCL_READ,
            0,
            "dsAttrTypeNative:js-isolation-probe: other\n",
            "",
        )
        .exit(
            "/usr/bin/dscl . -delete /Users/pool-1 dsAttrTypeNative:js-isolation-probe",
            0,
            "",
            "",
        );
    let ctx = ctx(&tools, &who);
    assert_eq!(
        try_and_undo(&ctx, Kind::AccountAttribute, "t1").to_string(),
        "priv:account-attribute: denied; exit 40: <main> attribute status: eDSPermissionError"
    );
    assert_eq!(
        find(&ctx, Kind::AccountAttribute, "t1", &who, true).to_string(),
        "find:account-attribute: found; removed"
    );
    assert_eq!(
        find(&ctx, Kind::AccountAttribute, "t1", &who, true).outcome,
        Outcome::Gone
    );
    assert_eq!(
        find(&ctx, Kind::AccountAttribute, "t1", &who, true).outcome,
        Outcome::Gone
    );
}

#[test]
fn a_missing_tool_cannot_look_and_its_leave_is_not_applicable() {
    let (_dir, who) = home("marks-missing");
    let tools = Fake::default().on("/usr/bin/pbcopy", missing()).on(
        "/usr/bin/security find-generic-password -s com.komira.js-isolation-probe -a t1",
        missing(),
    );
    let ctx = ctx(&tools, &who);
    assert_eq!(
        leave(&ctx, Kind::Pasteboard, "t1").outcome,
        Outcome::NotApplicable
    );
    let line = find(&ctx, Kind::Keychain, "t1", &who, false);
    assert_eq!(line.outcome, Outcome::NotApplicable);
    assert!(
        line.detail.starts_with("cannot look: cannot start: "),
        "{line}"
    );
}

#[test]
fn the_privileged_kinds_are_the_ones_that_make_something_lasting() {
    assert_eq!(
        Kind::PRIVILEGED.map(Kind::name),
        [
            "launchd-user",
            "launchd-gui",
            "crontab",
            "at-job",
            "account-attribute"
        ]
    );
    assert_eq!(join("", "b"), "b");
    assert_eq!(join("a", "b"), "a, b");
}
