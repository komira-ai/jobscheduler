//! The profile of a native run: where it lets a job write, and that the places only the
//! privacy controls guard stay denied.

use std::path::PathBuf;

use js_agent_wire::{HostPort, PortProtocol};
use js_mac_pool::seatbelt::{AbsPath, MountGrant, Net, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

use super::*;
use crate::mac_accounts::Slots;
use crate::native::Accounts;
use crate::native::Ceiling;
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
        host_ports: Vec::new(),
        read_model: ReadModel::Open,
        journal: PathBuf::from("/srv/state/native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    }
}

/// The network of a run of the level `none` under [`config`]'s ceiling.
fn none() -> Net {
    level(&config(), NetworkLevel::Off, None)
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
    let profile = build(&config(), &none(), home, (&[], &[]), None, &[]).unwrap();
    let rule = "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))";
    let writes: Vec<&str> = profile
        .text()
        .lines()
        .filter(|line| line.starts_with("(allow file-write") || line.starts_with("(allow file*"))
        .collect();
    assert_eq!(writes, [rule]);
    let params = profile.params();
    assert!(params.contains(&("RUN".to_owned(), "/srv/homes/kbf-acct-4/exec".to_owned())));
    // The home is named once, by the own-home read grant, which no write rule refers to.
    assert!(
        !params.iter().any(
            |(name, value)| (value == "/srv/homes/kbf-acct-4" && name != "OWN")
                || value.starts_with("/private/var/folders")
        ),
        "{params:?}"
    );
    assert!(params.contains(&("OWN".to_owned(), "/srv/homes/kbf-acct-4".to_owned())));
    assert!(
        !writes.iter().any(|line| line.contains("OWN")),
        "{writes:?}"
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
    let profile = build(&config(), &none(), home, (&[], &grants), None, &[]).unwrap();
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
        let why = build(&config(), &none(), home, (&[], &grants), None, &[]).unwrap_err();
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
    assert!(build(&config(), &none(), home, (&[], &grants), None, &[]).is_ok());
}

/// Asserts the homes roots of a pool whose homes are `/Users`, by the exact list: the homes
/// and root's home, each as given and below the data volume.
///
/// Catches root's home left out, and a root denied under one spelling only.
#[test]
fn the_homes_roots_are_the_homes_and_roots_home_under_both_spellings() {
    assert_eq!(
        home_roots(Path::new("/Users")),
        [
            "/Users",
            "/System/Volumes/Data/Users",
            "/private/var/root",
            "/System/Volumes/Data/private/var/root",
        ]
        .map(PathBuf::from)
    );
}

/// Asserts that each homes root is denied reads by a rule of the built profile, that after
/// those denies no rule grants a path that holds another account's home (a file in it, mode
/// `0755` or not), root's home, or the account's own home under its other spelling, and that
/// the account's own home is granted back, with a mount granted elsewhere in the profile.
/// This is what the isolation probe's `read:other-home` asks of a Mac.
///
/// Catches a homes root left out of the profile, a grant that reopens another home, and an
/// own home left unreadable by the deny of the homes above it.
#[test]
fn other_homes_and_roots_home_are_denied_reads_and_the_own_home_is_granted_back() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let grants = [mount("/srv/mounts/work")];
    let profile = build(&config(), &none(), home, (&[], &grants), None, &[]).unwrap();
    let params = profile.params();
    let lines: Vec<&str> = profile.text().lines().collect();
    let roots = home_roots(Path::new("/srv/homes"));
    let home_text = "/srv/homes/kbf-acct-4";
    let denies: Vec<usize> = roots
        .iter()
        .map(|root| {
            let root = root.to_str().unwrap();
            lines
                .iter()
                .position(|line| {
                    line.starts_with("(deny file-read*") && param_of(line, params) == Some(root)
                })
                .unwrap_or_else(|| panic!("{root} is not denied reads"))
        })
        .collect();
    let last = *denies.iter().max().unwrap();
    for place in [
        "/srv/homes/kbf-acct-9/f",
        "/srv/homes/kbf-acct-40/f",
        "/private/var/root/f",
        "/System/Volumes/Data/srv/homes/kbf-acct-4/exec/f",
    ] {
        for line in &lines[last + 1..] {
            if line.starts_with("(allow file-read*") {
                let reopened = param_of(line, params).is_some_and(|granted| covers(granted, place));
                assert!(!reopened, "{line} follows the deny of {place}");
            }
        }
    }
    let own_read = lines[last + 1..].iter().find(|line| {
        line.starts_with("(allow file-read*") && param_of(line, params) == Some(home_text)
    });
    assert!(own_read.is_some(), "{}", profile.text());
}

/// Asserts that a mount at a homes root or above one fails the build, as the policy's refusal:
/// its grant would follow the homes' denies and reopen every home.
///
/// Catches a mount of the homes that hands a job every other account's home.
#[test]
fn a_mount_over_a_homes_root_is_refused() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    for place in [
        "/srv/homes",
        "/srv",
        "/private/var/root",
        "/System/Volumes/Data/srv",
    ] {
        let grants = [mount(place)];
        let why = build(&config(), &none(), home, (&[], &grants), None, &[]).unwrap_err();
        assert_eq!(
            why.0, "the policy: a mount grant holds a read-denied path",
            "{place}"
        );
    }
}

