//! The config hook on a machine: the flags file read as the agent reads it, the lease client
//! over a real lease server, the unit manager of a Mac over a `launchctl` that keeps services,
//! and the steps that touch none of them on the machine's own ports.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::own_host::Lease as _;
use js_agent::update_hook::config::{Done, Params, Reason, Record, run_on, run_step};
use js_agent::update_hook::{Class, LaunchUnits, Machine, Outcome, Step, Units};

use super::super::lease_stub::{Reply, Stub};
use super::super::step_rig::as_root;
use super::super::unit_conformance::{Exit, Launchctl, Service, plist_of};
use super::rig::{Ticks, UNIT, UnitScript, text};
use crate::cordon::http::{enrol, lease_of};
use crate::host_certs::{Identities, server_pair};
use crate::server::Server;
use crate::support::Scratch;

/// A tree of one test: the params of a package over a scratch directory, and a flags file the
/// test writes.
struct Tree {
    files: Scratch,
    params: Params,
    /// A staged release directory, for `prepare`.
    staged: std::path::PathBuf,
}

impl Tree {
    fn new(name: &str, unit: &str) -> Tree {
        let files = Scratch::new(name);
        let root = files.path("tree");
        fs::create_dir_all(&root).unwrap();
        let root = root.display().to_string();
        let params = text(&root, 600, 300, 600).replace(UNIT, unit);
        Tree {
            staged: files.path("staged"),
            files,
            params: Params::parse(&params).unwrap(),
        }
    }

