//! The GPU flags of the crun runtime at the start of the whole agent: each one that cannot work
//! ends the agent with status 2 naming the flag, before the host is checked or anything is
//! sent. The host the agent detects is `linux_host`'s, with two GPUs (indices 0 and 1). The
//! nodes are the machine's own: `/dev/null` stands for a GPU's node (a character device every
//! Linux host has) and `/dev/pts` for a path below `/dev` that is not one. (Linux only: only
//! Linux runs containers.)

use crate::support::{
    CountingSleeper, Lowest, Script, ScriptConnector, code, flags, linux_host, run_defaulted,
    welcome,
};

/// The flags of a crun agent that pass every check before the GPU flags'.
const CRUN: [&str; 6] = [
    "--job-user",
    "jsjob",
    "--state-root",
    "/s",
    "--scratch-root",
    "/r",
];

/// Runs the agent to its first wait with `extra` exactly as given (a crun start when it names
/// neither `--no-jobs` nor `--allow-container-engine`) on `host`; returns (status, stderr,
/// whether it sent anything).
fn run_agent(extra: &[&str], host: &crate::support::Host) -> (String, String, bool) {
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run_defaulted(
        &flags(extra),
        host,
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    let sent = !sent.borrow().is_empty();
    (ran.status(), ran.err, sent)
}

/// Proves each GPU flag that cannot work stops the agent with status 2, naming the flag, and
/// sends nothing: a relative device path, one outside `/dev`, one with `..`, a second
/// `{index}`, a path that is not a character device, a missing node, a mount source below
/// `/proc`, a mount source that is a file, two mounts at one destination, a reserved
/// variable, `--gpu-mount` and `--gpu-env` without `--gpu-device`, a device list on a host
/// whose GPUs `--gpus 0` takes out of the offer, and the flags with
/// the container engine (`--allow-container-engine`) and an agent that runs no job
/// (`--no-jobs`). Catches a flag that is accepted and then ignored, a check that runs after the
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
            format!(
                "--gpu-mount: {twice}: the destination /usr/local/driver overlaps \
                 /usr/local/driver, another mount's"
            ),
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
            vec!["--gpu-device", "/dev/null", "--gpus", "0"],
            "--gpu-device: the host offers no GPU to give a job".to_owned(),
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
    let engine = [
        "--allow-container-engine",
        "--container-bin",
        "/usr/bin/docker",
    ];
    let others: Vec<(Vec<&str>, &str)> = vec![
        (
            [&engine[..], &["--gpu-device", "/dev/null"][..]].concat(),
            "--gpu-device: not with --allow-container-engine (a flag of the crun runtime)",
        ),
        (
            [&engine[..], &["--gpu-mount", "/proc:/driver"][..]].concat(),
            "--gpu-mount: not with --allow-container-engine (a flag of the crun runtime)",
        ),
        (
            [&engine[..], &["--gpu-env", "A=1"][..]].concat(),
            "--gpu-env: not with --allow-container-engine (a flag of the crun runtime)",
        ),
        (
            vec!["--no-jobs", "--gpu-device", "/dev/null"],
            "--gpu-device: not with --no-jobs (an agent that runs no job)",
        ),
    ];
    for (args, why) in others {
        let (status, err, sent) = run_agent(&args, &linux_host());
        assert_eq!(status, code(2), "{args:?}: {err}");
        assert!(err.contains(why), "{args:?}: {err}");
        assert!(!sent, "{args:?}: the agent sent a hello");
    }
}

/// `js-agent crun-gpu-dropin` is answered by `main_with` before the signal handlers are
/// installed, over the machine's own commands and nodes: on a `/dev` with no NVIDIA node it
/// writes no drop-in and exits 0, and an `nvidia-smi` that cannot be started is a warning, not
/// a failure. Catches the verb falling through to the agent's own flags (exit 2, `--out` unknown).
#[test]
fn main_with_answers_the_gpu_dropin_verb_before_it_installs_the_signal_handlers() {
    let dir = js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "crun-gpu-dropin");
    std::fs::create_dir(dir.join("dev")).expect("dev");
    let out = dir.join("zz-gpu.conf");
    let smi = dir.join("no-nvidia-smi");
    let installed = std::cell::Cell::new(false);
    let install = || {
        installed.set(true);
        Err(std::io::Error::other("no signal handlers for the drop-in"))
    };
    let args: Vec<std::ffi::OsString> = vec![
        "js-agent".into(),
        "crun-gpu-dropin".into(),
        "--out".into(),
        out.clone().into(),
        "--dev-root".into(),
        dir.join("dev").into(),
        "--nvidia-smi".into(),
        smi.clone().into(),
    ];
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = js_agent::main_with(&install, args, &mut stdout, &mut stderr);
    assert!(!installed.get(), "the verb installed the signal handlers");
    assert_eq!(format!("{status:?}"), code(0));
    assert!(!out.exists(), "a drop-in was written");
    let stdout = String::from_utf8(stdout).expect("utf-8");
    assert_eq!(
        stdout,
        format!(
            "crun-gpu-dropin: no GPU node in {}: no drop-in written\n",
            dir.join("dev").display()
        )
    );
    let stderr = String::from_utf8(stderr).expect("utf-8");
    let warning = format!("crun-gpu-dropin: warning: {} -L: ", smi.display());
    assert!(stderr.starts_with(&warning), "{stderr}");
}
