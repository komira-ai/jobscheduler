//! The subordinate id range the jobs are mapped onto: what is read from the id files, what
//! is refused, and the host-wide claim that keeps two agents out of each other's range.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_agent::FileReader;
use js_agent::cli_runtime::{IdRange, JobUsers, Mapping, RANGE_LEN, RangeClaim, SubidError};

use crate::support::{FakeFiles, Scratch};

const SUBUID: &str = "alice:100000:65536\njobmap:231072:65536\njobmap:900000:65536\n";
const SUBGID: &str = "alice:100000:65536\njobmap:462144:65536\n";
const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\n+::::::\nbroken:x:abc:abc::/:/bin/sh\n";
const GROUP: &str = "root:x:0:\nalice:x:1000:\n";

/// Host files that give `jobmap` the uids from 231072 and the gids from 462144.
pub fn host() -> FakeFiles {
    FakeFiles::default()
        .with("/etc/subuid", SUBUID)
        .with("/etc/subgid", SUBGID)
        .with("/etc/passwd", PASSWD)
        .with("/etc/group", GROUP)
}

fn read(files: &FakeFiles, user: &str) -> Result<Mapping, SubidError> {
    Mapping::read(user, &|path| files.read(path))
}

/// `host()` with `subuid` as the first file's text and `subgid` as the second's.
fn with_subids(subuid: &str, subgid: &str) -> FakeFiles {
    host()
        .with("/etc/subuid", subuid)
        .with("/etc/subgid", subgid)
}

/// Asserts that the range is the first entry of the user in each file (a user with two
/// entries gets the first), that the uid range and the gid range are read separately, that
/// the lines of other users and the lines of the databases that name no id (a `+` line, a
/// line whose id is not a number) are not in the way, and that the Podman maps are the
/// container ids 0 to 65535 onto that range. It catches a range taken from the wrong
/// user or the wrong file, and a gid range taken from the uid file.
#[test]
fn the_range_is_the_first_entry_of_the_user() {
    let mapping = read(&host(), "jobmap").unwrap();
    assert_eq!(mapping, Mapping::new(231_072, 462_144));
    assert_eq!(
        (mapping.uid().start(), mapping.gid().start()),
        (231_072, 462_144)
    );
    assert_eq!(
        mapping.podman_args(),
        ["--uidmap", "0:231072:65536", "--gidmap", "0:462144:65536"]
    );
    assert_eq!(
        read(&host(), "alice").unwrap(),
        Mapping::new(100_000, 100_000)
    );
}

/// Asserts each way an entry can be unusable, with the error that names the user and the
/// file: no entry, an entry that is not `name:start:count`, too few ids (65535, where
/// 65536 is enough), a range from 0 (a container's root would be the host's), and a range
/// that runs past the largest id (the last one that fits is accepted). It catches a range
/// that is accepted and then maps a container id onto a host id.
#[test]
fn an_entry_the_agent_cannot_use_is_refused() {
    let (user, file) = ("jobmap".to_owned(), "/etc/subuid");
    let cases = [
        (
            "alice:1:65536\n",
            SUBGID,
            SubidError::NoEntry {
                user: user.clone(),
                file,
            },
        ),
        (
            "jobmap:231072:65536:1\n",
            SUBGID,
            SubidError::Malformed {
                user: user.clone(),
                file,
            },
        ),
        (
            "jobmap:abc:65536\n",
            SUBGID,
            SubidError::Malformed {
                user: user.clone(),
                file,
            },
        ),
        (
            "jobmap\n",
            SUBGID,
            SubidError::Malformed {
                user: user.clone(),
                file,
            },
        ),
        (
            "jobmap:231072:65535\n",
            SUBGID,
            SubidError::TooSmall {
                user: user.clone(),
                file,
                count: 65535,
            },
        ),
        (
            "jobmap:0:65536\n",
            SUBGID,
            SubidError::Identity {
                user: user.clone(),
                file,
            },
        ),
        (
            "jobmap:4294901760:65536\n",
            SUBGID,
            SubidError::PastTheEnd {
                user: user.clone(),
                file,
                start: 4_294_901_760,
            },
        ),
        (
            SUBUID,
            "jobmap:0:65536\n",
            SubidError::Identity {
                user: user.clone(),
                file: "/etc/subgid",
            },
        ),
        (
            SUBUID,
            "alice:1:65536\n",
            SubidError::NoEntry {
                user: user.clone(),
                file: "/etc/subgid",
            },
        ),
    ];
    for (subuid, subgid, expected) in cases {
        let said = read(&with_subids(subuid, subgid), "jobmap");
        assert_eq!(said, Err(expected.clone()), "{subuid:?} {subgid:?}");
        assert!(
            expected.to_string().starts_with("--container-subuid"),
            "{expected}"
        );
    }
    let last = with_subids("jobmap:4294901759:65536\n", SUBGID);
    assert_eq!(
        read(&last, "jobmap").unwrap().uid(),
        IdRange::of_job(4_294_901_759),
        "ids up to 4294967294 are valid"
    );
}