/// Asserts that the trees given to `build` are granted by the profile: the mount root is a
/// read-denied parameter, the tree is a read-allowed one, and the same start without trees
/// has neither.
///
/// Catches: a build that drops the trees it is given, so a run could not read its own trees.
#[test]
fn the_trees_given_to_the_build_are_granted_and_none_grants_nothing() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let root = AbsPath::parse("/srv/store/trees").unwrap();
    let tree = AbsPath::parse("/srv/store/trees/abc").unwrap();
    let trees = js_mac_pool::seatbelt::MountTrees::new(root, vec![tree]).unwrap();
    let with = build(&config(), &none(), home, (&[], &[]), Some(trees), &[]).unwrap();
    let params = with.params();
    assert!(params.contains(&("MOUNT".to_owned(), "/srv/store/trees".to_owned())));
    assert!(params.contains(&("MT_0".to_owned(), "/srv/store/trees/abc".to_owned())));
    assert!(
        with.text()
            .lines()
            .any(|line| line == "(allow file-read* (subpath (param \"MT_0\")))")
    );
    let without = build(&config(), &none(), home, (&[], &[]), None, &[]).unwrap();
    assert!(!without.params().iter().any(|(name, _)| name == "MOUNT"));
    assert!(!without.text().contains("MT_0"));
}

/// Asserts the level of a run's profile for every ceiling and every level a start asks: the
/// job level `none` (`off` on the wire) is loopback under the ceilings `loopback`, `on` and
/// `open` and no network under `off`; `loopback` is loopback, `on` and `open` the open profile;
/// each met with the ceiling; and a bundle's `network_max` lowers it further (`off` takes even
/// loopback). Catches a `none` job left with the network denied on a Mac whose ceiling allows
/// loopback, one given loopback under the ceiling `off`, and a bundle cap that raises a level.
#[test]
fn a_none_job_gets_loopback_under_every_ceiling_but_off() {
    let at = |ceiling: Ceiling, asked: NetworkLevel, bundle: Option<Net>| {
        let config = NativeConfig {
            network: ceiling,
            ..config()
        };
        level(&config, asked, bundle)
    };
    for ceiling in [Ceiling::Loopback, Ceiling::On, Ceiling::Open] {
        assert_eq!(
            at(ceiling, NetworkLevel::Off, None),
            Net::Loopback,
            "{ceiling}"
        );
        assert_eq!(at(ceiling, NetworkLevel::Loopback, None), Net::Loopback);
    }
    for asked in [NetworkLevel::Off, NetworkLevel::Loopback, NetworkLevel::On] {
        assert_eq!(at(Ceiling::Off, asked, None), Net::Off, "{asked:?}");
    }
    assert_eq!(at(Ceiling::On, NetworkLevel::On, None), Net::On);
    assert_eq!(at(Ceiling::Open, NetworkLevel::Open, None), Net::On);
    assert_eq!(at(Ceiling::Loopback, NetworkLevel::On, None), Net::Loopback);
    assert_eq!(at(Ceiling::On, NetworkLevel::Off, Some(Net::Off)), Net::Off);
    assert_eq!(
        at(Ceiling::On, NetworkLevel::Off, Some(Net::On)),
        Net::Loopback
    );
    assert_eq!(
        at(Ceiling::Open, NetworkLevel::Open, Some(Net::Loopback)),
        Net::Loopback
    );
}

