//! The profile of a native run: where it lets a job write, and that the places only the
//! privacy controls guard stay denied.

use std::collections::BTreeMap;
use std::path::PathBuf;

use js_mac_pool::seatbelt::{AbsPath, MountGrant, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

use super::*;
use crate::mac_accounts::Slots;
use crate::native::Ceiling;
use crate::native::accounts::Accounts;
use crate::native::fence::PRIVACY_ONLY_PLACES;

/// The configuration of a run whose accounts' homes are below `/srv/homes`, with the agent's
/// read denies: its state root and the privacy-only places, as `from_flags` adds them.
fn config() -> NativeConfig {
    let mut read_denies = vec![AbsPath::parse("/srv/js-state").unwrap()];
    read_denies.extend(
        PRIVACY_ONLY_PLACES
            .iter()
            .map(|place| AbsPath::parse(place).unwrap()),
    );
    NativeConfig {
        accounts: Accounts {
            state: PathBuf::from("/srv/accounts"),
            slots: "700-709".parse::<Slots>().unwrap(),
            ready: 1,
        },
        homes: PathBuf::from("/srv/homes"),
        network: Ceiling::Loopback,
        tool_roots: vec![PathBuf::from("/usr/bin")],
        read_denies,
        mount_roots: Vec::new(),
        read_model: ReadModel::Open,
        journal: PathBuf::from("/srv/state/native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    }
}

fn body(program: &str, args: &[&str]) -> NativeSpec {
    NativeSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    }
}

/// A read-write mount grant of `path`.
fn mount(path: &str) -> MountGrant {
    MountGrant {
        path: AbsPath::parse(path).unwrap(),
        write: true,
        no_exec: true,
    }
}

/// The value of the parameter a rule line names (`(param "NAME")`), if it names one.
fn param_of<'a>(line: &str, params: &'a [(String, String)]) -> Option<&'a str> {
    let name = line.split("(param \"").nth(1)?.split('"').next()?;
    params
        .iter()
        .find(|(given, _)| given == name)
        .map(|(_, value)| value.as_str())
}

/// Whether the path `grant` covers `place`: it is the place or one of its ancestors.
fn covers(grant: &str, place: &str) -> bool {
    place == grant
        || place
            .strip_prefix(grant)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Asserts that the profile grants writes to the run directory `<home>/exec` by the exact
/// rule text and names no parameter that is the account's home.
///
/// Catches: a profile that grants the whole home, with which a job makes
/// `~/Library/Preferences` that root cannot remove, or the account's per-user folder.
#[test]
fn a_run_may_write_below_its_run_directory_alone() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let profile = build(&config(), &body("/bin/echo", &[]), home, (&[], &[]), None).unwrap();
    let rule = "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))";
    let writes: Vec<&str> = profile
        .text()
        .lines()
        .filter(|line| line.starts_with("(allow file-write") || line.starts_with("(allow file*"))
        .collect();
    assert_eq!(writes, [rule]);
    let params = profile.params();
    assert!(params.contains(&("RUN".to_owned(), "/srv/homes/kbf-acct-4/exec".to_owned())));
    assert!(
        !params
            .iter()
            .any(|(_, value)| value == "/srv/homes/kbf-acct-4"
                || value.starts_with("/private/var/folders")),
        "{params:?}"
    );
}

/// Asserts that each privacy-only place, under both spellings, is denied reads by a rule of
/// the built profile, and that no rule after that deny grants a path that holds the place
/// (Seatbelt's last matching rule wins), with a mount granted elsewhere in the profile.
///
/// Catches: the fence failing because the places were left out of the read denies, or
/// because a grant rendered after the deny reopens one of them.
#[test]
fn each_privacy_only_place_is_denied_after_every_grant_that_holds_it() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let grants = [mount("/srv/mounts/work")];
    let profile = build(
        &config(),
        &body("/bin/echo", &[]),
        home,
        (&[], &grants),
        None,
    )
    .unwrap();
    let params = profile.params();
    let lines: Vec<&str> = profile.text().lines().collect();
    for place in PRIVACY_ONLY_PLACES {
        let deny = lines
            .iter()
            .position(|line| {
                line.starts_with("(deny file-read*") && param_of(line, params) == Some(place)
            })
            .unwrap_or_else(|| panic!("{place} is not denied reads"));
        for line in &lines[deny + 1..] {
            if !line.starts_with("(allow") {
                continue;
            }
            let reopened = param_of(line, params).is_some_and(|granted| covers(granted, place));
            assert!(!reopened, "{line} follows the deny of {place}");
        }
        for line in &lines[..deny] {
            if line.starts_with("(allow file-read") {
                assert!(
                    !param_of(line, params).is_some_and(|granted| covers(granted, place)),
                    "{line}"
                );
            }
        }
    }
}

/// Asserts that a mount at or below a privacy-only place fails the build, naming the mount.
///
/// Catches: a mount grant, rendered after the read denies, that would reopen the place.
#[test]
fn a_mount_in_a_privacy_only_place_is_refused() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    for place in [
        "/Library/Application Support/com.apple.TCC",
        "/System/Volumes/Data/Library/Application Support/com.apple.TCC/sub",
    ] {
        let grants = [mount(place)];
        let why = build(
            &config(),
            &body("/bin/echo", &[]),
            home,
            (&[], &grants),
            None,
        )
        .unwrap_err();
        assert_eq!(
            why.0,
            format!("a mount: {place} lies in a place only the privacy controls guard")
        );
    }
}

/// Asserts that a mount beside a fenced place, whose name only begins like it, is built.
///
/// Catches: a fence check by text prefix that refuses `com.apple.TCC2`.
#[test]
fn a_mount_beside_a_privacy_only_place_is_built() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let grants = [mount("/Library/Application Support/com.apple.TCC2")];
    assert!(
        build(
            &config(),
            &body("/bin/echo", &[]),
            home,
            (&[], &grants),
            None
        )
        .is_ok()
    );
}
