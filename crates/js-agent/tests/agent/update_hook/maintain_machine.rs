//! `drain` and `resume` on a machine ([`run_maintain`]): the state root they look at before
//! anything else, the flags file the lease is made from, a whole drain and resume of an idle
//! host against a real lease server in the test process, and the host facts as the machine
//! reads them. The steps' own logic is tested over the fakes (steps_drain.rs,
//! steps_resume.rs). Nothing here starts a unit: a test that reaches the restart of the
//! agent would run `systemctl` on the host, so every test keeps the facts of the drain and the
//! resume equal, or leaves the hold with none.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use js_agent::lifecycle::StateRoot;
use js_agent::lifecycle::hold::{HOLD_FILE, HOLD_RESUME_FILE, HOLD_TEXT, write_text};
use js_agent::own_host::{HttpLease, Lease, RequestLimit};
use js_agent::update_hook::{
    Class, Done, DrainParams, Facts, Machine, MaintainInvocation, MaintenanceStep, Outcome, Reason,
    SystemFacts, host_facts, run_maintain,
};
use js_agent::{CREDENTIAL_FILE, Login};

use super::machine::{Tree, reaching};
use super::machine_root::open_state_root;
use super::step_rig::as_root;
use crate::cordon::http::enrol;
use crate::jobs::private;
use crate::server::T0;
use crate::support::{
    AMD_MAC, Host, NVIDIA_CMD, OS_VERSION_CMD, PROFILER_CMD, linux_host, mac_host,
};
use crate::xcodes::with_installed;

/// The agent's unit in the params of these tests. No test starts or stops it.
const UNIT: &str = "js-agent-crun.service";

/// What the agent's own check says of a link or a file where the state root should be.
const NOT_DIR: &str = "not a directory (a link is not accepted)";

/// What the report of a step that says nothing is, as the updater reads it.
const EMPTY_REPORT: &str = r#"{"class":"release","promotion_pending":false,"reversible":true}"#;

/// The params of `drain` and `resume` for `tree`'s flags file and token, over `state_root`.
fn params(tree: &Tree, state_root: &Path) -> DrainParams {
    DrainParams {
        unit: UNIT.to_owned(),
        flags_file: tree.flags(),
        token_file: tree.root.join("token"),
        state_root: state_root.to_owned(),
        cordon_lease_secs: 300,
        defer_retry_secs: 300,
        drain_deadline_unix: None,
    }
}

/// Runs `step` on a machine that has no id of its own.
fn run(step: MaintenanceStep, params: &DrainParams) -> Done {
    let invocation = MaintainInvocation {
        step,
        params: PathBuf::from("/unused"),
    };
    let own_host_id = || -> Result<String, String> { Err("unused".to_owned()) };
    let machine = Machine {
        stat: &as_root,
        own_host_id: &own_host_id,
        ..Machine::system()
    };
    run_maintain(&invocation, params, &machine)
}

/// The names of the files in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The text of a hold file whose note says `cordoned_before`, with no facts.
fn hold_text(cordoned_before: bool) -> String {
    format!("{HOLD_TEXT}cordoned_before={cordoned_before}\n")
}

/// Asserts that `done` is `state_invalid` of the release with the exact text `said`.
#[track_caller]
fn assert_state_invalid(done: &Done, said: &str, what: &str) {
    assert_eq!(done.outcome, Outcome::Failure, "{what}: {done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{what}");
    assert_eq!(done.report.class, Class::Release, "{what}");
    assert!(done.report.reversible, "{what}");
    assert_eq!(done.report.detail.as_deref(), Some(said), "{what}");
}

/// Asserts that a state root that is there and not usable (a file, a link, a directory others
/// can enter) ends `drain` and `resume` alike `state_invalid` with the exact text, naming the
/// params key `state_root` and the path, and that nothing is made: the directory that holds a
/// usable one stays as it was. It catches a state root read without the agent's rules, so
/// that a hold file would be written into, or read from, a directory anyone can write.
#[test]
fn a_state_root_that_is_there_and_not_usable_ends_drain_and_resume_state_invalid() {
    let tree = Tree::new("hook-maintain-bad-root");
    let open = open_state_root(&tree.files);
    let file = tree.files.write("file-state", "x", 0o600);
    let target = private(&tree.files, "private-state");
    let link = tree.files.path("linked-state");
    symlink(&target, &link).unwrap();
    let before = names(&target);
    for (path, why) in [
        (open, "group and others must have no permission"),
        (file, NOT_DIR),
        (link, NOT_DIR),
    ] {
        let said = format!("state_root: {}: {why}", path.display());
        for step in [MaintenanceStep::Drain, MaintenanceStep::Resume] {
            let done = run(step, &params(&tree, &path));
            assert_state_invalid(&done, &said, &format!("{step} on {}", path.display()));
        }
    }
    assert_eq!(names(&target), before);
    assert_eq!(names(&tree.files.path("open-state")), [CREDENTIAL_FILE]);
}