    fn flags(&self, lines: &[&str]) {
        fs::write(&self.params.flags_file, format!("{}\n", lines.join("\n"))).unwrap();
        fs::set_permissions(&self.params.flags_file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn record(&self) -> Option<Record> {
        fs::read(self.params.state_dir.join("record.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }

    fn run(&self, step: Step, own: Result<&str, &str>, units: &dyn Units) -> Done {
        self.run_staged(step, None, own, units)
    }

    /// [`Tree::run`], with the staged release directory `artifact` (for `prepare`).
    fn run_staged(
        &self,
        step: Step,
        artifact: Option<&Path>,
        own: Result<&str, &str>,
        units: &dyn Units,
    ) -> Done {
        let own_host_id = || own.map(str::to_owned).map_err(str::to_owned);
        let machine = Machine {
            stat: &as_root,
            own_host_id: &own_host_id,
            ..Machine::system()
        };
        run_on(
            step,
            artifact,
            &self.params,
            &machine,
            units,
            &Ticks::default(),
        )
    }
}

/// Asserts that a flags file the agent cannot use ends `cutover` as `bad_config`, with the
/// reason as the update hook tells it (`flags_file:` first), with the unit untouched and no
/// record: a flags file that is missing, one that names no server, one that names a facts file
/// that is not there, a host id the hello would refuse, and a machine whose own id cannot be
/// had when the file names none.
///
/// Catches a restart onto flags the agent would refuse to start with.
#[test]
fn a_flags_file_the_agent_cannot_start_with_ends_cutover_as_bad_config_on_the_machine() {
    let cases = [
        ("missing", None::<Vec<&str>>, Ok("web-04"), "cannot be read"),
        (
            "no server",
            Some(vec!["--pool=farm"]),
            Ok("web-04"),
            "required arguments were not provided",
        ),
        (
            "no facts",
            Some(vec![
                "--server=http://127.0.0.1:1",
                "--pool=farm",
                "--host-id-from-facts=/nowhere/facts.json",
            ]),
            Ok("web-04"),
            "--host-id-from-facts ",
        ),
        (
            "bad id",
            Some(vec![
                "--server=http://127.0.0.1:1",
                "--pool=farm",
                "--host-id=bad id",
            ]),
            Ok("web-04"),
            "flags_file: --host-id: ",
        ),
        (
            "no machine id",
            Some(vec!["--server=http://127.0.0.1:1", "--pool=farm"]),
            Err("no machine id"),
            "flags_file: no host id: no machine id",
        ),
    ];
    for (what, flags, own, said) in cases {
        let tree = Tree::new("config-machine-bad-flags", UNIT);
        if let Some(lines) = flags {
            tree.flags(&lines);
        }
        let units = UnitScript::default();
        let done = tree.run(Step::Cutover, own, &units);
        assert_eq!(done.outcome, Outcome::Failure, "{what}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::BadConfig), "{what}");
        assert_eq!(done.report.class, Class::Release, "{what}");
        let detail = done.report.detail.unwrap();
        assert!(detail.starts_with("flags_file: "), "{what}: {detail}");
        assert!(detail.contains(said), "{what}: {detail}");
        assert!(units.log.borrow().is_empty(), "{what}");
        assert!(tree.record().is_none(), "{what}");
    }
}

/// Asserts, over a real lease server and the host's own certificate, that `cutover` records the
/// session the server shows for the host, whether the host's id is the flags file's or the
/// machine's own, and that `verify` finds no hello while the server still shows that session.
///
/// Catches a lease built for another host than the agent's, one without the host certificate,
/// and a session compared as an order rather than by equality.
#[test]
fn over_a_real_server_cutover_records_the_session_and_verify_waits_for_another() {
    let enrolled = enrol("config_hook_machine_real");
    let seen = lease_of(&enrolled.server).host().unwrap();
    let session = seen.session_started_ms;
    assert!(session.is_some());
    for (named, own) in [(true, Err("unused")), (false, Ok("mac-1"))] {
        let tree = Tree::new("config-machine-real", UNIT);
        let mut flags = served_by(&enrolled.server);
        if named {
            flags.push("--host-id=mac-1".to_owned());
        }
        tree.flags(&flags.iter().map(String::as_str).collect::<Vec<_>>());
        let units = UnitScript::default();
        let done = tree.run(Step::Cutover, own, &units);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(
            tree.record().unwrap().session_started_ms,
            session,
            "named {named}"
        );
        assert_eq!(
            units
                .log
                .borrow()
                .iter()
                .filter(|call| *call == "start")
                .count(),
            1
        );
        let done = tree.run(Step::Verify, own, &units);
        assert_eq!(
            done.report.reason,
            Some(Reason::NoHello),
            "named {named}: {done:?}"
        );
    }
}

/// Asserts that, on the machine's own ports, `prepare` succeeds without asking the unit manager
/// or the lease server for anything, and keeps the file the staged package places at its path
/// below the machine's root `/`; that a `rollback` then puts that file back; and that a
/// `prepare` without the staged release directory is `bad_args` and keeps nothing.
///
/// Catches a machine whose ports are built to do work at the start of a step that does none,
/// one that maps the staged tree below another root than `/`, and one that runs `prepare` with
/// no package to read.
#[test]
fn on_the_machine_prepare_keeps_what_the_staged_package_replaces_below_the_root() {
    let tree = Tree::new("config-machine-prepare", UNIT);
    let machine = Machine {
        stat: &as_root,
        ..Machine::system()
    };
    let done = run_step(Step::Prepare, None, &tree.params, &machine);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::BadArgs));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("prepare needs the staged release directory")
    );
    assert!(!tree.params.state_dir.exists());

    tree.flags(&["--server=https://before.example:7300"]);
    let flags = fs::canonicalize(&tree.params.flags_file).unwrap();
    let placed = tree
        .staged
        .join("tree")
        .join(flags.strip_prefix("/").unwrap());
    fs::create_dir_all(placed.parent().unwrap()).unwrap();
    fs::write(&placed, "--server=https://after.example:7300\n").unwrap();
    let done = run_step(Step::Prepare, Some(&tree.staged), &tree.params, &machine);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(tree.params.state_dir.is_dir());

    fs::copy(&placed, &flags).unwrap();
    let done = run_step(Step::Rollback, None, &tree.params, &machine);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let kept = tree.params.state_dir.join("kept");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}; cutover restarted nothing: the unit is left as it is; the copy stays in {}, as removing the package deletes the files it placed",
            flags.display(),
            kept.display()
        ))
    );
    assert_eq!(
        fs::read_to_string(&flags).unwrap(),
        "--server=https://before.example:7300\n"
    );
}

/// Asserts that, on the machine's own ports, `cleanup` with no record left succeeds without
/// asking the unit manager or the lease server anything: an earlier cleanup finished.
///
/// Catches a machine that does not hand `cleanup` to its step.
#[test]
fn on_the_machine_a_cleanup_with_no_record_has_finished() {
    let tree = Tree::new("config-machine-cleanup", UNIT);
    let machine = Machine {
        stat: &as_root,
        ..Machine::system()
    };
    let done = run_step(Step::Cleanup, None, &tree.params, &machine);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some("there is no record: cleanup has finished")
    );
}

/// Asserts that, on the machine's own ports, `rollback` with no record (a cutover that never
/// began a restart) succeeds and leaves the unit alone, without asking the unit manager or the
/// lease server anything.
///
/// Catches a machine that does not hand `rollback` to its step.
#[test]
fn on_the_machine_a_rollback_with_nothing_restarted_leaves_the_unit_alone() {
    let tree = Tree::new("config-machine-rollback", UNIT);
    let machine = Machine {
        stat: &as_root,
        ..Machine::system()
    };
    let done = run_step(Step::Rollback, None, &tree.params, &machine);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cutover restarted nothing: the unit is left as it is")
    );
}

