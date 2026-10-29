//! The host contract in the hello of the whole agent: `runtime`, `exec` and `gpu-toolkit`
//! come from what the agent found, an operator cannot set them, and an agent that runs
//! no containers (or an older one) reports none.

use std::sync::Arc;

use serde_json::{Value, json};

use super::drain::container_flags;
use super::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Host, Jobs, Lowest, Ran, Reply, Scratch, Script, ScriptConnector, code, flags,
    linux_host, refused, reply, run_on, run_scripted, welcome,
};

/// The toolkit check the tests give the agent.
const HOOK: &str = "/opt/js/gpu-toolkit-check";

/// The refusal a pool with an execution mode sends a host without a runtime.
fn no_runtime() -> Reply {
    reply(403, json!({ "error": "host-contract", "field": "runtime" }))
}

/// Runs the whole agent over containers (and `extra` flags) until the server refuses its
/// hello; returns what it printed and the hello it sent (`null` when it sent none).
fn container_hello(name: &str, extra: &[&str], host: &Host) -> (Ran, Value) {
    let dir = Scratch::new(name);
    let mut owned = container_flags(&dir);
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&all);
    let (script, sent) = Script::new(vec![no_runtime()]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let machine = Arc::new(FakeMachine::new());
    let jobs: Jobs<'_> = (machine, None);
    let ran = run_on(&args, host, &connector, &mut sleeper, &mut Lowest, jobs);
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// A Linux host whose toolkit check is `answer`.
fn host_checked_by(answer: Result<&str, &str>) -> Host {
    let mut host = linux_host();
    host.commands = host.commands.with(&format!("{HOOK} "), answer);
    host
}

/// Asserts that an agent given `--container-bin` reports `runtime=container` and
/// `exec=container` beside `os` and `arch`, that a host with two GPUs and no toolkit hook
/// reports no `gpu-toolkit`, and that the capability the server names in its refusal is
/// printed. It catches an agent that does not say how it runs jobs (every pool with a mode
/// would refuse it), one that claims a toolkit it never checked, and one that does not
/// advertise `run-files` (the server would never send it a start with files, or, worse
/// for a server that guessed, would send one to an agent that ignores them).
#[test]
fn a_container_agent_reports_its_runtime_and_mode() {
    let (ran, hello) = container_hello("contract-container", &[], &linux_host());
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(
        hello["capabilities"],
        json!({
            "os": "linux",
            "arch": "x86_64",
            "runtime": "container",
            "exec": "container",
            "run-files": "true",
        })
    );
    let said = "(host-contract; the capability at fault is runtime)";
    assert!(ran.err.contains(said), "{}", ran.err);
}

/// Asserts that an agent without `--container-bin` runs no job and reports none of the
/// contract capabilities, as every agent did before them. It catches an agent that claims
/// a runtime it does not have, which a pool would then trust.
#[test]
fn an_agent_without_containers_reports_no_runtime() {
    let (ran, sent, _) = run_scripted(&[], &linux_host(), vec![refused(403, "host-bound")], 1);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let hello = sent.borrow()[0].1.clone();
    assert_eq!(
        hello["capabilities"],
        json!({ "os": "linux", "arch": "x86_64" })
    );
}

/// Asserts that `gpu-toolkit` is reported when the host offers GPUs and the hook exits 0
/// (and the hook ran), is left out with a warning that carries the runner's reason when
/// the hook fails, and that the hook is not run at all when the agent offers no GPU
/// (`--gpus 0`). It catches a toolkit reported without a check, one reported after a failed
/// check, and a check run for a host that has nothing to use it on.
#[test]
fn the_toolkit_is_reported_from_its_hook() {
    let flag = ["--gpu-toolkit-hook", HOOK];
    let host = host_checked_by(Ok(""));
    let (_, hello) = container_hello("contract-toolkit-ok", &flag, &host);
    assert_eq!(hello["capabilities"]["gpu-toolkit"], "true");
    assert_eq!(hello["capabilities"]["runtime"], "container");
    assert!(host.commands.ran.borrow().contains(&format!("{HOOK} ")));

    let host = host_checked_by(Err("exit status 1"));
    let (ran, hello) = container_hello("contract-toolkit-bad", &flag, &host);
    assert!(
        hello["capabilities"].get("gpu-toolkit").is_none(),
        "{hello}"
    );
    assert_eq!(hello["capabilities"]["runtime"], "container");
    let said =
        "--gpu-toolkit-hook: exit status 1; the host is not reported as having the GPU toolkit";
    assert!(ran.err.contains(said), "{}", ran.err);

    let host = host_checked_by(Ok(""));
    let none = ["--gpu-toolkit-hook", HOOK, "--gpus", "0"];
    let (_, hello) = container_hello("contract-toolkit-nogpu", &none, &host);
    assert!(
        hello["capabilities"].get("gpu-toolkit").is_none(),
        "{hello}"
    );
    assert!(!host.commands.ran.borrow().contains(&format!("{HOOK} ")));
}

/// Asserts that the flag is refused before anything is sent, naming the flag: without
/// `--container-bin` (there is no engine whose toolkit it checks) and with a path that is
/// not absolute (it would be looked up in whatever directory the agent is in).
#[test]
fn the_toolkit_hook_flag_is_checked_at_start() {
    let host = linux_host();
    let alone = ["--gpu-toolkit-hook", HOOK];
    let (ran, sent, _) = run_scripted(&alone, &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("--gpu-toolkit-hook: needs --container-bin"),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty());

    let (ran, hello) =
        container_hello("contract-relative", &["--gpu-toolkit-hook", "check"], &host);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("--gpu-toolkit-hook: check is not an absolute path"),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);
}

