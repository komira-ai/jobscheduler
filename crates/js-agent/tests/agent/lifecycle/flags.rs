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

/// Asserts that `--allow-host-jobs` ends the agent with status 2 before anything is sent,
/// and says why: the scheduler does not record a host job's taint yet. It catches an agent
/// that runs jobs outside a container before the scheduler can tell the host is tainted.
#[test]
fn host_jobs_are_not_available_yet() {
    let (status, err, sent) = start(&["--allow-host-jobs"]);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains(
            "--allow-host-jobs: host jobs are not available yet: the scheduler does not record a host job's taint"
        ),
        "{err}"
    );
    assert_eq!(sent, 0);
}

/// The user containers run as, for an agent of any user.
fn container_user() -> String {
    let uid = rustix::process::geteuid().as_raw();
    match uid {
        0 => "1:1".to_owned(),
        _ => format!("{uid}:{}", rustix::process::getegid().as_raw()),
    }
}

/// Runs the agent on `host` with `extra` and returns (status, stderr, messages sent).
fn start_on(host: &crate::support::Host, extra: &[&str]) -> (String, String, usize) {
    let (ran, sent, _) = run_scripted(extra, host, vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// Asserts that the state root and the watched directories are checked when the agent
/// starts, each wrong one ending it with status 2 before anything is sent and naming the
/// flag: containers without a state root, a watched directory without containers, a state
/// root other users can enter, a watched path that is relative (one that exists too),
/// missing or a file, and a
/// watched directory that is, holds or lies in the state root, the scratch root or a mount
/// root. It catches a taint marker kept where it cannot be trusted, a watch that would
/// taint the host after every job, and a flag that is only found wrong when the first job
/// ends.
#[test]
fn the_state_root_and_the_watched_directories_are_checked_at_start() {
    let dir = Scratch::new("life-flags-state");
    let (state, data, scratch, open) = (
        dir.path("state"),
        dir.path("data"),
        dir.path("scratch"),
        dir.path("open"),
    );
    for path in [&state, &data, &scratch, &open, &dir.path("watched")] {
        fs::create_dir(path).unwrap();
    }
    for path in [&state, &scratch] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(data.join("inner")).unwrap();
    let plain = dir.write("plain-file", "x", 0o600);
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let user = container_user();
    let s = |path: &std::path::Path| path.display().to_string();
    let (exec, state_s, scratch_s, open_s) = (s(&exec), s(&state), s(&scratch), s(&open));
    let (watched, plain, whole) = (s(&dir.path("watched")), s(&plain), s(&dir.path("")));
    let (data_s, inner) = (format!("data={}", data.display()), s(&data.join("inner")));
    let missing = s(&dir.path("missing"));
    let containers: Vec<&str> = vec!["--container-bin", &exec, "--container-user", &user];
    fn with<'a>(base: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
        [base, extra].concat()
    }
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            containers.clone(),
            "--state-root: required with --container-bin",
        ),
        (
            vec!["--watch-dir", &watched],
            "--watch-dir: needs --container-bin",
        ),
        (
            vec!["--state-root", &open_s],
            "--state-root: group and others must have no permission",
        ),
        (
            with(
                &containers,
                &["--state-root", &state_s, "--watch-dir", "relative/dir"],
            ),
            "not an existing directory given as an absolute path",
        ),
        (
            with(&containers, &["--state-root", &state_s, "--watch-dir", "."]),
            "not an existing directory given as an absolute path",
        ),
        (
            with(
                &containers,
                &["--state-root", &state_s, "--watch-dir", &missing],
            ),
            "not an existing directory given as an absolute path",
        ),
        (
            with(
                &containers,
                &["--state-root", &state_s, "--watch-dir", &plain],
            ),
            "not an existing directory given as an absolute path",
        ),
        (
            with(
                &containers,
                &["--state-root", &state_s, "--watch-dir", &state_s],
            ),
            "lies in or holds a --mount-root, --scratch-root or --state-root",
        ),
        (
            with(
                &containers,
                &["--state-root", &state_s, "--watch-dir", &whole],
            ),
            "lies in or holds a --mount-root, --scratch-root or --state-root",
        ),
        (
            with(
                &containers,
                &[
                    "--state-root",
                    &state_s,
                    "--scratch-root",
                    &scratch_s,
                    "--watch-dir",
                    &scratch_s,
                ],
            ),
            "lies in or holds a --mount-root, --scratch-root or --state-root",
        ),
        (
            with(
                &containers,
                &[
                    "--state-root",
                    &state_s,
                    "--mount-root",
                    &data_s,
                    "--watch-dir",
                    &inner,
                ],
            ),
            "lies in or holds a --mount-root, --scratch-root or --state-root",
        ),
    ];
    for (extra, text) in cases {
        let (status, err, sent) = start(&extra);
        assert_eq!(status, code(2), "{extra:?}");
        assert!(err.contains(text), "{extra:?}: {err}");
        assert_eq!(sent, 0, "{extra:?}");
    }

    let twice = format!("{watched}/./");
    let good = with(
        &containers,
        &[
            "--state-root",
            &state_s,
            "--watch-dir",
            &watched,
            "--watch-dir",
            &twice,
        ],
    );
    let (status, err, sent) = start(&good);
    assert_eq!(status, code(0), "{err}");
    assert_eq!(sent, 2);
    let (status, err, _) = start(&["--state-root", &state_s]);
    assert_eq!(status, code(0), "a state root needs no containers: {err}");
}

/// Asserts that a Mac refuses `--container-bin` when it starts, with status 2 and nothing
/// sent, saying why (the checks read `/proc`), and that the same flags start the agent on a
/// Linux host. It catches a Mac agent that would run container jobs with no post-job check.
#[test]
fn container_jobs_are_refused_on_a_mac() {
    let dir = Scratch::new("life-flags-mac");
    let state = dir.path("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let (state, exec, user) = (
        state.display().to_string(),
        exec.display().to_string(),
        container_user(),
    );
    let args = [
        "--container-bin",
        &exec,
        "--container-user",
        &user,
        "--state-root",
        &state,
    ];
    let (status, err, sent) = start_on(&crate::support::mac_host(), &args);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--container-bin: container jobs are Linux-only"),
        "{err}"
    );
    assert_eq!(sent, 0);
    let (status, err, _) = start_on(&crate::support::mac_host(), &["--state-root", &state]);
    assert_eq!(
        status,
        code(0),
        "a Mac without containers runs no checks: {err}"
    );
    let (status, err, _) = start_on(&linux_host(), &args);
    assert_eq!(status, code(0), "{err}");
}

/// Asserts that a taint marker that is already in the state root is told to the operator
/// when the agent starts, by its reason, and does not stop the agent: it registers, and
/// starts no job. It catches a restarted agent that says nothing about its taint.
#[test]
fn a_taint_marker_found_at_start_is_reported() {
    let dir = Scratch::new("life-flags-tainted");
    let state = dir.path("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let state_s = state.display().to_string();
    let (status, err, _) = start(&["--state-root", &state_s]);
    assert_eq!(status, code(0), "{err}");
    assert!(
        !err.contains("tainted"),
        "a clean state root says nothing: {err}"
    );
    fs::write(
        state.join("taint.json"),
        r#"{"reason":"host-job","run_id":"r1"}"#,
    )
    .unwrap();
    let (status, err, sent) = start(&["--state-root", &state_s]);
    assert_eq!(status, code(0), "{err}");
    assert!(
        err.contains(
            "js-agent: warning: this host is tainted (host-job): the agent starts no job on it"
        ),
        "{err}"
    );
    assert_eq!(sent, 2, "it still registers");
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