/// One config-hook `cutover` on a Mac's unit manager over `tool`, the fake `launchctl` the test
/// set up for `label`: the flags file in place (the gate the kickstart waits on), and a lease
/// server that has not heard the host yet, so there is no session to note. Returns what the step
/// ended with and the tree's record.
fn launchd_cutover(name: &str, label: &str, tool: &Launchctl) -> (Done, Option<Record>) {
    let server = Server::start(name, &[]);
    let tree = Tree::new(name, label);
    let mut flags = served_by(&server);
    flags.push("--host-id=mac-1".to_owned());
    tree.flags(&flags.iter().map(String::as_str).collect::<Vec<_>>());
    let clock = Ticks::default();
    let units = LaunchUnits::new(tool)
        .with_clock(&clock)
        .with_gate(Path::new(&tree.params.flags_file));
    let done = tree.run(Step::Cutover, Err("unused"), &units);
    (done, tree.record())
}

/// The flags lines that make a host of `server`: its address, the pool, and the certificate and
/// key of `mac-1` and the server certificate they trust.
fn served_by(server: &Server) -> Vec<String> {
    let identities = server.identities();
    let identity = identities.of("mac-1");
    vec![
        format!("--server={}", server.url()),
        "--pool=farm".to_owned(),
        format!("--host-cert={}", identity.cert_path().display()),
        format!("--host-key={}", identity.key_path().display()),
        format!("--ca-cert={}", identities.server_cert().display()),
    ]
}

/// The command line `launchctl <verb> ...` the fake ran.
fn line(args: &str) -> String {
    format!("/bin/launchctl {args}")
}

/// Asserts that on a Mac's unit manager `cutover` boots the service out, bootstraps it again
/// and sees it running, and records the restart: the order where the agent was loaded before
/// its config package came.
///
/// Catches a restart written for systemd's verbs: a launchd service that is stopped is no longer
/// loaded, and only a bootstrap brings it back.
#[test]
fn on_launchd_cutover_boots_the_service_out_and_bootstraps_it_again() {
    let label = "com.example.agent";
    let tool = Launchctl::default();
    tool.load(label, Service::running());
    let (done, record) = launchd_cutover("config-machine-launchd", label, &tool);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let calls = tool.calls();
    let at = |args: &str| calls.iter().position(|call| *call == line(args)).unwrap();
    assert!(
        at(&format!("bootout system/{label}"))
            < at(&format!("bootstrap system {}", plist_of(label))),
        "{calls:?}"
    );
    let record = record.unwrap();
    assert!(record.restarted);
    assert_eq!(record.session_started_ms, None);
    assert_eq!(tool.loaded.borrow()[label].state, "running");
}

/// Asserts that `cutover` on a Mac whose agent's plist is installed and not loaded (the order
/// where the config package came first) bootstraps it, boots nothing out, and records the
/// restart.
///
/// Catches a restart that needs a loaded service to begin with.
#[test]
fn on_launchd_cutover_bootstraps_a_service_that_is_not_loaded() {
    let label = "com.example.agent";
    let tool = Launchctl::default();
    tool.plists.borrow_mut().insert(label.to_owned());
    let (done, record) = launchd_cutover("config-machine-launchd-unloaded", label, &tool);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let calls = tool.calls();
    assert!(
        calls.contains(&line(&format!("bootstrap system {}", plist_of(label)))),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("bootout")),
        "{calls:?}"
    );
    assert!(record.unwrap().restarted);
    assert_eq!(tool.loaded.borrow()[label].state, "running");
}

/// Asserts that `cutover` succeeds when launchd holds the label by the time the hook bootstraps
/// it (it was loaded after the hook's `print` found it absent, and `bootstrap` exits 5): a
/// service that runs is left running, one that is idle is kicked, and the restart is recorded.
///
/// Catches a config hook that halts with `cannot start <label>: /bin/launchctl: exit status: 5`
/// on a service that is loaded when the hook expected it not to be.
#[test]
fn on_launchd_cutover_restarts_a_service_launchd_loaded_before_the_bootstrap() {
    let label = "com.example.agent";
    for (behind, kicks) in [
        (Service::running(), 0),
        (Service::new("not running", 1, Exit::Code(0)), 1),
    ] {
        let tool = Launchctl::default();
        tool.load(label, Service::running());
        tool.loaded_behind
            .borrow_mut()
            .insert(label.to_owned(), behind);
        let (done, record) = launchd_cutover("config-machine-launchd-behind", label, &tool);
        assert_eq!(done.outcome, Outcome::Success, "{kicks}: {done:?}");
        let calls = tool.calls();
        let kicked = calls
            .iter()
            .filter(|call| **call == line(&format!("kickstart -k system/{label}")))
            .count();
        assert_eq!(kicked, kicks, "{calls:?}");
        assert!(record.unwrap().restarted, "{kicks}");
        assert_eq!(tool.loaded.borrow()[label].state, "running", "{kicks}");
    }
}

