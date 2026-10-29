//! The runtime against a stub program the test writes, run as a real child process by the
//! bounded system runner. The stub is a shell script in the test's own directory that
//! appends its arguments to a log and answers like the command line; docker and podman
//! are never run.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use js_agent::cli_runtime::{CliError, CliRuntime, Settings, system_runner};
use js_agent::lifecycle::{ContainerRuntime, Exit, Status};

use super::{
    DOCKER_SCRATCH, HOST, NAME_R1, in_another_pid_namespace, made_at, mount, no_file, platform,
    profile_dir, profile_option, sees_the_engines_pid, spec,
};
use crate::support::Scratch;

pub(super) const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Logs each call (one argument per line, then a marker) in `$0.log` and prints what the
/// command line would print.
const STUB: &str = r#"#!/bin/sh
{ for a in "$@"; do printf '%s\n' "$a"; done; printf -- '--end--\n'; } >> "$0.log"
case "$1" in
--version) echo "Docker version 27.3.1, build ce12230" ;;
create|ps) echo 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef ;;
info) echo true ;;
volume) echo '{"device":"/work/scratch/r1","o":"bind,nosuid,nodev","type":"none"}' ;;
inspect)
  case "$*" in
  *Config.Labels*) echo 'running {"jobscheduler.dev/run-id":"r1","jobscheduler.dev/created-ms":"1000000"}' ;;
  *Mounts*) echo '[{"Type":"volume","Name":"vol1","Driver":"local","Source":"/var/lib/engine/volumes/vol1/_data","Destination":"/scratch","Mode":"z","RW":true},{"Type":"bind","Source":"/srv/in","Destination":"/mnt/data/in","Mode":"","RW":false}]' ;;
  *State.Pid*) echo "4000 running" ;;
  *) echo "exited 3 false" ;;
  esac ;;
esac
"#;

/// Runs the stub once so the system has finished with the file: a program that was
/// written a moment ago can be refused as busy while another thread of the test process
/// still holds a copy of the descriptor it was written through.
pub(super) fn settle(bin: &Path) {
    for _ in 0..10_000 {
        match Command::new(bin).arg("--version").output() {
            Err(err) if err.kind() == ErrorKind::ExecutableFileBusy => std::thread::yield_now(),
            _ => break,
        }
    }
}

/// The calls the stub logged, one argv each.
fn logged(bin: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(format!("{}.log", bin.display())).unwrap();
    let mut calls = vec![Vec::new()];
    for line in text.lines() {
        match line {
            "--end--" => calls.push(Vec::new()),
            arg => calls.last_mut().unwrap().push(arg.to_owned()),
        }
    }
    calls.pop();
    calls
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Drives every call of the runtime through a real child process and compares what the
/// stub received, argument by argument, with the documented shape: the version, the
/// `create` with its labels, limits and hardening, `start`, `inspect`, `stop`, `rm` and
/// the two calls of a listing. It catches a change to any argument of any call, and it
/// proves that the program is started by the path it was given with every value as one
/// argument (the job's `x y` arrives as one).
#[test]
fn cli_runtime_argv_snapshot() {
    let dir = Scratch::new("cli-stub-snapshot");
    let bin = dir.write("docker", STUB, 0o755);
    settle(&bin);
    fs::remove_file(format!("{}.log", bin.display())).unwrap();
    let settings = Settings::new(&bin, Some("jobs"), Some("1000:1000")).unwrap();
    let runner = Box::new(system_runner(Duration::from_secs(10)));
    let mut rt = CliRuntime::open(settings, HOST, &platform("linux"), made_at, runner)
        .ok()
        .unwrap()
        .with_profile_dir(profile_dir())
        .with_pid_seen(sees_the_engines_pid)
        .with_proc_files(no_file, in_another_pid_namespace);

    let mut job = spec("r1");
    job.gpu_indices = vec![1];
    job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
    let id = rt.create_and_start(&job).unwrap();
    assert_eq!(id.0, ID);
    let exit = Status::Exited(Exit {
        code: 3,
        oom_killed: false,
    });
    assert_eq!(rt.poll(&id), Ok(exit));
    rt.stop(&id, Duration::from_millis(2500)).unwrap();
    rt.remove(&id).unwrap();
    let found = rt.list_labelled().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id.0, ID);
    assert_eq!(found[0].running_ms, 0);

    let state = "{{.State.Status}} {{.State.ExitCode}} {{.State.OOMKilled}}";
    let expected: Vec<Vec<String>> = vec![
        strings(&["--version"]),
        strings(&["info", "--format", "{{.SwapLimit}}"]),
        strings(&[
            "create",
            "--name",
            NAME_R1,
            "--label",
            "jobscheduler.dev/agent=h-0123456789abcdef",
            "--label",
            "jobscheduler.dev/created-ms=1000000",
            "--label",
            "jobscheduler.dev/lease-id=lease-1",
            "--label",
            "jobscheduler.dev/run-id=r1",
            "--label",
            "team=ml",
            "--cpus",
            "1.500",
            "--memory",
            "1073741824",
            "--memory-swap",
            "1073741824",
            "--pids-limit",
            "100",
            "--gpus",
            "\"device=1\"",
            "--mount",
            DOCKER_SCRATCH,
            "--mount",
            "type=bind,src=/srv/in,dst=/mnt/data/in,readonly,bind-recursive=readonly,bind-propagation=rprivate",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--security-opt",
            &profile_option(),
            "--network",
            "jobs",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,noexec,nosuid,nodev,size=64m",
            "--ulimit",
            "core=0",
            "--ipc",
            "private",
            "--log-driver",
            "json-file",
            "--log-opt",
            "max-size=1048576",
            "--log-opt",
            "max-file=2",
            "--pull",
            "never",
            "--user",
            "1000:1000",
            "--",
            "registry.example/job:1",
            "--go",
            "x y",
        ]),
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{json .Mounts}}",
            "--",
            ID,
        ]),
        strings(&[
            "volume",
            "inspect",
            "--format",
            "{{json .Options}}",
            "--",
            "vol1",
        ]),
        strings(&["start", "--", ID]),
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{.State.Pid}} {{.State.Status}}",
            "--",
            ID,
        ]),
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            state,
            "--",
            ID,
        ]),
        strings(&["stop", "--time", "3", "--", ID]),
        strings(&["rm", "--force", "--volumes", "--", ID]),
        strings(&[
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            "label=jobscheduler.dev/run-id",
            "--filter",
            "label=jobscheduler.dev/agent=h-0123456789abcdef",
        ]),
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{.State.Status}} {{json .Config.Labels}}",
            "--",
            ID,
        ]),
    ];
    assert_eq!(logged(&bin), expected);
}