/// Asserts that an operator cannot give any of the five capabilities with `--capability`:
/// the agent ends with status 2 naming the key, and sends nothing. It catches a pool's
/// contract satisfied by a flag instead of by what the host has.
#[test]
fn the_contract_capabilities_cannot_be_set_by_flag() {
    for key in ["runtime", "exec", "gpu-toolkit", "disk-gib", "run-files"] {
        let flag = format!("{key}=container");
        let (ran, sent, _) = run_scripted(
            &["--capability", flag.as_str()],
            &linux_host(),
            vec![welcome(1)],
            1,
        );
        assert_eq!(ran.status(), code(2), "{key}: {}", ran.err);
        let said = format!("--capability: {key} is detected and cannot be set");
        assert!(ran.err.contains(&said), "{}", ran.err);
        assert!(sent.borrow().is_empty(), "{key}");
    }
}

/// Asserts that an operator who gave as many capabilities as the wire allows, with
/// `os` and `arch` among them, gets an agent that ends with status 2 saying the hello is
/// not acceptable (the contract capabilities no longer fit), before the job half starts
/// and before anything is sent. It catches a hello that is sent over the wire's limit and
/// refused by the server as `invalid-hello`, which an operator could not tell from a bug.
#[test]
fn capabilities_that_leave_no_room_for_the_contract_are_refused_at_start() {
    let given: Vec<String> = (0..62).map(|n| format!("c{n:02}=v")).collect();
    let mut extra: Vec<&str> = Vec::new();
    for pair in &given {
        extra.extend(["--capability", pair.as_str()]);
    }
    let (ran, hello) = container_hello("contract-room", &extra, &linux_host());
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("the hello is not acceptable"),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);
}

/// The host a container agent runs on when `df` answers `answer` for its scratch root.
fn host_with_disk(dir: &Scratch, answer: Result<&str, &str>) -> Host {
    let mut host = linux_host();
    let line = format!("/bin/df -P -k {}", dir.path("run-scratch").display());
    host.commands = host.commands.with(&line, answer);
    host
}

/// Runs the whole agent over containers on a host whose `df` answers `answer`; returns
/// what it printed and the hello it sent.
fn disk_hello(name: &str, answer: Result<&str, &str>) -> (Ran, Value) {
    let dir = Scratch::new(name);
    let host = host_with_disk(&dir, answer);
    let owned = container_flags(&dir);
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![no_runtime()]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
    let ran = run_on(
        &flags(&all),
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

const DF_HEAD: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n";

/// Asserts that the agent reports `disk-gib` as the free space of the scratch root's
/// volume in whole GiB rounded down (an exact GiB is that many, one KiB less is one fewer,
/// and under a GiB is 0), and that a file system or mount point with spaces in its name
/// does not shift the column read. It catches a disk read in the wrong unit, rounded up,
/// or taken from the wrong column.
#[test]
fn the_free_disk_of_the_work_volume_is_reported_in_whole_gib() {
    let cases = [
        ("/dev/sda1 900 100 10485760 11% /", "10"),
        ("/dev/sda1 900 100 10485759 11% /", "9"),
        ("/dev/sda1 900 100 1048575 11% /", "0"),
        (
            "/dev/disk3 s1 900 100 3145728 11% /Volumes/My Disk 100%",
            "3",
        ),
    ];
    for (n, (row, gib)) in cases.into_iter().enumerate() {
        let (_, hello) = disk_hello(&format!("disk-ok-{n}"), Ok(&format!("{DF_HEAD}{row}\n")));
        assert_eq!(hello["capabilities"]["disk-gib"], gib, "{row}");
        assert_eq!(hello["capabilities"]["runtime"], "container");
    }
}

/// Asserts that a volume `df` cannot report (the program fails, prints nothing, prints no
/// second row, or no number before the capacity) leaves `disk-gib` out of the hello with a
/// warning naming `--scratch-root` and the reason, and does not stop the agent. It catches
/// a guess reported as free space.
#[test]
fn an_unreadable_volume_is_a_warning_and_no_disk() {
    let cases = [
        (Err("df: not found"), "df: not found"),
        (Ok(""), "df printed no free space"),
        (Ok(DF_HEAD), "df printed no free space"),
        (Ok("h\n/dev/sda1 1 2 x 11% /\n"), "df printed no free space"),
        (Ok("h\n/dev/sda1 1 2 3 4 /\n"), "df printed no free space"),
    ];
    for (n, (answer, reason)) in cases.into_iter().enumerate() {
        let (ran, hello) = disk_hello(&format!("disk-bad-{n}"), answer);
        assert!(hello["capabilities"].get("disk-gib").is_none(), "{hello}");
        assert_eq!(hello["capabilities"]["runtime"], "container");
        let said = format!("--scratch-root: cannot read the free disk ({reason}");
        assert!(ran.err.contains(&said), "{}", ran.err);
    }
}