/// Asserts that a range which holds an id the host's databases name is refused, from its
/// first id to its last, and not one id outside it: a uid of `/etc/passwd` in the uid
/// range, a gid of `/etc/group` or the primary gid of a user in the gid range; and that a
/// uid in the gid range (and a gid in the uid range) is not in the way, since they are
/// different id spaces. It catches a container id that would be a host user's.
#[test]
fn a_range_that_holds_a_host_id_is_refused() {
    let uid_start = 231_072;
    let gid_start = 462_144;
    let passwd = |line: String| format!("{PASSWD}{line}\n");
    let group = |line: String| format!("{GROUP}{line}\n");
    let user_at = |id: u32| format!("u{id}:x:{id}:1000::/:/bin/sh");
    let user_with_group = |id: u32| format!("g{id}:x:1000:{id}::/:/bin/sh");
    let group_at = |id: u32| format!("g{id}:x:{id}:");
    for id in [uid_start, uid_start + 1, uid_start + 65_535] {
        let files = host().with("/etc/passwd", &passwd(user_at(id)));
        assert_eq!(
            read(&files, "jobmap"),
            Err(SubidError::Overlaps {
                user: "jobmap".to_owned(),
                file: "/etc/subuid",
                start: uid_start,
                database: "/etc/passwd",
                id,
            }),
            "uid {id}"
        );
    }
    for id in [gid_start, gid_start + 65_535] {
        let files = host().with("/etc/group", &group(group_at(id)));
        assert_eq!(
            read(&files, "jobmap"),
            Err(SubidError::Overlaps {
                user: "jobmap".to_owned(),
                file: "/etc/subgid",
                start: gid_start,
                database: "/etc/group",
                id,
            }),
            "gid {id}"
        );
        let files = host().with("/etc/passwd", &passwd(user_with_group(id)));
        assert_eq!(
            read(&files, "jobmap"),
            Err(SubidError::Overlaps {
                user: "jobmap".to_owned(),
                file: "/etc/subgid",
                start: gid_start,
                database: "/etc/passwd",
                id,
            }),
            "primary gid {id}"
        );
    }
    let outside = host()
        .with(
            "/etc/passwd",
            &passwd(format!(
                "{}\n{}\n{}\n{}",
                user_at(uid_start - 1),
                user_at(uid_start + 65_536),
                user_at(gid_start),
                user_with_group(uid_start),
            )),
        )
        .with(
            "/etc/group",
            &group(format!(
                "{}\n{}\n{}",
                group_at(gid_start - 1),
                group_at(gid_start + 65_536),
                group_at(uid_start)
            )),
        );
    assert_eq!(
        read(&outside, "jobmap"),
        Ok(Mapping::new(uid_start, gid_start))
    );
    let text = SubidError::Overlaps {
        user: "jobmap".to_owned(),
        file: "/etc/subuid",
        start: uid_start,
        database: "/etc/passwd",
        id: uid_start,
    }
    .to_string();
    assert!(
        text.contains("231072") && text.contains("/etc/passwd"),
        "{text}"
    );
}

/// Asserts that a file that cannot be read is an error naming it, for each of the four
/// files. It catches a missing file taken for an empty one, which would accept any range.
#[test]
fn an_unreadable_id_file_is_refused() {
    for path in ["/etc/subuid", "/etc/subgid", "/etc/passwd", "/etc/group"] {
        let files = host().without(path);
        let said = read(&files, "jobmap").unwrap_err();
        assert_eq!(
            said,
            SubidError::Unreadable(format!("{path}: No such file or directory"))
        );
        assert!(said.to_string().starts_with("--container-subuid"), "{said}");
    }
}

/// Asserts the arithmetic of a run of ids at its edges: the first and the last id are in
/// it, the ones beside them are not, two runs that share one id overlap and two that touch
/// do not, and a run at the top of the id space does not wrap. It catches an off-by-one
/// that lets the neighbour of a range pass as outside it.
#[test]
fn a_run_of_ids_has_exact_edges() {
    let one = IdRange::single(5);
    assert!(one.contains(5) && !one.contains(4) && !one.contains(6));
    let job = IdRange::of_job(10);
    let last = 10 + RANGE_LEN - 1;
    assert!(job.contains(10) && job.contains(last));
    assert!(!job.contains(9) && !job.contains(last + 1));
    assert!(job.overlaps(IdRange::of_job(last)));
    assert!(IdRange::of_job(last).overlaps(job));
    assert!(!job.overlaps(IdRange::of_job(last + 1)));
    assert!(!IdRange::of_job(last + 1).overlaps(job));
    assert!(job.overlaps(IdRange::single(10)));
    let top = IdRange::of_job(u32::MAX - 10);
    assert!(top.contains(u32::MAX) && top.overlaps(IdRange::single(u32::MAX)));
    assert!(!top.overlaps(job));
}