/// The lease server's reading of the host `mac-1` live in the session that began at 5000.
const LIVE_5000: &str = r#"{"id":"mac-1","state":"live","epoch":3,"session_started_ms":5000,"leases":0,"releasing":false,"agent_version":"1.4.0","runtime":"native"}"#;

/// The same host live in the session that began at 6000: an agent that started again.
const LIVE_6000: &str = r#"{"id":"mac-1","state":"live","epoch":4,"session_started_ms":6000,"leases":0,"releasing":false,"agent_version":"1.4.0","runtime":"native"}"#;

/// How one config update on a Mac's unit manager ended, up to its `verify`.
struct Update {
    cutover: Done,
    verify: Done,
    record: Option<Record>,
    /// The connections the lease server served.
    served: usize,
}

/// One config update on a Mac's unit manager over `tool`, the fake `launchctl` the test set up
/// for `label`, on the machine's root `/`: the flags file in place (the server, the pool `farm`,
/// the host `mac-1`, mode 0600), the package staged with the same flags in the pool `pool`,
/// `prepare`, the installer's activate (the file written beside its path and renamed over it),
/// `cutover` and `verify`, the lease server answering its connections as `replies` says.
fn launchd_update(
    name: &str,
    label: &str,
    tool: &Launchctl,
    pool: &str,
    replies: &[Reply],
) -> Update {
    let tree = Tree::new(name, label);
    let (server_cert, server_key) = server_pair(&tree.files);
    let stub = Stub::start_tls(replies, &server_cert, &server_key);
    let host = Identities::new(&tree.files).flags("mac-1");
    let lines = |pool: &str| {
        let mut lines = vec![
            format!("--server={}", stub.url),
            format!("--pool={pool}"),
            "--host-id=mac-1".to_owned(),
        ];
        // `--host-cert`, `--host-key` and `--ca-cert` (the stub's certificate), as `--flag=file`.
        lines.extend(
            host.chunks(2)
                .map(|pair| format!("{}={}", pair[0], pair[1])),
        );
        lines
    };
    tree.flags(&lines("farm").iter().map(String::as_str).collect::<Vec<_>>());
    let flags = fs::canonicalize(&tree.params.flags_file).unwrap();
    let placed = tree
        .staged
        .join("tree")
        .join(flags.strip_prefix("/").unwrap());
    fs::create_dir_all(placed.parent().unwrap()).unwrap();
    fs::write(&placed, format!("{}\n", lines(pool).join("\n"))).unwrap();
    fs::set_permissions(&placed, fs::Permissions::from_mode(0o600)).unwrap();

    let clock = Ticks::default();
    let units = LaunchUnits::new(tool)
        .with_clock(&clock)
        .with_gate(Path::new(&tree.params.flags_file));
    let own = Err("unused");
    let done = tree.run_staged(Step::Prepare, Some(&tree.staged), own, &units);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let installing = flags.with_file_name(".flags.installing");
    fs::copy(&placed, &installing).unwrap();
    fs::rename(&installing, &flags).unwrap();
    let cutover = tree.run(Step::Cutover, own, &units);
    let verify = tree.run(Step::Verify, own, &units);
    Update {
        cutover,
        verify,
        record: tree.record(),
        served: stub.stop(),
    }
}

/// Whether `calls` hold a command that stops or starts the service.
fn restarts_any(calls: &[String]) -> bool {
    calls.iter().any(|call| {
        ["bootout", "bootstrap", "kickstart"]
            .iter()
            .any(|verb| call.contains(verb))
    })
}

