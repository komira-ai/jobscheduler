//! The check for a job's leftover processes when the jobs run in a user namespace mapped
//! onto a subordinate range: it looks for the whole range of host uids.

use js_agent::cli_runtime::JobUsers;
use js_agent::integrity::{FakeHost, Integrity, code};

use super::{after, beside, process, said};

/// The first uid of the range of `jobmap` in these tests.
const START: u32 = 231_072;

/// The users of the jobs: the range of `jobmap`.
fn jobmap() -> JobUsers {
    JobUsers::Mapped("jobmap".to_owned())
}

/// A quiet host whose id files give `jobmap` the uids from `START` and the gids from 462144.
fn host() -> FakeHost {
    let host = FakeHost::quiet();
    host.set_file("/etc/subuid", "jobmap:231072:65536\n");
    host.set_file("/etc/subgid", "jobmap:462144:65536\n");
    host.set_file("/etc/passwd", "root:x:0:0:root:/root:/bin/sh\n");
    host.set_file("/etc/group", "root:x:0:\n");
    host
}

fn started(host: &FakeHost) -> Integrity {
    Integrity::start(Box::new(host.clone()), jobmap(), vec![]).unwrap()
}

/// Asserts that a process the baseline did not have is a finding when its host uid is the
/// first, the last or the job's own id (`START + 65534`) of the mapped range, whatever its
/// parent, and that the finding names the pid and the host uid. It catches a leftover
/// process of a mapped job that the check does not see, which is what comparing with the
/// container user's id alone would do.
#[test]
fn a_leftover_process_in_the_mapped_range_is_found() {
    for uid in [START, START + 65_534, START + 65_535] {
        let host = host();
        let mut checks = started(&host);
        host.start_process(process(
            700,
            100,
            uid,
            900,
            "0::/system.slice/leftover.service",
        ));
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(
            said(&found),
            (
                true,
                code::PROCESS_AS_JOB_USER,
                format!("process-as-job-user: pid 700 of user {uid} is running after job r1")
            )
        );
    }
}

/// Asserts that processes outside the range are not the job's: the uid just below the
/// range, the one just above it, root, the container user's own id (65534, which on the
/// host is somebody else's when the jobs are mapped), and the agent's. A process of the
/// range that belongs to another job's container is not found either. It catches a check
/// that flags the host's own users and a mapped job's neighbour.
#[test]
fn a_process_outside_the_mapped_range_is_not_found() {
    let host = host();
    let mut checks = started(&host);
    for (pid, uid) in [
        (701, START - 1),
        (702, START + 65_536),
        (703, 0),
        (704, 65_534),
        (705, 1000),
    ] {
        host.start_process(process(
            pid,
            100,
            uid,
            900 + u64::from(pid),
            "0::/user.slice",
        ));
    }
    assert!(after(&mut checks, "r1").is_ok());

    host.start_process(process(710, 1, START + 5, 950, "0::/docker/c2"));
    assert!(beside(&mut checks, "r2", &["c2"]).is_ok());
}

/// Asserts that the checks do not start on a host where the range is not acceptable (the
/// files give the user no range, or the range begins at 0, or holds a user of the host),
/// with the reason, since they would look for the wrong users. It catches a baseline taken
/// for a range that maps container ids onto host users.
#[test]
fn the_checks_do_not_start_on_an_unusable_range() {
    let refused = |host: &FakeHost| {
        Integrity::start(Box::new(host.clone()), jobmap(), vec![])
            .err()
            .unwrap()
    };
    let host = host();
    host.remove_file("/etc/subuid");
    assert!(refused(&host).starts_with("--container-subuid: cannot read"));
    let host = self::host();
    host.set_file("/etc/subuid", "jobmap:0:65536\n");
    assert!(refused(&host).contains("begins at 0"));
    let host = self::host();
    host.set_file("/etc/passwd", "alice:x:231100:1000::/:/bin/sh\n");
    assert!(refused(&host).contains("holds the id 231100"));
}