/// Asserts that the users of the jobs are the one uid for settings without a mapping and
/// the whole uid range for settings with one, read from the files it is given, and that a
/// range the files do not give is an error. It catches the post-job checks looking for the
/// wrong users.
#[test]
fn the_job_users_are_the_uid_range() {
    let files = host();
    let read_file = |path: &str| files.read(path);
    assert_eq!(
        JobUsers::Uid(65534).range(&read_file),
        Ok(IdRange::single(65534))
    );
    let mapped = JobUsers::Mapped("jobmap".to_owned());
    assert_eq!(mapped.range(&read_file), Ok(IdRange::of_job(231_072)));
    let unknown = JobUsers::Mapped("nobody-here".to_owned());
    assert!(matches!(
        unknown.range(&read_file),
        Err(SubidError::NoEntry { .. })
    ));
}

/// Asserts that a second claim of the same range is refused while the first is held, that
/// the range can be claimed again once the first is dropped, and that the directory is
/// made private. It catches two agents mapping their jobs onto one range.
#[test]
fn a_range_is_claimed_by_one_agent_at_a_time() {
    let dir = Scratch::new("subid-claim-same");
    let claims = dir.path("run/claims");
    let mapping = Mapping::new(100_000, 100_000);
    let first = RangeClaim::take(&claims, mapping).unwrap();
    let mode = fs::metadata(&claims).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert_eq!(
        RangeClaim::take(&claims, mapping).unwrap_err(),
        SubidError::Claimed("100000-100000".to_owned())
    );
    drop(first);
    RangeClaim::take(&claims, mapping).unwrap();
}

/// Asserts that a claim of a range that overlaps one that is held is refused, whichever
/// side overlaps (the uids, or only the gids, by one id at either end), and that ranges
/// that touch without sharing an id, a file left by an agent that has gone (nothing holds
/// its lock), and names in the directory that are no claim do not stand in the way. It
/// catches an overlap that goes unnoticed because the start differs, and a leftover file
/// that locks the range for good.
#[test]
fn overlapping_ranges_are_refused_and_neighbours_are_not() {
    let dir = Scratch::new("subid-claim-overlap");
    let claims = dir.path("claims");
    let held = RangeClaim::take(&claims, Mapping::new(100_000, 400_000)).unwrap();
    let refused = |uid: u32, gid: u32, other: &str| {
        assert_eq!(
            RangeClaim::take(&claims, Mapping::new(uid, gid)).unwrap_err(),
            SubidError::Claimed(other.to_owned()),
            "{uid} {gid}"
        );
    };
    refused(100_001, 900_000, "100000-400000");
    refused(100_000 + 65_535, 900_000, "100000-400000");
    refused(100_000 - 65_535, 900_000, "100000-400000");
    refused(900_000, 400_001, "100000-400000");
    refused(900_000, 400_000 - 65_535, "100000-400000");
    let touching = [
        Mapping::new(100_000 + 65_536, 900_000),
        Mapping::new(100_000 - 65_536, 400_000 + 65_536),
    ];
    let mut kept = Vec::new();
    for mapping in touching {
        kept.push(RangeClaim::take(&claims, mapping).unwrap());
    }

    dir.write("claims/notes.txt", "x", 0o600);
    dir.write("claims/1-x", "x", 0o600);
    dir.write("claims/300000-300000", "", 0o600);
    let stale = RangeClaim::take(&claims, Mapping::new(300_010, 700_000)).unwrap();
    drop((held, kept, stale));
    RangeClaim::take(&claims, Mapping::new(100_001, 400_001)).unwrap();
}

/// Asserts that a claim directory that cannot be made is an error that says so, and not a
/// claim. It catches a refusal to claim that the agent ignores.
#[test]
fn a_claim_directory_that_cannot_be_made_is_an_error() {
    let dir = Scratch::new("subid-claim-dir");
    let file = dir.write("file", "x", 0o600);
    let said = RangeClaim::take(&file.join("claims"), Mapping::new(100_000, 100_000));
    assert!(matches!(said, Err(SubidError::Claim(_))), "{said:?}");
    assert!(
        said.unwrap_err()
            .to_string()
            .starts_with("--container-subuid")
    );
}
