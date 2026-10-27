//! The job flags: `--allow-host-jobs`, `--mount-root` and `--scratch-root` are checked
//! when the agent starts, before it says hello.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use crate::support::{Scratch, code, linux_host, run_scripted, welcome};

/// Runs the agent to its first wait with `extra` and returns (status, stderr, sent).
fn start(extra: &[&str]) -> (String, String, usize) {
    let (ran, sent, _) = run_scripted(extra, &linux_host(), vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// Asserts that good job flags are accepted and the agent registers as before, and that
/// the flags change nothing in the hello. It catches a flag that breaks a start.
#[test]
fn good_job_flags_start_the_agent() {
    let dir = Scratch::new("life-flags-good");
    let (data, other) = (dir.path("data"), dir.path("other"));
    let scratch = dir.path("scratch");
    for path in [&data, &other, &scratch] {
        fs::create_dir(path).unwrap();
    }
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let (data, other, scratch) = (
        format!("data={}", data.display()),
        format!("other={}", other.display()),
        scratch.display().to_string(),
    );
    let (status, err, sent) = start(&[
        "--allow-host-jobs",
        "--mount-root",
        &data,
        "--mount-root",
        &other,
        "--scratch-root",
        &scratch,
    ]);
    assert_eq!(status, code(0), "{err}");
    assert_eq!(sent, 2, "the hello, and the draining heartbeat of the stop");
}

/// Asserts that a wrong job flag ends the agent with status 2 before anything is sent,
/// naming the flag: a root that does not exist, a root name twice, a root name the wire
/// would refuse, a scratch root other users can enter, and one that is missing. It
/// catches a mount root or scratch root that is only found wrong when the first job
/// arrives.
#[test]
fn a_wrong_job_flag_stops_the_agent_before_it_registers() {
    let dir = Scratch::new("life-flags-bad");
    let data = dir.path("data");
    fs::create_dir(&data).unwrap();
    let open = dir.path("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    let good = format!("data={}", data.display());
    let missing = format!("data={}", dir.path("missing").display());
    let badname = format!("../x={}", data.display());
    let open = open.display().to_string();
    let absent = dir.path("absent").display().to_string();
    let cases: [(&[&str], &str); 5] = [
        (
            &["--mount-root", &missing],
            "--mount-root data: not an existing directory",
        ),
        (
            &["--mount-root", &good, "--mount-root", &good],
            "--mount-root data: given twice",
        ),
        (&["--mount-root", &badname], "not a usable root name"),
        (
            &["--scratch-root", &open],
            "--scratch-root: group and others must have no permission",
        ),
        (
            &["--scratch-root", &absent],
            "--scratch-root: cannot read the directory",
        ),
    ];
    for (extra, text) in cases {
        let (status, err, sent) = start(extra);
        assert_eq!(status, code(2), "{extra:?}");
        assert!(err.contains(text), "{extra:?}: {err}");
        assert_eq!(sent, 0, "{extra:?}");
    }
    let (status, err, sent) = start(&["--mount-root", "no-equals-sign"]);
    assert_eq!(status, code(2));
    assert!(err.contains("is not key=value"), "{err}");
    assert_eq!(sent, 0);
}
