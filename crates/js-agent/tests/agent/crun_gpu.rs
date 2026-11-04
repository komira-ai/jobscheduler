//! The GPU flags of `--runtime crun` at the start of the whole agent: each one that cannot work
//! ends the agent with status 2 naming the flag, before the host is checked or anything is
//! sent. The host the agent detects is `linux_host`'s, with two GPUs (indices 0 and 1). The
//! nodes are the machine's own: `/dev/null` stands for a GPU's node (a character device every
//! Linux host has) and `/dev/pts` for a path below `/dev` that is not one. (Linux only: only
//! Linux runs containers.)

use crate::support::{code, linux_host, mac_host, run_scripted, welcome};

/// The flags of a crun agent that pass every check before the GPU flags'.
const CRUN: [&str; 8] = [
    "--runtime",
    "crun",
    "--job-user",
    "jsjob",
    "--state-root",
    "/s",
    "--scratch-root",
    "/r",
];

/// Runs the agent to its first wait with `extra` on `host`; returns (status, stderr, whether
/// it sent anything).
fn run_agent(extra: &[&str], host: &crate::support::Host) -> (String, String, bool) {
    let (ran, sent, _) = run_scripted(extra, host, vec![welcome(1)], 1);
    let sent = !sent.borrow().is_empty();
    (ran.status(), ran.err, sent)
}

/// Proves each GPU flag that cannot work stops the agent with status 2, naming the flag, and
/// sends nothing: a relative device path, one outside `/dev`, one with `..`, a second
/// `{index}`, a path that is not a character device, a missing node, a mount source below
/// `/proc`, a mount source that is a file, two mounts at one destination, a reserved
/// variable, `--gpu-mount` and `--gpu-env` without `--gpu-device`, and the flags with
/// `--runtime docker`, with the engine by default and on a Mac. Catches a flag that is accepted and then ignored, a check that runs after the
/// host registered, and a runtime that ignores GPU flags it cannot honour.
#[test]
fn gpu_flags_are_validated_naming_the_flag() {
    let tmp = std::fs::canonicalize(std::env::temp_dir()).expect("the temporary directory");
    let tmp = tmp.display().to_string();
    let twice = format!("{tmp}:/usr/local/driver");
    let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "crun-gpu-file-source");
    let library = std::fs::canonicalize(dir.path())
        .expect("canonical")
        .join("libdriver.so.1");
    std::fs::write(&library, b"").expect("a file");
    let library = library.display().to_string();
    let file_source = format!("{library}:/usr/local/driver/libdriver.so.1");
    let crun_cases: Vec<(Vec<&str>, String)> = vec![
        (
            vec!["--gpu-device", "dev/nvidia0"],
            "--gpu-device: dev/nvidia0 is not an absolute path below /dev".to_owned(),
        ),
        (
            vec!["--gpu-device", "/tmp/nvidia0"],
            "--gpu-device: /tmp/nvidia0 is not an absolute path below /dev".to_owned(),
        ),
        (
            vec!["--gpu-device", "/dev/../dev/null"],
            "--gpu-device: /dev/../dev/null is not an absolute path below /dev".to_owned(),
        ),
        (
            vec!["--gpu-device", "/dev/nvidia{index}-{index}"],
            "--gpu-device: /dev/nvidia{index}-{index} names {index} more than once".to_owned(),
        ),
        (
            vec!["--gpu-device", "/dev/pts"],
            "--gpu-device: /dev/pts is not a character device".to_owned(),
        ),
        (
            vec!["--gpu-device", "/dev/null{index}"],
            "--gpu-device: /dev/null0: No such file or directory".to_owned(),
        ),
        (
            vec!["--gpu-device", "/dev/null", "--gpu-mount", "/proc:/driver"],
            "--gpu-mount: /proc:/driver: the source /proc is below /proc".to_owned(),
        ),
        (
            vec![
                "--gpu-device",
                "/dev/null",
                "--gpu-mount",
                &twice,
                "--gpu-mount",
                &twice,
            ],
            format!("--gpu-mount: {twice}: another mount has the destination /usr/local/driver"),
        ),
        (
            vec!["--gpu-device", "/dev/null", "--gpu-mount", &file_source],
            format!("--gpu-mount: {file_source}: the source {library} is not a directory"),
        ),
        (
            vec!["--gpu-device", "/dev/null", "--gpu-env", "JS_RUN=1"],
            "--gpu-env: JS_RUN is reserved".to_owned(),
        ),
        (
            vec!["--gpu-mount", "/proc:/driver"],
            "--gpu-mount: needs --gpu-device".to_owned(),
        ),
        (
            vec!["--gpu-env", "A=1"],
            "--gpu-env: needs --gpu-device".to_owned(),
        ),
    ];
    for (flags, why) in crun_cases {
        let mut args = CRUN.to_vec();
        args.extend(&flags);
        let (status, err, sent) = run_agent(&args, &linux_host());
        assert_eq!(status, code(2), "{flags:?}: {err}");
        assert!(
            err.contains(&format!("js-agent: {why}")),
            "{flags:?}: {err}"
        );
        assert!(!sent, "{flags:?}: the agent sent a hello");
    }
    let engine = ["--runtime", "docker", "--container-bin", "/usr/bin/docker"];
    let others: Vec<(Vec<&str>, &str)> = vec![
        (
            [&engine[..], &["--gpu-device", "/dev/null"][..]].concat(),
            "--gpu-device: needs --runtime crun",
        ),
        (
            [&engine[..], &["--gpu-mount", "/proc:/driver"][..]].concat(),
            "--gpu-mount: needs --runtime crun",
        ),
        (
            [&engine[..], &["--gpu-env", "A=1"][..]].concat(),
            "--gpu-env: needs --runtime crun",
        ),
        (
            vec!["--gpu-device", "/dev/null"],
            "--gpu-device: needs --runtime crun",
        ),
    ];
    for (args, why) in others {
        let (status, err, sent) = run_agent(&args, &linux_host());
        assert_eq!(status, code(2), "{args:?}: {err}");
        assert!(err.contains(why), "{args:?}: {err}");
        assert!(!sent, "{args:?}: the agent sent a hello");
    }
    let (status, err, _) = run_agent(&["--gpu-device", "/dev/null"], &mac_host());
    assert_eq!(status, code(2), "{err}");
    assert!(err.contains("--gpu-device: needs --runtime crun"), "{err}");
}
