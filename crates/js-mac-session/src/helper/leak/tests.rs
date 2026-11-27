use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::*;
use crate::autologin::CommandOutput;
use crate::helper::Outcome;
use crate::helper::tests::{Rig, log, me, rig_with};
use crate::lease::{PoolSize, UidRange};
use crate::proto::Request;

const DUMPBTM: &str = "/usr/bin/sfltool dumpbtm";

/// `sfltool dumpbtm` with one entry per identifier in `ids`, all under uid 0.
fn dumpbtm(ids: &[&str]) -> String {
    let mut text = "========================\n Records for UID 0 : 0000\n========================\n\n Items:\n\n".to_owned();
    for (n, id) in ids.iter().enumerate() {
        text.push_str(&format!(
            " #{}:\n                 UUID: {n}\n           Identifier: {id}\n                 Type: legacy daemon (0x10010)\n           Generation: {n}\n",
            n + 1
        ));
    }
    text
}

fn answer(rig: &Rig, command: &str, success: bool, stdout: &str, stderr: &str) {
    rig.host.state().answers.insert(
        command.to_owned(),
        CommandOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        },
    );
}

/// Writes `text` to `path`, mode 0600 whatever the umask (the helper trusts no file
/// its group or others may write).
fn write_private(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Records the baseline as a release with a `baseline` verb wrote it: the header, then each
/// item a collection finds now, one per line, in a file of the helper's own. Every item must
/// be readable (that release refused an incomplete baseline). Returns how many items each
/// class holds, every class named.
fn record(rig: &Rig) -> Vec<(String, usize)> {
    let snapshot = {
        let ledger = rig.helper.lock();
        scan::collect(
            rig.helper.host.as_ref(),
            &rig.helper.settings,
            &own_uids(&ledger),
        )
    };
    assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
    let mut text = format!("{}\n", scan::BASELINE_HEADER);
    for item in &snapshot.items {
        text.push_str(item);
        text.push('\n');
    }
    write_private(&rig.helper.settings.scan.baseline, &text);
    let mut counts: BTreeMap<String, usize> = [
        "auto-login",
        "background-tasks",
        "launch-agents",
        "launch-daemons",
        "lease-users",
        "login-items",
    ]
    .into_iter()
    .map(|class| (class.to_owned(), 0))
    .collect();
    for item in &snapshot.items {
        let class = item.split(' ').next().unwrap_or_default();
        *counts.entry(class.to_owned()).or_default() += 1;
    }
    counts.into_iter().collect()
}

/// A Mac with something in every class but auto-login (off at rest): a launch agent,
/// a launch daemon, a login items file (and launchd's `disabled` overrides, which
/// the login items class leaves out), a background task, a foreign user record in the
/// lease range; and a pool of four free users, which are no scan items, in a range of
/// six uids.
fn furnished(name: &str) -> Rig {
    let mut rig = rig_with(name, UidRange::new(me(), me() + 5).unwrap(), None);
    rig.helper.settings.pool = PoolSize::new(4).unwrap();
    rig.helper.boot_reset(&[]).unwrap();
    log(&rig);
    for folder in ["LaunchAgents", "LaunchDaemons", "launchd"] {
        std::fs::create_dir(rig.dir.join(folder)).unwrap();
    }
    std::fs::write(rig.dir.join("LaunchAgents/com.apple.a.plist"), "a").unwrap();
    std::fs::write(rig.dir.join("LaunchDaemons/com.apple.d.plist"), "d").unwrap();
    std::fs::write(rig.dir.join("launchd/loginitems.501.plist"), "l").unwrap();
    std::fs::write(rig.dir.join("launchd/disabled.501.plist"), "o").unwrap();
    answer(&rig, DUMPBTM, true, &dumpbtm(&["16.com.apple.btm"]), "");
    rig.host.state().foreign.insert(me() + 4);
    rig
}

/// A furnished Mac whose baseline is recorded.
fn baselined(name: &str) -> Rig {
    let rig = furnished(name);
    record(&rig);
    log(&rig);
    rig
}

fn classes(pairs: &[(&str, usize)]) -> Vec<(String, usize)> {
    pairs.iter().map(|&(c, n)| (c.to_owned(), n)).collect()
}

/// Catches: a scan of an untouched Mac that is not clean (an item that changes by
/// itself, such as a BTM entry's UUID or generation), or a collection that leaves a class
/// out.
#[test]
fn a_clean_tree_gives_an_empty_diff() {
    let rig = furnished("leak-clean");
    let counts = record(&rig);
    assert_eq!(
        counts,
        classes(&[
            ("auto-login", 0),
            ("background-tasks", 1),
            ("launch-agents", 1),
            ("launch-daemons", 1),
            ("lease-users", 1),
            ("login-items", 1),
        ])
    );
    // The entry's UUID and generation change; nothing installed did.
    let moved = dumpbtm(&["16.com.apple.btm"]).replace("UUID: 0", "UUID: 9");
    answer(
        &rig,
        DUMPBTM,
        true,
        &moved.replace("Generation: 0", "Generation: 4"),
        "",
    );
    let report = rig.helper.scan().unwrap();
    assert_eq!(report, ScanReport::default());
    assert!(report.clean());
    // An empty Mac too: no folder, no entry.
    let empty = crate::helper::tests::rig("leak-empty", 2);
    record(&empty);
    assert!(empty.helper.scan().unwrap().clean());
}

/// Asserts that a Mac with no auto-login set collects no auto-login item and no error, and
/// scans clean, whichever words its `defaults read` uses for the missing key (the texts are
/// what macOS 26.4, 26.6 and 27.0.1 printed). Catches a scan that knows only macOS 26's
/// wording: on macOS 27 every scan would report an error.
#[test]
fn an_unset_auto_login_scans_clean_in_the_words_of_macos_26_and_27() {
    let unset = [
        "2026-11-20 21:29:13.385 defaults[84941:2779365] \nThe domain/default pair of \
         (/Library/Preferences/com.apple.loginwindow, autoLoginUser) does not exist\n",
        "Error: Could not find key 'autoLoginUser' in domain 'com.apple.loginwindow'.\n",
    ];
    for (n, stderr) in unset.into_iter().enumerate() {
        let rig = furnished(&format!("leak-unset-auto-login-{n}"));
        let domain = rig.dir.join("loginwindow").display().to_string();
        let read = format!("/usr/bin/defaults read {domain} autoLoginUser");
        answer(&rig, &read, false, "", stderr);
        let counts = record(&rig);
        assert!(
            counts.contains(&("auto-login".to_owned(), 0)),
            "{stderr}: {counts:?}"
        );
        assert_eq!(
            rig.helper.scan().unwrap(),
            ScanReport::default(),
            "{stderr}"
        );
    }
}

/// Makes the next reset fail, and the replacement after it fail to delete the user, so
/// the slot breaks with its user still there.
fn break_the_reset(rig: &Rig) {
    let mut state = rig.host.state();
    state.fail_script.insert("make_home", [true].into());
    state.fail.insert("delete_user");
}

/// Catches: the user of a live lease reported as a leak, so that a scan while a lease
/// runs comes back dirty though nothing leaked; or the user of a lease that ended and
/// whose slot broke passed over as if its lease were still live.
#[test]
fn a_live_lease_s_user_is_no_item_and_is_one_once_its_lease_ended() {
    let rig = baselined("leak-live-lease");
    let uid = rig.helper.user_create("5.1").unwrap();
    rig.helper.user_create("5.2").unwrap();
    assert_eq!(rig.helper.scan().unwrap(), ScanReport::default());
    break_the_reset(&rig);
    rig.helper.user_delete("5.1").unwrap_err();
    let report = rig.helper.scan().unwrap();
    assert_eq!(report.diff.added, [format!("lease-users uid {uid}")]);
    assert_eq!(report.diff.removed, Vec::<String>::new());
    assert_eq!(report.errors, Vec::<String>::new());
}

/// One planted leak per item class: what it plants, and the start of the line the scan
/// must add.
type Plant = fn(&Rig);

const PLANTS: [(&str, Plant, &str); 9] = [
    (
        "a launch agent",
        |rig| std::fs::write(rig.dir.join("LaunchAgents/evil.plist"), "x").unwrap(),
        "launch-agents \"evil.plist\" file ",
    ),
    (
        "a launch daemon",
        |rig| std::fs::write(rig.dir.join("LaunchDaemons/evil.plist"), "x").unwrap(),
        "launch-daemons \"evil.plist\" file ",
    ),
    (
        "a login items file",
        |rig| std::fs::write(rig.dir.join("launchd/loginitems.0.plist"), "x").unwrap(),
        "login-items \"loginitems.0.plist\" file ",
    ),
    (
        "a background task",
        |rig| {
            let two = dumpbtm(&["16.com.apple.btm", "8.com.evil"]);
            answer(rig, DUMPBTM, true, &two, "");
        },
        "background-tasks uid=0 id=\"8.com.evil\" type=\"legacy daemon (0x10010)\"",
    ),
    (
        "a user in the lease range",
        |rig| {
            rig.host.state().foreign.insert(me() + 5);
        },
        "lease-users uid ",
    ),
    (
        "the user of a slot broken at the end of its lease",
        |rig| {
            rig.helper.user_create("5.1").unwrap();
            break_the_reset(rig);
            rig.helper.user_delete("5.1").unwrap_err();
        },
        "lease-users uid ",
    ),
    (
        "auto-login",
        |rig| {
            let domain = rig.dir.join("loginwindow").display().to_string();
            let read = format!("/usr/bin/defaults read {domain} autoLoginUser");
            answer(rig, &read, true, "kbf-lease-9-9\n", "");
        },
        "auto-login user \"kbf-lease-9-9\"",
    ),
    (
        "a kcpassword",
        |rig| std::fs::write(rig.dir.join("kcpassword"), "x").unwrap(),
        "auto-login kcpassword present",
    ),
    (
        "a launch daemon's new contents",
        |rig| std::fs::write(rig.dir.join("LaunchDaemons/com.apple.d.plist"), "e").unwrap(),
        "launch-daemons \"com.apple.d.plist\" file ",
    ),
];

/// Catches: a scan that ignores an item class (one plant per class, the leak scan's
/// planted-leak rule in fleet-updates.md 10.2), a daemon whose contents changed under
/// the same name passed as clean, or a plant that is found but leaves the scan clean.
#[test]
fn every_planted_leak_is_reported_as_a_diff() {
    for (n, (what, plant, line)) in PLANTS.iter().enumerate() {
        let rig = baselined(&format!("leak-plant-{n}"));
        plant(&rig);
        let report = rig.helper.scan().unwrap();
        assert!(!report.clean(), "{what}: {report:?}");
        assert!(
            report.diff.added.iter().any(|item| item.starts_with(line)),
            "{what}: no {line:?} in {report:#?}"
        );
        assert!(report.errors.is_empty(), "{what}: {report:?}");
    }
}

/// Catches: an item gone since the baseline (a system daemon a lease deleted) passed
/// as clean, a link followed instead of recorded, or a directory, a socket or a
/// `disabled` override taken for a file; and a mode or owner change missed.
#[test]
fn removals_links_and_odd_entries_are_items() {
    let rig = baselined("leak-kinds");
    std::fs::remove_file(rig.dir.join("LaunchAgents/com.apple.a.plist")).unwrap();
    let daemons = rig.dir.join("LaunchDaemons");
    std::os::unix::fs::symlink("/etc/passwd", daemons.join("link.plist")).unwrap();
    std::fs::create_dir(daemons.join("sub")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(daemons.join("sock")).unwrap();
    std::fs::write(rig.dir.join("launchd/disabled.0.plist"), "x").unwrap();
    let report = rig.helper.scan().unwrap();
    assert_eq!(report.diff.removed.len(), 1, "{report:#?}");
    assert!(report.diff.removed[0].starts_with("launch-agents \"com.apple.a.plist\" file "));
    let added = &report.diff.added;
    assert_eq!(added.len(), 3, "{report:#?}");
    assert!(added[0].starts_with("launch-daemons \"link.plist\" link "));
    assert!(added[0].ends_with(" -> \"/etc/passwd\""), "{added:?}");
    assert!(added[1].starts_with("launch-daemons \"sock\" other "));
    assert!(added[2].starts_with("launch-daemons \"sub\" dir "));
    // Mode 0600 instead of the file's 0644: the line changes.
    let rig = baselined("leak-mode");
    let file = rig.dir.join("LaunchDaemons/com.apple.d.plist");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let report = rig.helper.scan().unwrap();
    assert_eq!((report.diff.added.len(), report.diff.removed.len()), (1, 1));
    assert!(report.diff.added[0].contains(" 600 sha256:"), "{report:?}");
}

/// Catches: a file hashed only in part (the read loop stopping after its first 64 KiB
/// buffer, or after any read short of the end), so that a launch daemon over 64 KiB
/// whose tail a lease rewrote scans clean; and a hash that is not the SHA-256 of the
/// whole file. The file spans three full reads and a short fourth; each change sits
/// in a later read than the first.
#[test]
fn a_change_past_the_first_read_of_a_large_file_is_seen() {
    use sha2::Digest as _;
    const READ: usize = 64 * 1024;
    let size = 3 * READ + 7;
    let rig = furnished("leak-large");
    let file = rig.dir.join("LaunchDaemons/big.plist");
    std::fs::write(&file, vec![b'a'; size]).unwrap();
    record(&rig);
    for (n, at) in [READ, 2 * READ, size - 1].into_iter().enumerate() {
        let mut body = vec![b'a'; size];
        body[at] = b'b';
        std::fs::write(&file, &body).unwrap();
        let report = rig.helper.scan().unwrap();
        assert!(!report.clean(), "byte {at}: {report:?}");
        let sha = hex::encode(sha2::Sha256::digest(&body));
        assert_eq!(report.diff.added.len(), 1, "byte {at}: {report:#?}");
        let line = &report.diff.added[0];
        assert!(
            line.starts_with("launch-daemons \"big.plist\" file "),
            "{line}"
        );
        assert!(
            line.ends_with(&format!("sha256:{sha}")),
            "byte {at}: {line}"
        );
        assert_eq!(report.diff.removed.len(), 1, "byte {at}: {report:#?}");
        // Back to the baselined contents before the next change.
        std::fs::write(&file, vec![b'a'; size]).unwrap();
        assert!(rig.helper.scan().unwrap().clean(), "restore {n}");
    }
}

/// Catches: an item that cannot be read taken as an empty class (a leak hidden by a
/// failing `sfltool`, an unreadable folder or a broken directory lookup would pass).
#[test]
fn an_unreadable_item_makes_the_scan_dirty() {
    let breaks: [(&str, Plant, &str); 6] = [
        (
            "a folder that is a file",
            |rig| {
                std::fs::remove_dir_all(rig.dir.join("LaunchAgents")).unwrap();
                std::fs::write(rig.dir.join("LaunchAgents"), "").unwrap();
            },
            "launch-agents ",
        ),
        (
            "sfltool fails",
            |rig| answer(rig, DUMPBTM, false, "", "no access"),
            "dumpbtm failed: no access",
        ),
        (
            "sfltool prints no record",
            |rig| answer(rig, DUMPBTM, true, "what\n", ""),
            "no \"Records for UID\" header",
        ),
        (
            "the directory lookup fails",
            |rig| {
                rig.host.state().fail.insert("uid_taken");
            },
            "lease-users: looking up uid ",
        ),
        (
            "defaults fails for another reason",
            |rig| {
                let domain = rig.dir.join("loginwindow").display().to_string();
                let read = format!("/usr/bin/defaults read {domain} autoLoginUser");
                answer(rig, &read, false, "", "busy");
            },
            "failed: busy",
        ),
        (
            "no program runs",
            |rig| {
                rig.host.state().fail.insert("command");
            },
            "running /usr/bin/sfltool: command failed",
        ),
    ];
    for (n, (what, broken, says)) in breaks.iter().enumerate() {
        let rig = baselined(&format!("leak-broken-{n}"));
        broken(&rig);
        let report = rig.helper.scan().unwrap();
        assert!(!report.clean(), "{what}: {report:?}");
        assert!(
            report.errors.iter().any(|error| error.contains(says)),
            "{what}: no {says:?} in {report:#?}"
        );
    }
}

/// Catches: a file the helper cannot open (a mode 000 plist planted by an admin
/// lease) skipped rather than reported, or a `/etc/kcpassword` that cannot be looked
/// at taken as absent. Root opens any file, so this needs another user.
#[test]
fn an_unreadable_file_is_an_error() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let rig = baselined("leak-mode000");
    let file = rig.dir.join("LaunchDaemons/locked.plist");
    std::fs::write(&file, "x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = rig.helper.scan().unwrap();
    assert!(
        report.errors[0].contains("locked.plist: Permission denied"),
        "{report:?}"
    );
    // The state directory stops being searchable once the scan has read its baseline
    // (as `defaults read` runs, just before kcpassword is looked at).
    let dir = rig.dir.clone();
    rig.host.state().on_command = Some(Box::new(move |command: &str| {
        if command.starts_with("/usr/bin/defaults read") {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }));
    let report = rig.helper.scan().unwrap();
    std::fs::set_permissions(rig.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("kcpassword: Permission denied")),
        "{report:?}"
    );
}

/// Catches: a scan with no baseline taken as clean (everything would be "added", or
/// nothing compared), or a baseline file that is not the helper's own, or not one,
/// trusted.
#[test]
fn a_scan_needs_a_recorded_baseline_of_its_own() {
    let rig = furnished("leak-nobase");
    let error = rig.helper.scan().unwrap_err();
    assert!(error.contains("no baseline was recorded"), "{error}");
    let path = rig.dir.join("scan-baseline");
    write_private(&path, "something else\n");
    let error = rig.helper.scan().unwrap_err();
    assert!(error.contains("not a baseline"), "{error}");
    write_private(&path, "kbf-scan-baseline-v1\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
    let error = rig.helper.scan().unwrap_err();
    assert!(error.contains("writable by its group or others"), "{error}");
}

/// Catches: the scan verb routed to the wrong method, or a reply larger than a
/// frame (every list is cut to SCAN_LINES lines of SCAN_LINE_BYTES, the rest
/// counted).
#[test]
fn the_scan_verb_is_routed_and_its_reply_capped() {
    let rig = furnished("leak-handle");
    let reply = |request| match rig.helper.handle(request, Vec::new()) {
        Outcome::Reply(reply) => reply,
        other => panic!("not a reply: {other:?}"),
    };
    record(&rig);
    let daemons = rig.dir.join("LaunchDaemons");
    for n in 0..SCAN_LINES + 2 {
        std::fs::write(daemons.join(format!("{n:02}{}", "x".repeat(200))), "x").unwrap();
    }
    let Reply::Scanned {
        clean,
        added,
        removed,
        errors,
    } = reply(Request::Scan)
    else {
        panic!("not a scan reply");
    };
    assert!(!clean);
    assert_eq!(added.len(), SCAN_LINES + 1);
    assert_eq!(added[SCAN_LINES], "and 2 more");
    assert!(added.iter().all(|line| line.len() <= SCAN_LINE_BYTES));
    assert!(added[0].len() == SCAN_LINE_BYTES, "{}", added[0]);
    assert!(removed.is_empty() && errors.is_empty());
    // A cut never splits a character.
    let line = format!("{}é", "a".repeat(SCAN_LINE_BYTES - 1));
    assert_eq!(capped(vec![line])[0].len(), SCAN_LINE_BYTES - 1);
}

/// The folders are what `ScanFolder::macos` names: the scan of a real Mac looks at
/// the launch agent and daemon folders and launchd's login items.
#[test]
fn the_macos_folders_are_the_minimal_set() {
    let folders = crate::scan::ScanFolder::macos();
    let paths: Vec<&Path> = folders.iter().map(|f| f.path.as_path()).collect();
    assert_eq!(
        paths,
        [
            Path::new("/Library/LaunchAgents"),
            Path::new("/Library/LaunchDaemons"),
            Path::new("/private/var/db/com.apple.xpc.launchd"),
        ]
    );
    assert_eq!(folders[2].prefix, Some("loginitems."));
}