/// Asserts that a missing binary and a nonzero status are typed errors that name the
/// call and the program and print nothing of the program's output: a program that was
/// there when the flags were checked and is gone when the runtime starts, and one that
/// exits with status 1. It catches a failure reported as an empty success.
#[test]
fn missing_binary_and_nonzero_status_are_typed_errors() {
    let dir = Scratch::new("cli-stub-errors");
    let bin = dir.write("docker", STUB, 0o755);
    let settings = Settings::new(&bin, None, None).unwrap();
    fs::remove_file(&bin).unwrap();
    let runner = Box::new(system_runner(Duration::ZERO));
    let opened = CliRuntime::open(settings, HOST, &platform("linux"), made_at, runner);
    let Some(CliError::Call { call, why }) = opened.err() else {
        panic!("not a call failure");
    };
    assert_eq!(call, "--version");
    assert!(why.starts_with(bin.to_str().unwrap()), "{why}");
    assert!(why.contains("No such file or directory"), "{why}");

    let falsy = Settings::new(Path::new("/usr/bin/false"), None, None).unwrap();
    let runner = Box::new(system_runner(Duration::ZERO));
    let opened = CliRuntime::open(falsy, HOST, &platform("linux"), made_at, runner);
    let Some(CliError::Call { call, why }) = opened.err() else {
        panic!("not a call failure");
    };
    assert_eq!(call, "--version");
    assert!(why.starts_with("/usr/bin/false: "), "{why}");
    assert!(why.contains("exit status: 1"), "{why}");
}

/// Asserts that the error of a call that fails carries the call and the status and
/// never what the program printed: the lifecycle records the text. It catches a program's
/// output (which can hold an image name, a path or a credential) reaching a log.
#[test]
fn a_runtime_error_carries_the_call_and_the_status_only() {
    let dir = Scratch::new("cli-stub-text");
    let bin = dir.write("docker", "#!/bin/sh\necho secret-output\nexit 3\n", 0o755);
    settle(&bin);
    let settings = Settings::new(&bin, None, None).unwrap();
    let runner = Box::new(system_runner(Duration::from_secs(10)));
    let opened = CliRuntime::open(settings, HOST, &platform("linux"), made_at, runner);
    let Some(CliError::Call { why, .. }) = opened.err() else {
        panic!("not a call failure");
    };
    assert!(why.contains("exit status: 3"), "{why}");
    assert!(!why.contains("secret-output"), "{why}");
}