/// A host port of a start, at `job` as its job port.
fn host_port(port: u16, job: Option<u16>) -> HostPort {
    HostPort {
        host_port: port,
        container_port: job,
        protocol: PortProtocol::Tcp,
    }
}

/// The network lines of `profile`, in order, as the profile has them.
fn network_lines(profile: &Profile) -> Vec<String> {
    let lines = profile.text().lines();
    let network = lines.filter(|line| line.contains("network"));
    network.map(str::to_owned).collect()
}

/// Asserts that a start's host ports open their loopback ports in the profile of a run of
/// the level `none` under the ceiling `off`, which leaves it no other network: the `off`
/// rules, then one `localhost:PORT` allow per port, lowest first and each once (a port named
/// at its own number and one with no job port alike), then the floor's; and that the same
/// run without host ports has no such allow. Catches a native job refused its service under
/// the ceiling `off`, a host port that opens all of loopback, and one granted to a run that
/// named none.
#[test]
fn a_host_port_opens_its_loopback_port_under_the_ceiling_off() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let config = NativeConfig {
        network: Ceiling::Off,
        ..config()
    };
    let net = level(&config, NetworkLevel::Off, None);
    assert_eq!(net, Net::Off);
    let ports = [
        host_port(5432, Some(5432)),
        host_port(80, None),
        host_port(5432, None),
    ];
    let with = build(&config, &net, home, (&[], &[]), None, &ports).unwrap();
    let network = network_lines(&with);
    let wanted = [
        "(deny network*)",
        "(allow network-outbound (literal \"/private/var/run/syslog\"))",
        "(allow network* (remote ip \"localhost:80\"))",
        "(allow network* (remote ip \"localhost:5432\"))",
    ];
    assert_eq!(network[..wanted.len()], wanted, "{network:#?}");
    assert!(
        network[wanted.len()..]
            .iter()
            .all(|line| !line.contains("localhost")),
        "{network:#?}"
    );
    let without = build(&config, &net, home, (&[], &[]), None, &[]).unwrap();
    assert!(!without.text().contains("localhost"), "{}", without.text());
}

/// Asserts that host ports leave the profile of a run whose level already reaches loopback as
/// it is: under the ceilings `loopback` (a `none` job) and `open` (an `open` job) the profile
/// with a host port is the profile without, to the byte. Catches a host-port rule that would
/// narrow an open loopback, and a rule rendered where it cannot matter.
#[test]
fn a_host_port_adds_nothing_where_loopback_is_open() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    for (ceiling, asked) in [
        (Ceiling::Loopback, NetworkLevel::Off),
        (Ceiling::Open, NetworkLevel::Open),
    ] {
        let config = NativeConfig {
            network: ceiling,
            ..config()
        };
        let net = level(&config, asked, None);
        let ports = [host_port(5432, None)];
        let with = build(&config, &net, home, (&[], &[]), None, &ports).unwrap();
        let without = build(&config, &net, home, (&[], &[]), None, &[]).unwrap();
        assert_eq!(with.text(), without.text(), "{ceiling}");
        assert_eq!(with.params(), without.params(), "{ceiling}");
    }
}

/// Asserts that a host port the profile cannot name fails the build, naming the host ports:
/// port 0, which the admission never lets through (no `--host-port` range holds it), is
/// refused by the profile too, never dropped. Catches a run started without a host port it
/// named.
#[test]
fn a_host_port_the_profile_cannot_name_fails_the_build() {
    let home = Path::new("/srv/homes/kbf-acct-4");
    let ports = [host_port(0, None)];
    let why = build(&config(), &none(), home, (&[], &[]), None, &ports).unwrap_err();
    assert_eq!(why.to_string(), "the host ports: port 0 listed");
}
