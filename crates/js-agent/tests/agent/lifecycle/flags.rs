//! The job flags: `--allow-host-jobs`, `--mount-root` and `--scratch-root` are checked
//! when the agent starts, before it says hello.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use js_agent::lifecycle::{
    DEFAULT_SILENCE_STOP, DEFAULT_STOP_GRACE, MAX_SILENCE_STOP_SECS, SILENCE_STOP_GRACE_RATIO,
    fits_grace, silence_stop,
};
use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, RETRY_AFTER_CAP};
use js_oci::Allow;

use super::quota::local_with;
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
    let containers: Vec<&str> = vec![
        "--container-bin",
        &exec,
        "--container-user",
        &user,
        "--container-subuid",
        "jobmap",
    ];
    fn with<'a>(base: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
        [base, extra].concat()
    }
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            containers.clone(),
            "--state-root: required with --container-bin",
        ),
        (
            with(&containers, &["--state-root", &state_s]),
            "--scratch-root: required with --container-bin",
        ),
        (
            vec!["--watch-dir", &watched],
            "--watch-dir: needs --container-bin",
        ),
        (
            vec!["--stop-grace", "5"],
            "--stop-grace and --drain-wait: need --container-bin",
        ),
        (
            vec!["--drain-wait", "5"],
            "--stop-grace and --drain-wait: need --container-bin",
        ),
        (
            with(
                &containers,
                &[
                    "--state-root",
                    &state_s,
                    "--scratch-root",
                    &scratch_s,
                    "--stop-grace",
                    "3601",
                ],
            ),
            "--stop-grace: at most 3600 seconds",
        ),
        (
            with(
                &containers,
                &[
                    "--state-root",
                    &state_s,
                    "--scratch-root",
                    &scratch_s,
                    "--drain-wait",
                    "3601",
                ],
            ),
            "--drain-wait: at most 3600 seconds",
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
            "--scratch-root",
            &scratch_s,
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

/// Asserts that a state root that is, or lies in, a mount root or the scratch root ends the
/// agent with status 2 before anything is sent and names `--state-root`, and that a state
/// root that holds a mount root or the scratch root starts it (a job sees only the subtree
/// it is given). It catches a taint marker in a directory that a job with a read-write
/// mount can delete or overwrite.
#[test]
fn a_state_root_inside_a_directory_a_job_can_write_is_refused() {
    let dir = Scratch::new("life-flags-state-inside");
    let (data, inner, scratch, held) = (
        dir.path("data"),
        dir.path("data/state"),
        dir.path("scratch"),
        dir.path("held"),
    );
    for path in [&data, &inner, &scratch, &held, &held.join("data")] {
        fs::create_dir(path).unwrap();
    }
    fs::create_dir(scratch.join("state")).unwrap();
    for path in [&data, &inner, &scratch, &scratch.join("state"), &held] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let s = |path: &std::path::Path| path.display().to_string();
    let (data_s, scratch_s) = (s(&data), s(&scratch));
    let (mount, inside) = (format!("data={data_s}"), s(&inner));
    let (below_scratch, held_s) = (s(&scratch.join("state")), s(&held));
    let held_mount = format!("data={}", held.join("data").display());
    let refused: Vec<Vec<&str>> = vec![
        vec!["--mount-root", &mount, "--state-root", &data_s],
        vec!["--mount-root", &mount, "--state-root", &inside],
        vec!["--scratch-root", &scratch_s, "--state-root", &scratch_s],
        vec!["--scratch-root", &scratch_s, "--state-root", &below_scratch],
    ];
    for extra in refused {
        let (status, err, sent) = start(&extra);
        assert_eq!(status, code(2), "{extra:?}");
        assert!(
            err.contains("--state-root: lies in a --mount-root or the --scratch-root"),
            "{extra:?}: {err}"
        );
        assert_eq!(sent, 0, "{extra:?}");
    }
    let (status, err, _) = start(&["--mount-root", &held_mount, "--state-root", &held_s]);
    assert_eq!(status, code(0), "a state root may hold a mount root: {err}");
}

/// Asserts that on a system other than Linux (this test is built only there, so the macOS
/// job runs it) the agent, with the platform this binary was built for, refuses
/// `--container-bin` when it starts: status 2, the flag named, the reason given, nothing
/// sent. It catches a Mac build whose own platform is not what the refusal tests, so that
/// it would run container jobs where the post-job checks, which read `/proc`, cannot work.
#[cfg(not(target_os = "linux"))]
#[test]
fn this_platform_refuses_container_jobs() {
    let dir = Scratch::new("life-flags-this-platform");
    let state = dir.path("state");
    let scratch = dir.path("scratch");
    for path in [&state, &scratch] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let (state, exec, user, scratch) = (
        state.display().to_string(),
        exec.display().to_string(),
        container_user(),
        scratch.display().to_string(),
    );
    let mut host = crate::support::mac_host();
    host.platform = js_agent::Platform::current();
    let args = [
        "--container-bin",
        &exec,
        "--container-user",
        &user,
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ];
    let (status, err, sent) = start_on(&host, &args);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--container-bin: container jobs are Linux-only"),
        "{err}"
    );
    assert_eq!(sent, 0);
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
    let scratch = dir.path("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let (state, exec, user, scratch) = (
        state.display().to_string(),
        exec.display().to_string(),
        container_user(),
        scratch.display().to_string(),
    );
    let args = [
        "--container-bin",
        &exec,
        "--container-user",
        &user,
        "--container-subuid",
        "jobmap",
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
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

/// Asserts that `--image-allow` entries reach the job flags, each parsed, in the order given,
/// and that none given means none: an image on no entry is never pulled. It catches a flag
/// that is read and dropped, and entries reordered or merged.
#[test]
fn image_allow_entries_reach_the_job_flags_in_order() {
    let local = local_with(
        "image-allow",
        &[
            "--image-allow",
            "registry.example:5000/team/",
            "--image-allow",
            "ghcr.io/komira-ai/",
        ],
    );
    assert_eq!(
        local.image_allow,
        [
            Allow::parse("registry.example:5000/team/").unwrap(),
            Allow::parse("ghcr.io/komira-ai/").unwrap(),
        ]
    );
    assert!(local_with("image-allow-none", &[]).image_allow.is_empty());
}

/// Asserts that an `--image-allow` entry that names a bare host (every account on it) or is
/// not a registry reference ends the agent with status 2 before it registers, and says
/// which flag. It catches an allow-list that is only found wrong at the first pull.
#[test]
fn a_wrong_image_allow_entry_stops_the_agent_before_it_registers() {
    for entry in ["ghcr.io", "ghcr.io/Komira AI/"] {
        let (status, err, sent) = start(&["--image-allow", entry]);
        assert_eq!(status, code(2), "{entry}");
        assert!(
            err.contains(&format!(
                "--image-allow: {entry:?} is not a well formed registry reference"
            )),
            "{entry}: {err}"
        );
        assert_eq!(sent, 0, "{entry}");
    }
}

/// Asserts that `--registry-credentials` is read when the agent starts: a good file for a
/// listed entry starts the agent as before, and a file other users may read, or one not
/// owned by the user it must be owned by (root outside a test), ends it with status 2
/// before it registers, naming the flag and the file and holding nothing of the content.
/// It catches a flag that is read only at the first pull, and a secret file any local user
/// could read being used.
#[test]
fn registry_credentials_are_checked_before_the_agent_registers() {
    let dir = Scratch::new("life-flags-credentials");
    // The file's directory must not be writable by group or others either.
    fs::set_permissions(dir.path("."), fs::Permissions::from_mode(0o700)).unwrap();
    let good = dir.write("good", "puller:s3cret-pw\n", 0o600);
    let loose = dir.write("loose", "puller:s3cret-pw\n", 0o644);
    let me = rustix::process::geteuid().as_raw();
    let mut host = linux_host();
    host.secret_owner = me;
    let flag = |path: &std::path::Path| format!("registry.example/team={}", path.display());
    let with = |path: &std::path::Path| {
        vec![
            "--image-allow".to_owned(),
            "registry.example/team".to_owned(),
            "--registry-credentials".to_owned(),
            flag(path),
        ]
    };
    let started = |host: &crate::support::Host, path: &std::path::Path| {
        let args = with(path);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        start_on(host, &args)
    };

    let (status, err, sent) = started(&host, &good);
    assert_eq!(status, code(0), "{err}");
    assert_eq!(sent, 2, "the hello, and the draining heartbeat of the stop");

    let rule = "not owned by the expected user, or readable or writable by group or others";
    let mut not_root = host.clone();
    not_root.secret_owner = me.wrapping_add(1);
    for (host, path) in [(&host, &loose), (&not_root, &good)] {
        let (status, err, sent) = started(host, path);
        assert_eq!(status, code(2), "{err}");
        // The credentials are read before anything else is said, so this is all of it.
        assert_eq!(
            err,
            format!(
                "js-agent: --registry-credentials: {}: {rule}\n",
                path.display()
            )
        );
        assert_eq!(sent, 0, "{err}");
    }
}

/// Asserts that the job flags, read on their own as a caller other than the start of the
/// agent reads them, refuse an `--image-allow` entry that is not one and name the flag
/// (the agent's start refuses it earlier, with the same words). It catches job flags that
/// take a bare host when they are read without the start's check.
#[test]
fn the_job_flags_alone_refuse_a_wrong_image_allow_entry() {
    use clap::Parser as _;

    let dir = Scratch::new("life-flags-image-allow");
    let root = dir.path("scratch");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let root = root.display().to_string();
    let argv = [
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
        "--pool",
        "p",
        "--scratch-root",
        &root,
        "--scratch-quota",
        "--image-allow",
        "ghcr.io",
    ];
    let cli = js_agent::Cli::try_parse_from(argv).unwrap();
    let why = js_agent::lifecycle::Local::from_cli(&cli, 0).unwrap_err();
    assert_eq!(
        why,
        "--image-allow: \"ghcr.io\" is not a well formed registry reference"
    );
}

/// Asserts the default silence bound against the numbers it is chosen from: at least
/// [`SILENCE_STOP_GRACE_RATIO`] (twenty) times the server's default grace, above what a
/// server restart costs a well agent beside the restart's own time (a heartbeat interval,
/// the cap on `Retry-After` and the old session's grace) plus the stop grace, taken by a
/// welcome of the default grace, and within the flag's bounds. It catches an edit of the
/// default (or of a number it rests on) that slips it under the grace, so that a routine
/// server restart or a lost reply would stop every job of the fleet.
#[test]
fn silence_stop_default_is_well_above_the_grace() {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap();
    let default = ms(DEFAULT_SILENCE_STOP);
    assert_eq!(SILENCE_STOP_GRACE_RATIO, 20);
    assert!(default >= SILENCE_STOP_GRACE_RATIO * DEFAULT_GRACE_MS);
    let restart = DEFAULT_HEARTBEAT_INTERVAL_MS + ms(RETRY_AFTER_CAP) + DEFAULT_GRACE_MS;
    assert!(
        default > restart + ms(DEFAULT_STOP_GRACE),
        "{default} {restart}"
    );
    assert_eq!(
        fits_grace(DEFAULT_SILENCE_STOP, DEFAULT_GRACE_MS, DEFAULT_STOP_GRACE),
        Ok(())
    );
    assert_eq!(silence_stop(None), Ok(DEFAULT_SILENCE_STOP));
    assert!(DEFAULT_SILENCE_STOP.as_secs() <= MAX_SILENCE_STOP_SECS);
}

/// Asserts the bounds of `--silence-stop`: 1 and 7200 seconds are taken, 0 (there is no
/// value that turns the stop off) and 7201 are refused naming the flag, a bound exactly at
/// the grace plus the stop grace is refused and one a millisecond above is taken, and the
/// flag on an agent that runs no job ends it with status 2 before anything is sent. It
/// catches a bound that switches the stop off, one without a ceiling, an off-by-one at the
/// grace, and a flag taken where nothing would be stopped.
#[test]
fn the_silence_stop_flag_is_bounded() {
    assert_eq!(silence_stop(Some(1)), Ok(Duration::from_secs(1)));
    assert_eq!(silence_stop(Some(7200)), Ok(Duration::from_secs(7200)));
    let zero = silence_stop(Some(0)).unwrap_err();
    assert!(
        zero.starts_with("--silence-stop: at least 1 second"),
        "{zero}"
    );
    assert_eq!(
        silence_stop(Some(7201)),
        Err("--silence-stop: at most 7200 seconds".to_owned())
    );
    let (bound, grace) = (Duration::from_secs(25), Duration::from_secs(1));
    assert!(fits_grace(bound, 24_000, grace).is_err());
    assert_eq!(fits_grace(bound, 23_999, grace), Ok(()));

    let (status, err, sent) = start(&["--silence-stop", "300"]);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--silence-stop: needs --container-bin or --native-helper-socket"),
        "{err}"
    );
    assert_eq!(sent, 0);
}

/// Asserts that an agent whose stop grace the silence bound does not outlast, against the
/// server's default grace, ends at start with status 2 before anything is sent, naming
/// `--silence-stop` and the numbers: `--stop-grace 285` with the default bound of 300 s
/// (285 s and 15 s are exactly 300 s). It catches an agent that registers with a bound the
/// default grace could trip, and then ends at every welcome instead of once, at start.
#[test]
fn a_stop_grace_the_silence_bound_does_not_outlast_is_refused_at_start() {
    let dir = Scratch::new("life-flags-silence-grace");
    let (state, scratch) = (dir.path("state"), dir.path("scratch"));
    for path in [&state, &scratch] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let user = container_user();
    let s = |path: &std::path::Path| path.display().to_string();
    let (exec, state, scratch) = (s(&exec), s(&state), s(&scratch));
    let (status, err, sent) = start(&[
        "--container-bin",
        &exec,
        "--container-user",
        &user,
        "--container-subuid",
        "jobmap",
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
        "--stop-grace",
        "285",
    ]);
    assert_eq!(status, code(2), "{err}");
    let said = "--silence-stop: 300000 ms is at or below the server's grace of 15000 ms plus the \
                stop grace of 285000 ms";
    assert!(err.contains(said), "{err}");
    assert_eq!(sent, 0);
}
