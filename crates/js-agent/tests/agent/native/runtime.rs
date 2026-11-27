//! The native runtime over the real account pool (on a fake host) and the fake supervisor:
//! what a start asks of the supervisor and puts on disk (the account, the run directory, the
//! profile, the environment, the limits), how an end is told, the logs, the removal, and
//! every way a start fails before or after an account is lent.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use js_agent::lifecycle::{Body, Exit, RunHandle, Runtime, Status};
use js_agent::native::{Ceiling, Journal};
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
use js_agent_wire::{NetworkLevel, refusal};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};

use super::pf_gate::{Fake, Verdict};
use super::{Rig, native, param};

fn handle(lease: &str) -> RunHandle {
    RunHandle(lease.to_owned())
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// The one account the rig's pool keeps ready, by name: the account the next start is lent.
fn ready_account(rig: &Rig) -> String {
    let records = rig.accounts.records();
    assert_eq!(records.len(), 1, "{records:?}");
    records[0].clone()
}

/// The leases the rig's journal holds live.
fn live(rig: &Rig) -> Vec<String> {
    Journal::new(&rig.dir.path("native-leases")).live().unwrap()
}

/// Asserts that a start that was refused or failed lent nothing and left nothing: the pool
/// holds no account and keeps one ready that is not `lent`, no lease is live, and the
/// supervisor was started `launches` times.
fn nothing_left(rig: &Rig, lent: &str, launches: usize) {
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
    let records = rig.accounts.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_ne!(records[0], lent, "the failed start's account is retired");
    assert_eq!(live(rig), Vec::<String>::new());
    assert_eq!(rig.launcher.launched().len(), launches);
}

/// Asserts that a start refused before an account is lent changed nothing: the pool's ready
/// account is still the one it was, no supervisor was started, no lease was written.
fn untouched(rig: &Rig, ready: &str) {
    assert_eq!(rig.accounts.records(), [ready.to_owned()]);
    assert!(rig.pool.held().is_empty());
    assert!(rig.launcher.launched().is_empty());
    assert!(
        !rig.dir.path("native-leases").exists(),
        "the journal was touched"
    );
}

/// Asserts that a start lends the run an account named for the pool's slots, makes the run
/// directory (`exec`, the working directory, with `.home` and `.tmp` inside it) private to
/// the account before the program starts, and asks the supervisor to run the program under a
/// profile that grants writes to the run directory (not to the home, nor to the account's
/// folder entry), in an environment of the agent's value, `TMPDIR` and the account's names
/// (the job's home and `TMPDIR` inside the run directory), as the account's uid and group,
/// with the configured largest file.
///
/// Catches: a program started without its profile, a working directory that is not the run
/// directory, a run directory the account cannot use, and an environment that lacks the
/// account's names.
#[test]
fn a_start_lends_an_account_and_makes_a_run_directory_and_runs_the_program_under_the_profile() {
    let mut rig = Rig::new("start");
    let lent = ready_account(&rig);
    let mut body = native("/usr/bin/id", &["-u"]);
    body.env.insert("A".to_owned(), "b".to_owned());
    let spec = rig.spec("r1", body);
    let id = rig.rt.create_and_start(&spec).unwrap();
    assert_eq!(id, handle("7.0"));
    let home = rig.home_of(&lent);
    for dir in ["exec", "exec/.tmp", "exec/.home"] {
        let mode = fs::metadata(home.join(dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o700, "{dir}");
    }
    let launched = rig.launcher.last();
    assert_eq!(launched.cwd, fs::canonicalize(home.join("exec")).unwrap());
    let tmp = home.join("exec/.tmp");
    let job_home = home.join("exec/.home").display().to_string();
    let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    assert_eq!(
        launched.env,
        [
            pair("A", "b"),
            pair("TMPDIR", &tmp.display().to_string()),
            pair("HOME", &job_home),
            pair("CFFIXED_USER_HOME", &job_home),
            pair("USER", &lent),
            pair("LOGNAME", &lent),
            pair("xcrun_db", &tmp.join("xcrun_db").display().to_string()),
        ]
    );
    assert_eq!(launched.program, "/usr/bin/id");
    assert_eq!(launched.args, ["-u"]);
    assert_eq!(launched.max_file_bytes, 1 << 30, "1 GiB in bytes");
    assert_eq!(
        param(&launched, "RUN"),
        Some(home.join("exec").display().to_string().as_str())
    );
    assert!(
        launched
            .profile
            .contains("(allow file-write* (subpath (param \"RUN\"))")
    );
    assert!(!launched.profile.contains("UENTRY"));
    assert!(
        launched
            .profile
            .ends_with("(deny network-outbound (literal \"/private/var/run/usbmuxd\"))\n"),
        "the floor is the profile's last rule"
    );
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a job's standard input is empty: a program that reads it to its end ends at
/// once with nothing read, and its output is its own.
///
/// Catches: a job started with the agent's terminal or a pipe as its input, which a reader
/// would wait on for ever.
#[test]
fn a_jobs_standard_input_is_empty() {
    let mut rig = Rig::new("stdin");
    let id = rig.start("r1", "/bin/cat", &[]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!((tail.stdout.as_str(), tail.stderr.as_str()), ("", ""));
    rig.rt.remove(&id).unwrap();
}

/// Asserts that every start gets a lease of its own (`7.0`, then `7.1`) and an account of its
/// own: a name, a uid and a home that no other run held at the same time, each with its run
/// directory. Two runs at once need two slots.
///
/// Catches: a lease or an account reused for a second run, which would share one account's
/// files.
#[test]
fn each_start_gets_an_account_of_its_own() {
    let mut rig = Rig::in_dir("leases", 3, |_, _| {});
    let first = rig.start("r1", "/usr/bin/true", &[]);
    let second = rig.start("r2", "/usr/bin/true", &[]);
    assert_eq!((first, second), (handle("7.0"), handle("7.1")));
    let launches = rig.launcher.launched();
    let names: Vec<&str> = launches
        .iter()
        .map(|launched| launched.var("USER").unwrap())
        .collect();
    assert_ne!(names[0], names[1]);
    assert_ne!(launches[0].uid, launches[1].uid);
    assert_ne!(rig.home_of(names[0]), rig.home_of(names[1]));
    for name in names {
        assert!(rig.home_of(name).join("exec").is_dir(), "{name}");
    }
}

/// Asserts that a run's home is the home of the account the pool lent it (named for the
/// pool, `kbf-acct-<n>`, not for the lease), at one of the uids of the pool's slots: each
/// run's directory and its profile's write grant are in that account's home.
///
/// Catches: a run directory made below a home the pool never made, which fails every start on
/// a Mac.
#[test]
fn a_run_lives_in_the_home_of_the_account_the_pool_lent_it() {
    let mut rig = Rig::in_dir("poolhome", 3, |_, _| {});
    rig.start("r1", "/usr/bin/true", &[]);
    rig.start("r2", "/usr/bin/true", &[]);
    for launched in rig.launcher.launched() {
        let name = launched.var("USER").unwrap();
        assert!(name.starts_with("kbf-acct-"), "{name}");
        assert!(!name.contains("7-0") && !name.contains("7.0"), "{name}");
        assert!((super::me()..super::me() + 3).contains(&launched.uid));
        let home = rig.settings.homes.join(name);
        assert_eq!(rig.home_of(name), home);
        assert!(home.join("exec").is_dir(), "{}", home.display());
        assert_eq!(
            param(&launched, "RUN"),
            Some(home.join("exec").display().to_string().as_str())
        );
    }
}

/// Asserts the network rules of the profile: a job of the level `none` (`off` on the wire) and
/// one asking `loopback` have the loopback rule under every ceiling but `off`, which denies the
/// network, and an open network has no network rule at all; a request above the ceiling,
/// given to the runtime directly, is still held at the ceiling. An agent refuses such a start before it
/// reaches the runtime (`check_native`); the profile's own bound is a defence in depth.
///
/// Catches: a profile that grants more than the ceiling, a level mapped to the wrong rules,
/// and a `none` job left with the network denied where the ceiling allows loopback.
#[test]
fn a_runs_network_is_the_lower_of_its_request_and_the_agents_cap() {
    let loopback = "(allow network* (remote ip \"localhost:*\"))";
    let cases = [
        (Ceiling::Loopback, NetworkLevel::Off, true, true),
        (Ceiling::Off, NetworkLevel::Off, false, true),
        (Ceiling::Loopback, NetworkLevel::Loopback, true, true),
        (Ceiling::Loopback, NetworkLevel::On, true, true),
        (Ceiling::Off, NetworkLevel::On, false, true),
        (Ceiling::On, NetworkLevel::On, false, false),
        (Ceiling::On, NetworkLevel::Off, true, true),
    ];
    for (n, (cap, asked, has_loopback, denies)) in cases.into_iter().enumerate() {
        let mut rig = Rig::with(&format!("net{n}"), |config| {
            config.network = cap;
            config.egress = Some(Fake::new(Verdict::Enforced));
        });
        let mut body = native("/usr/bin/true", &[]);
        body.network = asked;
        let spec = rig.spec("r1", body);
        rig.rt.create_and_start(&spec).unwrap();
        let profile = rig.launcher.last().profile;
        let said = format!("{cap:?} cap, {asked:?} asked");
        assert_eq!(profile.contains(loopback), has_loopback, "{said}");
        assert_eq!(profile.contains("(deny network*)"), denies, "{said}");
    }
}

/// Asserts that the paths the agent keeps from jobs are read-denied in the profile, as
/// parameters.
///
/// Catches: a profile that lets a run read the agent's state.
#[test]
fn the_agents_own_paths_are_denied_to_the_run() {
    let mut rig = Rig::new("denies");
    rig.start("r1", "/usr/bin/true", &[]);
    let launched = rig.launcher.last();
    assert!(
        launched
            .params
            .contains(&("RD_0".to_owned(), "/srv/js-state".to_owned()))
    );
    assert!(
        launched
            .profile
            .contains("(deny file-read* (subpath (param \"RD_0\")))")
    );
}

/// Asserts that the profile is built from the policy and the paths of the run only: two
/// runs that differ only in their environment, one of them with a value that would break
/// out of a quoted string, are given the same profile text and the same parameters (but the
/// run directory's and the account's home, `RUN` and `OWN`).
///
/// Catches: a value of the job's environment that reaches the profile.
#[test]
fn the_profile_does_not_depend_on_the_jobs_environment() {
    let mut rig = Rig::in_dir("profile-env", 3, |_, _| {});
    let plain = native("/usr/bin/id", &["-u"]);
    let mut hostile = native("/usr/bin/id", &["-u"]);
    hostile.env.insert(
        "A".to_owned(),
        "\") (allow file-write* (subpath \"/\")".to_owned(),
    );
    for (n, body) in [plain, hostile].into_iter().enumerate() {
        let spec = rig.spec(&format!("r{n}"), body);
        rig.rt.create_and_start(&spec).unwrap();
    }
    let launches = rig.launcher.launched();
    assert_eq!(launches[0].profile, launches[1].profile);
    let params = |at: usize| -> Vec<(String, String)> {
        launches[at]
            .params
            .iter()
            .filter(|(name, _)| name != "RUN" && name != "OWN")
            .cloned()
            .collect()
    };
    assert_eq!(params(0), params(1));
}

/// Asserts how a run's end is told: a job that is running is polled as running; an exit code
/// is the run's; a job a signal ended has the status a shell gives it (128 and the signal);
/// and the answer is the same on every later poll.
///
/// Catches: an exit status lost between the supervisor and the lifecycle and a poll that
/// blocks.
#[test]
fn a_run_is_running_until_its_job_ends_and_its_end_is_told_the_same_every_time() {
    let mut rig = Rig::in_dir("ends", 4, |_, _| {});
    let running = rig.start("r0", "/bin/sleep", &["600"]);
    assert_eq!(rig.rt.poll(&running).unwrap(), Status::Running);
    rig.rt
        .stop(&running, std::time::Duration::from_secs(600))
        .unwrap();
    assert_eq!(rig.settle(&running), exited(143));
    rig.rt.remove(&running).unwrap();

    let ends = [
        ("exit 3", exited(3)),
        ("kill -HUP $$", exited(128 + 1)),
        ("exit 0", exited(0)),
    ];
    for (n, (script, want)) in ends.into_iter().enumerate() {
        let id = rig.start(&format!("r{}", n + 1), "/bin/sh", &["-c", script]);
        assert_eq!(rig.settle(&id), want, "{script}");
        assert_eq!(rig.rt.poll(&id).unwrap(), want, "asked again");
        rig.rt.remove(&id).unwrap();
    }
}

/// Asserts that a handle the runtime never made is `Gone`, has no log and no gpu, that
/// stopping or removing it is no error, and that the runtime lists nothing to adopt.
///
/// Catches: a poll of a stranger that errs and a runtime that claims runs it cannot find
/// again.
#[test]
fn an_unknown_handle_is_gone_and_nothing_is_listed() {
    let mut rig = Rig::new("unknown");
    let stranger = handle("9.9");
    assert_eq!(rig.rt.poll(&stranger).unwrap(), Status::Gone);
    assert_eq!(rig.rt.log_tail(&stranger).unwrap(), None);
    rig.rt.stop(&stranger, std::time::Duration::ZERO).unwrap();
    rig.rt.remove(&stranger).unwrap();
    assert!(rig.rt.list_labelled().unwrap().is_empty());
    assert!(rig.rt.list_ids().unwrap().is_empty());
    assert!(!rig.rt.supports_gpus());
}

/// Asserts that the two streams of the job are the two log files: what the program wrote to
/// its standard output and standard error comes back, apart, as the tail.
///
/// Catches: streams swapped or merged.
#[test]
fn the_tail_is_what_the_job_wrote_to_each_stream() {
    let mut rig = Rig::new("tail");
    let script = "printf 'to out\\n'; printf 'to err\\n' >&2";
    let id = rig.start("r1", "/bin/sh", &["-c", script]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "to out\n");
    assert_eq!(tail.stderr, "to err\n");
    assert!(!tail.truncated);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a job that wrote more than the tail holds on either stream keeps the end of
/// that stream and is marked truncated, and that one that wrote less is not.
///
/// Catches: a tail that grows with the log and a truncation nobody is told of.
#[test]
fn a_long_stream_is_cut_to_its_end_and_marked() {
    let mut rig = Rig::new("long");
    let long = MAX_LOG_STREAM_LEN + 100;
    let cases = [(long, 1, true), (1, long, true), (1, 1, false)];
    for (n, (out, err, truncated)) in cases.into_iter().enumerate() {
        let script = format!(
            "/usr/bin/head -c {out} /dev/zero | /usr/bin/tr '\\0' o; \
             /usr/bin/head -c {err} /dev/zero | /usr/bin/tr '\\0' e >&2"
        );
        let id = rig.start(&format!("r{n}"), "/bin/sh", &["-c", &script]);
        assert_eq!(rig.settle(&id), exited(0), "case {n}");
        let tail = rig.rt.log_tail(&id).unwrap().unwrap();
        assert_eq!(tail.truncated, truncated, "case {n}");
        assert!(tail.stdout.len() <= MAX_LOG_STREAM_LEN);
        assert!(tail.stderr.len() <= MAX_LOG_STREAM_LEN);
        assert!(tail.stdout.chars().all(|c| c == 'o'), "case {n}");
        assert!(tail.stderr.chars().all(|c| c == 'e'), "case {n}");
        rig.rt.remove(&id).unwrap();
    }
}

/// Asserts that a log file that cannot be read is an error of the log call, not a panic or
/// an empty tail.
///
/// Catches: a result that claims the job printed nothing.
#[test]
fn a_log_that_cannot_be_read_is_an_error() {
    let mut rig = Rig::new("nolog");
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    let id = rig.rt.create_and_start(&spec).unwrap();
    rig.settle(&id);
    fs::remove_file(spec.scratch.join("stdout")).unwrap();
    let error = rig.rt.log_tail(&id).unwrap_err();
    assert!(error.0.starts_with("the log: "), "{}", error.0);
}

/// Asserts that a removal ends the job's processes (a job still running is ended), retires
/// the account (its record and its home are gone), ends the lease, and forgets the run: it
/// polls `Gone` and has no log.
///
/// Catches: an account left on the Mac after its job, or a run remembered after its removal.
#[test]
fn a_removal_ends_the_job_retires_the_account_and_forgets_the_run() {
    let mut rig = Rig::new("remove");
    let lent = ready_account(&rig);
    let id = rig.start("r1", "/bin/sleep", &["600"]);
    let home = rig.home_of(&lent);
    assert!(home.is_dir());
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    rig.rt.remove(&id).unwrap();
    assert!(!home.exists());
    assert!(!rig.accounts.records().contains(&lent));
    assert!(rig.pool.held().is_empty());
    assert_eq!(live(&rig), Vec::<String>::new());
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Gone);
    assert_eq!(rig.rt.log_tail(&id).unwrap(), None);
}

/// Asserts that every failure after the account is lent leaves nothing behind: the account
/// is retired, the lease is done, no job runs, and the start's error names the first
/// failure. The failures: no scratch directory (the supervisor's records cannot be made), a
/// records directory already there, a directory in the place of each of the three
/// standard-stream files, and a program the profile cannot name.
///
/// Catches: an account, a lease or a home left on the Mac by a start that failed.
#[test]
fn a_start_that_fails_after_the_account_is_lent_leaves_nothing() {
    let cases = [
        ("no scratch directory", "/supervisor: "),
        ("a records directory already there", "/supervisor: "),
        ("a directory for stdin", "/stdin: "),
        ("a directory for stdout", "/stdout: "),
        ("a directory for stderr", "/stderr: "),
        ("program the profile cannot name", "/tmp/tool: "),
    ];
    for (n, (why, text)) in cases.into_iter().enumerate() {
        let mut rig = Rig::new(&format!("fail{n}"));
        let lent = ready_account(&rig);
        let mut spec = rig.spec("r1", native("/usr/bin/true", &[]));
        match n {
            0 => fs::remove_dir(&spec.scratch).unwrap(),
            1 => fs::create_dir(spec.scratch.join("supervisor")).unwrap(),
            2 => fs::create_dir(spec.scratch.join("stdin")).unwrap(),
            3 => fs::create_dir(spec.scratch.join("stdout")).unwrap(),
            4 => fs::create_dir(spec.scratch.join("stderr")).unwrap(),
            _ => spec.body = Body::Native(native("/tmp/tool", &[])),
        }
        let error = rig.rt.create_and_start(&spec).unwrap_err();
        assert!(error.0.contains(text), "{why}: {}", error.0);
        nothing_left(&rig, &lent, 0);
    }
}

/// Asserts that a home the profile cannot name (here one spelled with `..`, which the
/// kernel resolves and a profile parameter may not hold) fails the start by name and retires
/// the account, and that under the allow read model a run that pins no bundle starts under the
/// open model: no default read deny in its profile.
///
/// Catches: a run started under a profile built from a path that was never checked, and a
/// run without a bundle held to an allow-list its program was never granted for.
#[test]
fn a_home_the_profile_cannot_name_fails_the_start_and_a_run_without_a_bundle_reads_openly() {
    // The same directory as the pool's homes, spelled through its parent.
    let mut rig = Rig::with("oddhome", |config| {
        config.homes = config.homes.join("..").join("Users");
    });
    let lent = ready_account(&rig);
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.contains("not in normal form"), "{}", error.0);
    nothing_left(&rig, &lent, 0);

    let mut rig = Rig::with("allow", |config| config.read_model = ReadModel::Allow);
    let id = rig.start("r1", "/usr/bin/true", &[]);
    let profile = rig.launcher.last().profile;
    assert!(
        !profile.lines().any(|line| line == "(deny file-read*)"),
        "{profile}"
    );
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a policy over the profile's bounds (too many read-denied paths) fails the
/// start with the policy's name and retires the account.
///
/// Catches: a profile built from a policy the builder refused.
#[test]
fn a_policy_over_the_bounds_fails_the_start_and_retires_the_account() {
    let mut rig = Rig::with("bounds", |config| {
        config.read_denies = (0..65)
            .map(|n| AbsPath::parse(&format!("/srv/deny{n}")).unwrap())
            .collect();
    });
    let lent = ready_account(&rig);
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("the policy: "), "{}", error.0);
    nothing_left(&rig, &lent, 0);
}

/// Asserts that a body that is not native lends no account.
///
/// Catches: a container start handed to the Mac runtime that lends an account for nothing.
#[test]
fn a_container_body_is_refused_before_any_account_is_lent() {
    let mut rig = Rig::new("container");
    let ready = ready_account(&rig);
    let mut spec = rig.spec("r1", native("/usr/bin/true", &[]));
    spec.body = Body::Container {
        image: "registry.example/job:1".to_owned(),
        args: Vec::new(),
    };
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(error.0, "this runtime runs native bodies only");
    untouched(&rig, &ready);
}

/// Asserts what the runtime answers a native start before anything is made: a program under
/// a tool root passes; one outside every root is `invalid-program`; a mount input and a
/// working directory below `exec/`, which this runtime stages, and declared outputs, which
/// it collects, pass.
///
/// Catches: a program run from anywhere on the Mac.
#[test]
fn a_native_start_is_checked_against_the_tool_roots_and_what_is_staged() {
    let rig = Rig::new("check");
    assert_eq!(rig.rt.check_native(&native("/usr/bin/true", &[])), Ok(()));
    assert_eq!(rig.rt.check_native(&native("/bin/sh", &[])), Ok(()));
    let outside = rig
        .rt
        .check_native(&native("/usr/binx/true", &[]))
        .unwrap_err();
    assert_eq!(outside.code, refusal::INVALID_PROGRAM);
    assert_eq!(
        rig.rt
            .check_native(&native("/opt/tool", &[]))
            .unwrap_err()
            .code,
        refusal::INVALID_PROGRAM
    );
    let mut with_input = native("/bin/sh", &[]);
    with_input.inputs.push(js_agent_wire::InputRef::Mount {
        root_name: "data".to_owned(),
        rel: "x".to_owned(),
        dest: "in".to_owned(),
    });
    let mut with_output = native("/bin/sh", &[]);
    with_output.outputs.push(js_agent_wire::OutputSpec {
        path: "out".to_owned(),
        kind: js_agent_wire::OutputKind::File,
    });
    let mut with_cwd = native("/bin/sh", &[]);
    with_cwd.cwd = "sub".to_owned();
    for body in [with_input, with_cwd, with_output] {
        assert_eq!(rig.rt.check_native(&body), Ok(()));
    }
}

/// Asserts that a start refuses, by name, each environment name that steers a shell
/// (`SHELLOPTS`, `BASHOPTS`, `BASH_ENV`, `ENV`, `PS4`, `PROMPT_COMMAND`, `IFS`, `CDPATH`),
/// any name starting `BASH_FUNC_`, and each name the agent owns, before an account is lent:
/// no supervisor is started and the lease journal is not made.
///
/// Catches: a body that skipped the wire's validation and reaches a program that is a shell,
/// which reads the environment the job has.
#[test]
fn a_start_refuses_reserved_env_names_before_an_account_is_lent() {
    use js_types::env::{OWNED_ENV_NAMES, SHELL_ENV_NAMES};

    let mut rig = Rig::new("shell-names");
    let ready = ready_account(&rig);
    let names = SHELL_ENV_NAMES
        .iter()
        .chain(OWNED_ENV_NAMES.iter())
        .copied()
        .chain(["BASH_FUNC_ulimit%%", "BASH_FUNC_x"]);
    for (at, name) in names.enumerate() {
        let mut body = native("/usr/bin/true", &[]);
        body.env = BTreeMap::from([
            ("OK".to_owned(), "1".to_owned()),
            (name.to_owned(), "x".to_owned()),
        ]);
        let spec = rig.spec(&format!("r{at}"), body);
        let refused = rig.rt.create_and_start(&spec).err();
        let text = refused.map(|why| why.0).unwrap_or_default();
        assert!(
            text.contains(name),
            "{name} was not refused by name: {text:?}"
        );
    }
    untouched(&rig, &ready);
}

/// Asserts that the configured limits reach the supervisor as fields of its spec, in bytes
/// and as a count, exactly as configured (a file limit of 10 bytes is 10 bytes; the
/// processes are the configured count and one more, for the supervisor itself).
///
/// Catches: a run that starts without the limits an operator set, and limits sent in blocks.
#[test]
fn the_configured_limits_travel_to_the_supervisor_as_fields() {
    let mut rig = Rig::with("limits", |config| {
        config.max_file_bytes = 10;
        config.max_processes = 7;
    });
    rig.start("r1", "/usr/bin/true", &[]);
    let launched = rig.launcher.last();
    assert_eq!(launched.max_file_bytes, 10);
    assert_eq!(launched.max_processes, 8);
    let mut big = Rig::with("limits-big", |config| config.max_file_bytes = 3 << 20);
    big.start("r1", "/usr/bin/true", &[]);
    assert_eq!(big.launcher.last().max_file_bytes, 3 << 20);
}

/// Asserts that a job environment that names `TMPDIR`, which the agent sets, is refused before
/// an account is lent, as it is for a run that pins a bundle.
///
/// Catches: a plain run that takes the job's `TMPDIR` beside the agent's and leaves the
/// choice to the program.
#[test]
fn a_job_that_sets_tmpdir_is_refused_before_an_account_is_lent() {
    let mut rig = Rig::new("tmpdir");
    let ready = ready_account(&rig);
    let mut body = native("/usr/bin/id", &[]);
    body.env
        .insert("TMPDIR".to_owned(), "/private/tmp".to_owned());
    let spec = rig.spec("r1", body);
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(why.0.contains("TMPDIR"), "{}", why.0);
    untouched(&rig, &ready);
}
