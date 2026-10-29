//! The check for a job's leftover processes when the jobs run in a user namespace mapped
//! onto a subordinate range: it looks for the whole range of host uids.

use js_agent::cli_runtime::{JobUsers, Mapping, Settings};
use js_agent::integrity::{FakeHost, Integrity, code};

use super::{after, beside, process, said};

/// The first uid of the range of `jobmap` in these tests.
const START: u32 = 231_072;

/// The users of the jobs: the range the runtime resolved, from `START`.
fn jobmap() -> JobUsers {
    JobUsers::resolved(Mapping::new(START, 462_144))
}

/// A quiet host.
fn host() -> FakeHost {
    FakeHost::quiet()
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

/// Asserts that the checks do not start for jobs whose range no runtime resolved (the
/// settings were not used to open one), and that they take the range from what was
/// resolved and not from the id files of the host: files that name another range are not
/// read, so a process in the resolved range is found and one in the files' range is not.
/// It catches checks that watch a range the jobs do not run in.
#[test]
fn the_checks_use_the_resolved_range_and_never_read_the_id_files() {
    let unresolved = Settings::new(std::path::Path::new("/bin/sh"), None, None)
        .unwrap()
        .with_subuid("jobmap")
        .unwrap();
    let host = host();
    let said = Integrity::start(Box::new(host.clone()), unresolved.uid(), vec![]);
    assert!(said.err().unwrap().contains("not been resolved"));

    host.set_file("/etc/subuid", "jobmap:900000:65536\n");
    host.set_file("/etc/subgid", "jobmap:900000:65536\n");
    let mut checks = started(&host);
    host.start_process(process(720, 100, 900_000 + 5, 900, "0::/user.slice"));
    assert!(
        after(&mut checks, "r1").is_ok(),
        "the files' range is not watched"
    );
    host.start_process(process(721, 100, START + 5, 901, "0::/user.slice"));
    assert!(after(&mut checks, "r2").is_err(), "the resolved range is");
}