/// Asserts that a state root where nothing is ends `drain` `state_invalid`, with the text that
/// says a drain needs the state root of the agent that runs, and ends `resume` with success and
/// the report of a step that says nothing, and that neither makes the directory. It catches a
/// drain that goes on over a host whose agent never ran, and a resume that fails over a host
/// that was never held.
#[test]
fn an_absent_state_root_ends_drain_state_invalid_and_resume_with_success() {
    let tree = Tree::new("hook-maintain-no-root");
    let absent = tree.root.join("no-such-state");
    let params = params(&tree, &absent);
    let done = run(MaintenanceStep::Drain, &params);
    let said = format!(
        "state_root: {} is not there, and a drain needs the state root of the agent that runs",
        absent.display()
    );
    assert_state_invalid(&done, &said, "drain");
    let done = run(MaintenanceStep::Resume, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.to_line(), EMPTY_REPORT);
    assert!(!absent.exists());
}

/// Asserts that a flags file that cannot be used (not there, or naming no server) ends `drain`
/// `lease_unreachable`, class environment, with the text the flags file's refusal starts with
/// (`flags_file: `), and that it writes no hold file: the drain's first call to the lease
/// server is the read of the host for the note, which comes before the hold is written, so the
/// state root stays empty. It catches a drain that holds the host before it knows it can
/// reach the server, which would leave the host out of service for a run that never began.
#[test]
fn a_flags_file_that_cannot_be_used_ends_drain_before_it_writes_the_hold() {
    let tree = Tree::new("hook-maintain-drain-bad-flags");
    let state = private(&tree.files, "agent-state");
    let params = params(&tree, &state);
    let cases = [
        ("missing", None, "cannot be read: No such file or directory"),
        (
            "no server",
            Some("--pool=farm\n"),
            "required arguments were not provided",
        ),
    ];
    for (what, text, said) in cases {
        if let Some(text) = text {
            fs::write(tree.flags(), text).unwrap();
            fs::set_permissions(tree.flags(), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let done = run(MaintenanceStep::Drain, &params);
        assert_eq!(done.outcome, Outcome::Failure, "{what}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable), "{what}");
        assert_eq!(done.report.class, Class::Environment, "{what}");
        let detail = done.report.detail.unwrap();
        assert!(detail.starts_with("flags_file: "), "{what}: {detail}");
        assert!(detail.contains(said), "{what}: {detail}");
        assert!(names(&state).is_empty(), "{what}");
    }
}

/// Asserts that `resume` of a hold whose note says no cordon was in force, with a flags file
/// that cannot be used, ends `lease_unreachable` (it must lift a cordon and cannot ask), after
/// it renamed the hold file: the text is in `hold.resume`, which a later `resume` finishes. It
/// catches a resume that removes the hold when the cordon was not lifted, which would leave
/// the host cordoned with nothing to renew or lift it.
#[test]
fn resume_that_cannot_lift_the_cordon_keeps_the_renamed_hold() {
    let tree = Tree::new("hook-maintain-resume-bad-flags");
    let state = private(&tree.files, "agent-state");
    let root = StateRoot::open(&state).unwrap();
    let text = hold_text(false);
    write_text(&root, &text).unwrap();
    let done = run(MaintenanceStep::Resume, &params(&tree, &state));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert!(done.report.detail.unwrap().starts_with("flags_file: "));
    assert_eq!(names(&state), [HOLD_RESUME_FILE]);
    assert_eq!(
        fs::read_to_string(state.join(HOLD_RESUME_FILE)).unwrap(),
        text
    );
}

/// Asserts that `resume` of a hold whose note says a cordon was in force asks the lease server
/// nothing: with a flags file that cannot be used it still succeeds, and both the hold file
/// and the renamed file are gone. It catches a resume that lifts an operator's cordon, or one
/// that reaches for the server when it has nothing to lift.
#[test]
fn resume_of_a_host_cordoned_before_the_drain_asks_the_lease_server_nothing() {
    let tree = Tree::new("hook-maintain-resume-kept");
    let state = private(&tree.files, "agent-state");
    let root = StateRoot::open(&state).unwrap();
    write_text(&root, &hold_text(true)).unwrap();
    let done = run(MaintenanceStep::Resume, &params(&tree, &state));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert!(names(&state).is_empty());
}

/// Asserts a whole drain and resume of an idle host against a real lease server: the drain
/// succeeds with the report of a step that says nothing, writes the hold file (mode 0600,
/// the hold text, then the note `cordoned_before=false`) in the state root and places the
/// cordon of `cordon_lease_secs` on the server; a second drain finds the hold and the cordon
/// and changes neither (the hold file's text, the cordon's expiry); the resume succeeds,
/// removes the hold file and the renamed file, restarts nothing and lifts the cordon; and a
/// second resume changes nothing. It catches a drain that cordons without holding (or holds
/// without cordoning), a resume that leaves the cordon or the hold, and a rerun that rewrites
/// the note.
#[test]
fn a_drain_and_a_resume_of_an_idle_host_hold_and_cordon_it_and_put_it_back() {
    let enrolled = enrol("hook_maintain_drain_resume");
    let tree = Tree::new("hook-maintain-drain-resume");
    let _ = tree.params(true, &reaching(&enrolled));
    let params = params(&tree, &enrolled.state);
    let hold = enrolled.state.join(HOLD_FILE);
    let server = &enrolled.server;
    assert!(server.host("mac-1").get("cordon").is_none());

    let done = run(MaintenanceStep::Drain, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.to_line(), EMPTY_REPORT);
    let held = fs::read_to_string(&hold).unwrap();
    assert!(held.starts_with(HOLD_TEXT), "{held}");
    assert_eq!(held.lines().nth(1), Some("cordoned_before=false"));
    assert_eq!(
        fs::metadata(&hold).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 300_000
    );

    let done = run(MaintenanceStep::Drain, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(fs::read_to_string(&hold).unwrap(), held);
    assert_eq!(
        server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 300_000
    );

    let done = run(MaintenanceStep::Resume, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert!(
        done.report
            .detail
            .as_deref()
            .is_none_or(|detail| detail.contains("not restarted")),
        "{done:?}"
    );
    assert!(!hold.exists());
    assert!(!enrolled.state.join(HOLD_RESUME_FILE).exists());
    assert!(server.host("mac-1").get("cordon").is_none());

    let done = run(MaintenanceStep::Resume, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.to_line(), EMPTY_REPORT);
    assert!(server.host("mac-1").get("cordon").is_none());
}

/// Asserts that a drain of a host an operator had cordoned notes `cordoned_before=true`, takes
/// the cordon over (its expiry becomes the drain's, which is longer) and that the resume
/// removes the hold but leaves the cordon in force. It catches a resume that lifts a cordon
/// the drain did not place.
#[test]
fn resume_leaves_a_cordon_that_was_in_force_before_the_drain() {
    let enrolled = enrol("hook_maintain_cordoned_before");
    let tree = Tree::new("hook-maintain-cordoned-before");
    let _ = tree.params(true, &reaching(&enrolled));
    let params = params(&tree, &enrolled.state);
    let operator = HttpLease::connect(
        &enrolled.server.url(),
        &Login::Token(enrolled.state.join(CREDENTIAL_FILE)),
        None,
        "mac-1",
        RequestLimit::DEFAULT,
    )
    .unwrap();
    operator.cordon(100).unwrap();
    assert_eq!(
        enrolled.server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 100_000
    );

    let done = run(MaintenanceStep::Drain, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let held = fs::read_to_string(enrolled.state.join(HOLD_FILE)).unwrap();
    assert_eq!(held.lines().nth(1), Some("cordoned_before=true"));

    let done = run(MaintenanceStep::Resume, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!enrolled.state.join(HOLD_FILE).exists());
    assert!(!enrolled.state.join(HOLD_RESUME_FILE).exists());
    assert_eq!(
        enrolled.server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 300_000
    );
}

/// A Mac that has no Apple GPU (its only GPU is AMD's), as the agent's tests have it.
fn mac_without_gpu() -> Host {
    let mut host = mac_host();
    host.commands = host.commands.with(PROFILER_CMD, Ok(AMD_MAC));
    host
}

/// Asserts the exact line of the host facts of a Linux host with two A100s (its CPUs, memory,
/// the count and class of its GPUs and each device with its memory in bytes; no os version),
/// and that it differs from the line of the same host without a GPU, whose GPU fields are
/// empty. It catches a line that drops a device or the class, which would let a changed GPU
/// pass as unchanged and the agent keep its old hello.
#[test]
fn the_facts_of_a_linux_host_name_its_gpus_and_devices() {
    let host = linux_host();
    let line = host_facts(&host.platform, &host.commands, &host.files).unwrap();
    assert_eq!(
        line,
        "os=linux arch=x86_64 os_version= cpu_millis=8000 memory_bytes=34359738368 gpus=2 \
         gpu_class=nvidia-a100-sxm4-80gb \
         gpu_devices=0:GPU-3f1c9a52-0b7e-4d0a-9a55-6f1d2c8b7e01:NVIDIA A100-SXM4-80GB:85899345920,\
         1:GPU-77a0c2d4-5e19-4b3f-8c66-0d9e1f2a3b02:NVIDIA A100-SXM4-80GB:85899345920"
    );
    let mut bare = linux_host();
    bare.commands = bare.commands.with(NVIDIA_CMD, Err("no driver"));
    let bare_line = host_facts(&bare.platform, &bare.commands, &bare.files).unwrap();
    assert_eq!(
        bare_line,
        "os=linux arch=x86_64 os_version= cpu_millis=8000 memory_bytes=34359738368 gpus=0 \
         gpu_class= gpu_devices="
    );
    assert_ne!(line, bare_line);
}

/// Asserts the exact line of a Mac: the major version of macOS (`26.1` is `26`), the Apple GPU
/// with its class and no device list; of a Mac whose version probe fails (an empty version);
/// and of a Mac with no Apple GPU (none). It catches a Mac whose new major version of macOS
/// reads as no change, and a probe failure that fails the whole read.
#[test]
fn the_facts_of_a_mac_carry_its_os_version() {
    let mac = mac_host();
    let line = host_facts(&mac.platform, &mac.commands, &mac.files).unwrap();
    assert_eq!(
        line,
        format!(
            "os=macos arch=aarch64 os_version=26 cpu_millis=10000 memory_bytes=17179869184 gpus=1 \
             gpu_class={} gpu_devices= xcode=",
            js_agent::APPLE_GPU_CLASS
        )
    );
    let mut old = mac_host();
    old.commands = old.commands.with(OS_VERSION_CMD, Err("sysctl: not found"));
    let line = host_facts(&old.platform, &old.commands, &old.files).unwrap();
    assert!(line.starts_with("os=macos arch=aarch64 os_version= cpu_millis=10000 "));
    let none = mac_without_gpu();
    let line = host_facts(&none.platform, &none.commands, &none.files).unwrap();
    assert_eq!(
        line,
        "os=macos arch=aarch64 os_version=26 cpu_millis=10000 memory_bytes=17179869184 gpus=0 \
         gpu_class= gpu_devices= xcode="
    );
}

/// Asserts that the line of a Mac ends with the builds of its installed Xcodes, ascending and
/// joined with a comma (`xcode=16C5032a,16F6`), that it differs once an Xcode is added to the
/// files, and that a Linux host's line has no `xcode` part even with Xcode apps in
/// `/Applications`. It catches an Xcode installed during a maintenance run that reads as no
/// change, so the agent would keep a hello without it.
#[test]
fn the_facts_of_a_mac_name_its_xcodes_and_change_when_one_is_installed() {
    let mut mac = mac_host();
    mac.files = with_installed(
        mac.files,
        &[("Xcode-16.4.app", "16F6"), ("Xcode-16.2.app", "16C5032a")],
    );
    let line = host_facts(&mac.platform, &mac.commands, &mac.files).unwrap();
    assert!(
        line.ends_with(" gpu_devices= xcode=16C5032a,16F6"),
        "{line}"
    );
    mac.files = with_installed(
        mac.files,
        &[
            ("Xcode-16.4.app", "16F6"),
            ("Xcode-16.2.app", "16C5032a"),
            ("Xcode-26.app", "17A100"),
        ],
    );
    let later = host_facts(&mac.platform, &mac.commands, &mac.files).unwrap();
    assert!(later.ends_with(" xcode=16C5032a,16F6,17A100"), "{later}");
    assert_ne!(line, later);

    let mut linux = linux_host();
    linux.files = with_installed(linux.files, &[("Xcode-16.2.app", "16C5032a")]);
    let line = host_facts(&linux.platform, &linux.commands, &linux.files).unwrap();
    assert!(!line.contains("xcode"), "{line}");
}

/// Asserts that a host whose facts cannot be detected (no `/proc/meminfo`) is an error with
/// detect's own text, from `host_facts` and from `SystemFacts::read`, and that `SystemFacts`
/// of a readable host reads the same line as `host_facts`. It catches facts read as an empty
/// line when they failed, which `resume` would take for facts that changed.
#[test]
fn facts_that_cannot_be_detected_are_an_error_with_the_reason() {
    let mut host = linux_host();
    host.files = host.files.without("/proc/meminfo");
    let said = "cannot size memory: /proc/meminfo: No such file or directory";
    assert_eq!(
        host_facts(&host.platform, &host.commands, &host.files),
        Err(said.to_owned())
    );
    let facts = SystemFacts {
        platform: host.platform.clone(),
        runner: &host.commands,
        files: &host.files,
    };
    assert_eq!(facts.read(), Err(said.to_owned()));

    let good = linux_host();
    let facts = SystemFacts {
        platform: good.platform.clone(),
        runner: &good.commands,
        files: &good.files,
    };
    assert_eq!(
        facts.read(),
        host_facts(&good.platform, &good.commands, &good.files)
    );
}
