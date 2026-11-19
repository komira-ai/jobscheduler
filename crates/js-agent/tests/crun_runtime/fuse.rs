//! A FUSE job over a real `crun`, as the job user: a job given the FUSE device (`fuse` in its
//! run) mounts a FUSE file system in its own mount namespace and reads the kernel's first
//! request from its descriptor, with the capabilities, network and privileges the design
//! allows it and no more; a job of the same image without it has no device and cannot mount.
//!
//! The first test needs what every test of this target needs (root in a `Delegate=yes` unit,
//! the job user `JS_JOB_USER`, the image `JS_TEST_IMAGE`) and `/dev/fuse` on the host; hosted
//! runners have it. The second asks the agent's own probe ([`js_agent::fuse_env`]) whether
//! this host can offer FUSE jobs, and needs a kernel of 6.14 or later: the ubuntu-24.04 runners
//! have 6.8, so the `js-agent-crun-runtime` workflow skips it by name, and it is run on a farm
//! node: the target's binary, as that workflow runs it, with `--ignored
//! this_host_offers_fuse_jobs`.

use js_agent::crun::read_node;
use js_agent::fuse_env::probe;
use js_agent::lifecycle::{Body, Exit, Runtime, Status};

use crate::runtime_suite::{Program, Subject, spec_of};
use crate::subject;

/// What the job prints, one line for each check: the node `/dev/fuse` (or `nodev`), its
/// effective capabilities, `NoNewPrivs`, the interfaces of its network namespace, whether a
/// FUSE mount at `/tmp/m` over the descriptor it opened on `/dev/fuse` succeeded, and, when it
/// did, the length and opcode of the kernel's first request on that descriptor (opcode 26 is
/// `FUSE_INIT`), after which the mount is detached.
const SCRIPT: &str = r#"
if [ -e /dev/fuse ]; then echo "node $(stat -c '%F %t %T' /dev/fuse)"; else echo "node nodev"; fi
echo "capeff $(awk '/^CapEff:/ {print $2}' /proc/self/status)"
echo "nnp $(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)"
echo "net $(awk 'NR>2 {sub(/:.*/, "", $1); n = n (n == "" ? "" : " ") $1} END {print n}' /proc/net/dev)"
mkdir /tmp/m
if [ -e /dev/fuse ]; then exec 3<>/dev/fuse; fi
mount -t fuse -o fd=3,rootmode=40000,user_id=$(id -u),group_id=$(id -g) tenv /tmp/m 2>/dev/null
if grep -q ' /tmp/m fuse' /proc/self/mounts; then
  echo "mounted yes"
  set -- $(dd bs=1048576 count=1 <&3 2>/dev/null | od -An -tu4 -N8)
  echo "init $1 $2"
  umount -l /tmp/m
else
  echo "mounted no"
fi
echo ran
"#;

/// Runs [`SCRIPT`] in a job of the subject's image, given the FUSE device when `fuse`, and
/// returns the lines it printed.
fn run_script(
    s: &mut subject::CrunSubject,
    runtime: &mut js_agent::crun::CrunRuntime,
    run_id: &str,
    fuse: bool,
) -> Vec<String> {
    let mut spec = spec_of(s, run_id, Program::Exit(0));
    spec.fuse = fuse;
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), SCRIPT.to_owned()],
    };
    let id = runtime.create_and_start(&spec).expect("create and start");
    runtime.wait_ended(&id);
    let ended = runtime.poll(&id).expect("poll");
    let tail = runtime.log_tail(&id).expect("a tail").expect("the tail");
    assert_eq!(
        ended,
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        }),
        "{tail:?}"
    );
    runtime.remove(&id).expect("remove");
    tail.stdout.lines().map(str::to_owned).collect()
}

/// Proves, over a real `crun` and as the job user, that a job run with `fuse` sees
/// `/dev/fuse` as a character device 10:229 (`a e5`), holds `CAP_SYS_ADMIN` alone (effective
/// set `0000000000200000`) with `NoNewPrivs` on, has a network namespace with loopback only,
/// can mount a FUSE file system at a path of its own with `mount(2)` over the descriptor it
/// opened on the device, and reads `FUSE_INIT` (opcode 26, a header of more than 40 bytes)
/// as the kernel's first request on it; and that a job of the same image without `fuse` has
/// no `/dev/fuse`, an empty effective set, and a refused mount. Catches a device or a
/// capability that never reaches the container, a seccomp filter or device rule that keeps
/// the mount from working, a network namespace given more than loopback, and a plain job
/// given the device.
#[test]
#[ignore = "real crun as root for a job user: run by the js-agent-crun-runtime workflow"]
fn a_fuse_job_mounts_fuse_in_its_own_namespace_and_reads_init() {
    let mut s = subject::crun_subject("a_fuse_job_mounts_fuse");
    s.prepared();
    let mut runtime = s.open_as("crun-suite-fuse");
    let listed = run_script(&mut s, &mut runtime, "f1", true);
    for want in [
        "node character special file a e5",
        "capeff 0000000000200000",
        "nnp 1",
        "net lo",
        "mounted yes",
        "ran",
    ] {
        assert!(
            listed.iter().any(|line| line == want),
            "{want} not in {listed:?}"
        );
    }
    let init = listed
        .iter()
        .find_map(|line| line.strip_prefix("init "))
        .unwrap_or_else(|| panic!("no init line in {listed:?}"));
    let numbers: Vec<u32> = init
        .split_whitespace()
        .map(|number| number.parse().expect("a number"))
        .collect();
    assert_eq!(numbers.len(), 2, "{init:?}");
    assert_eq!(numbers[1], 26, "the opcode of the first request: {init:?}");
    assert!(numbers[0] > 40, "the length of FUSE_INIT: {init:?}");

    let listed = run_script(&mut s, &mut runtime, "f2", false);
    for want in ["node nodev", "capeff 0000000000000000", "mounted no", "ran"] {
        assert!(
            listed.iter().any(|line| line == want),
            "{want} not in {listed:?}"
        );
    }
    assert!(
        !listed.iter().any(|line| line.starts_with("init ")),
        "{listed:?}"
    );
}

/// Proves this host can give a job the FUSE device by the agent's own probe: its kernel is
/// at least 6.14 and `/dev/fuse` is the FUSE device. The answer for FUSE over io_uring is
/// printed and not asserted: the farm boots with `fuse.enable_uring` off until the node
/// image sets it. Catches a node the farm believes fit for FUSE kernel tests that the agent
/// would not report `fuse-jobs` on.
#[test]
#[ignore = "a node with kernel >= 6.14 and /dev/fuse: run on a farm node (see the module)"]
fn this_host_offers_fuse_jobs() {
    let env = probe(
        &|path| std::fs::read_to_string(path).map_err(|why| why.to_string()),
        read_node,
    );
    println!("fuse-uring: {:?}", env.uring);
    assert_eq!(
        env.jobs,
        Ok(()),
        "this host cannot give a job the FUSE device"
    );
}