/// Asserts that on a Mac's unit manager a config package re-versioned with the same files is
/// applied in one pass: `cutover` runs no `launchctl` command that stops or starts the agent and
/// asks the lease server nothing, and `verify` succeeds at its first look, on the session the
/// agent was already in.
///
/// Catches the update that failed on every Mac after five minutes with `no_hello` ("no new
/// session of the host said hello in 285 s") while the agent ran on, unrestarted, and that held
/// back every other update of the Mac for that time.
#[test]
fn on_launchd_unchanged_files_succeed_in_one_pass_with_no_restart() {
    let label = "com.example.agent";
    let tool = Launchctl::default();
    tool.load(label, Service::running());
    let update = launchd_update(
        "config-machine-launchd-unchanged",
        label,
        &tool,
        "farm",
        &[Reply::Answer(200, LIVE_5000)],
    );
    assert_eq!(
        update.cutover.outcome,
        Outcome::Success,
        "{:?}",
        update.cutover
    );
    assert_eq!(
        update.cutover.report.detail.as_deref(),
        Some(
            "the files the package placed are as they were before the update: nothing was restarted"
        )
    );
    assert_eq!(
        update.verify.outcome,
        Outcome::Success,
        "{:?}",
        update.verify
    );
    assert_eq!(update.verify.report, Default::default());
    let calls = tool.calls();
    assert!(!restarts_any(&calls), "{calls:?}");
    assert!(tool.launches.borrow().is_empty());
    assert_eq!(tool.loaded.borrow()[label].runs, 1);
    let record = update.record.unwrap();
    assert_eq!(
        (record.unchanged, record.restarted, record.verified),
        (true, false, true)
    );
    assert_eq!(update.served, 1);
}

/// Asserts that on a Mac's unit manager a config package whose flags changed boots the agent
/// out and bootstraps it, and that `verify` waits past the session the agent was in before for
/// the new one.
///
/// Catches an update of changed flags that the comparison takes for unchanged, which would leave
/// the agent on the flags it read at its start.
#[test]
fn on_launchd_changed_files_restart_and_wait_for_the_new_session() {
    let label = "com.example.agent";
    let tool = Launchctl::default();
    tool.load(label, Service::running());
    let update = launchd_update(
        "config-machine-launchd-changed",
        label,
        &tool,
        "farm-2",
        &[
            Reply::Answer(200, LIVE_5000),
            Reply::Answer(200, LIVE_5000),
            Reply::Answer(200, LIVE_6000),
        ],
    );
    assert_eq!(
        update.cutover.outcome,
        Outcome::Success,
        "{:?}",
        update.cutover
    );
    assert_eq!(update.cutover.report, Default::default());
    assert_eq!(
        update.verify.outcome,
        Outcome::Success,
        "{:?}",
        update.verify
    );
    let calls = tool.calls();
    let at = |args: &str| calls.iter().position(|call| *call == line(args)).unwrap();
    assert!(
        at(&format!("bootout system/{label}"))
            < at(&format!("bootstrap system {}", plist_of(label))),
        "{calls:?}"
    );
    assert_eq!(tool.launches.borrow()[label], 1);
    let record = update.record.unwrap();
    assert_eq!(
        (
            record.unchanged,
            record.restarted,
            record.session_started_ms,
            record.verified
        ),
        (false, true, Some(5000), true)
    );
    assert_eq!(update.served, 3);
}

/// Asserts that on a Mac's unit manager, with the files unchanged, an agent that is down (a
/// daemon that died, or a label launchd does not hold) ends `verify` at once as `agent_down`,
/// class environment, naming the agent's label, with nothing started and nothing asked of the
/// lease server.
///
/// Catches a down agent reported as `no_hello` after a wait of the whole bound, a fault of the
/// host told as one of the config.
#[test]
fn on_launchd_a_down_agent_with_unchanged_files_is_agent_down() {
    let label = "com.example.agent";
    let cases = [
        (
            Some(Service::new("not running", 1, Exit::Code(1))),
            "failed (failed)",
        ),
        (None, "inactive (dead)"),
    ];
    for (service, shown) in cases {
        let tool = Launchctl::default();
        tool.plists.borrow_mut().insert(label.to_owned());
        if let Some(service) = service {
            tool.load(label, service);
        }
        let update = launchd_update(
            "config-machine-launchd-down",
            label,
            &tool,
            "farm",
            &[Reply::Answer(200, LIVE_5000)],
        );
        assert_eq!(update.cutover.outcome, Outcome::Success, "{shown}");
        assert_eq!(update.verify.outcome, Outcome::Failure, "{shown}");
        assert_eq!(
            update.verify.report.reason,
            Some(Reason::AgentDown),
            "{shown}"
        );
        assert_eq!(update.verify.report.class, Class::Environment, "{shown}");
        assert_eq!(
            update.verify.report.detail,
            Some(format!(
                "the agent's unit {label} is {shown}: the files did not change, so nothing was \
                 restarted, and the agent is not running"
            ))
        );
        let calls = tool.calls();
        assert!(!restarts_any(&calls), "{shown}: {calls:?}");
        assert!(!update.record.unwrap().verified, "{shown}");
        assert_eq!(update.served, 0, "{shown}");
    }
}
