//! The native runtime against the fake helper: what a start asks of the helper and puts on
//! disk, the command line it builds, how an end is told, the logs, the kill and the removal,
//! and every way a start fails.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use js_agent::lifecycle::{Body, Exit, RunHandle, Runtime, Status};
use js_agent::native::NativeRuntime;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
use js_agent_wire::{NetworkLevel, refusal};
use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};
use js_mac_session::client::Client;
use js_mac_session::helper::RESET_ALREADY;

use super::fake_helper::End;
use super::{LIMITER, Rig, TERM, config, native, parse};

fn handle(lease: &str) -> RunHandle {
    RunHandle(lease.to_owned())
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Asserts that a start asks the helper for a user named for the run's lease, makes the run
/// directory (`exec`, the working directory, and `tmp`) private to that user before the
/// program starts, hands the helper a working directory and an empty standard input, and
/// runs the program under `sandbox-exec` with a profile that grants writes to the home and
/// not to the user's folder entry, with `TMPDIR` inside the home. It catches a program
/// started without its profile, a working directory that is not a directory, a run
/// directory the user cannot use, and a start that skips the helper's reset.
#[test]
fn a_start_makes_a_user_and_a_run_directory_and_runs_the_program_under_the_profile() {
    let mut rig = Rig::new("start");
    let mut native = native("/usr/bin/id", &["-u"]);
    native.env.insert("A".to_owned(), "b".to_owned());
    let spec = rig.spec("r1", native);
    let id = rig.rt.create_and_start(&spec).unwrap();
    assert_eq!(id, handle("7.0"));
    assert_eq!(
        rig.fake.verbs(),
        ["boot-reset", "user-create 7.0", "run 7.0"]
    );
    let home = rig.fake.home_of("7.0");
    for dir in ["exec", "tmp"] {
        let mode = fs::metadata(home.join(dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o700, "{dir}");
    }
    let run = rig.fake.run_of("7.0");
    assert!(run.cwd_is_dir, "the working directory is a directory");
    assert_eq!(run.stdin_bytes, 0, "standard input is empty");
    let tmp = home.join("tmp").display().to_string();
    assert_eq!(
        run.env,
        [("A".to_owned(), "b".to_owned()), ("TMPDIR".to_owned(), tmp)]
    );
    let parsed = parse(&run.argv);
    assert_eq!(parsed.tail, ["/usr/bin/id", "-u"]);
    assert_eq!(
        parsed.limits,
        ("1048576".to_owned(), "512".to_owned()),
        "1 GiB in blocks of 1024 bytes, and 512 processes"
    );
    assert_eq!(
        parsed.params[0],
        ("RUN".to_owned(), home.display().to_string())
    );
    assert!(
        parsed
            .profile
            .contains("(allow file-write* (subpath (param \"RUN\"))")
    );
    assert!(!parsed.profile.contains("UENTRY"));
    assert!(
        parsed
            .profile
            .ends_with("(deny network-outbound (literal \"/private/var/run/usbmuxd\"))\n"),
        "the floor is the profile's last rule"
    );
}

/// Asserts that every start gets a lease of its own (`7.0`, then `7.1`), so a user's name is
/// never used twice, and a home of its own. It catches a lease reused for a second run,
/// which the helper refuses and which would share one user's files.
#[test]
fn each_start_gets_a_user_of_its_own() {
    let mut rig = Rig::new("leases");
    let first = rig.start("r1", "/bin/true", &[]);
    let second = rig.start("r2", "/bin/true", &[]);
    assert_eq!((first, second), (handle("7.0"), handle("7.1")));
    assert_ne!(rig.fake.home_of("7.0"), rig.fake.home_of("7.1"));
    assert!(rig.fake.home_of("7.1").join("exec").is_dir());
}

/// Asserts the network rules by the lower of what the start asks and what the agent allows:
/// `off` has no loopback rule, `loopback` has it, a request above the cap is held at the
/// cap, and an open network has no network rule at all. It catches a request that raises
/// the agent's cap and a level mapped to the wrong rules.
#[test]
fn a_runs_network_is_the_lower_of_its_request_and_the_agents_cap() {
    let loopback = "(allow network* (remote ip \"localhost:*\"))";
    let cases = [
        (Net::Loopback, NetworkLevel::Off, false, true),
        (Net::Loopback, NetworkLevel::Loopback, true, true),
        (Net::Loopback, NetworkLevel::On, true, true),
        (Net::Off, NetworkLevel::On, false, true),
        (Net::On, NetworkLevel::On, false, false),
        (Net::On, NetworkLevel::Off, false, true),
    ];
    for (n, (cap, asked, has_loopback, denies)) in cases.into_iter().enumerate() {
        let mut rig = Rig::with(&format!("net{n}"), |config| config.net = cap.clone());
        let mut body = native("/bin/true", &[]);
        body.network = asked;
        let spec = rig.spec("r1", body);
        rig.rt.create_and_start(&spec).unwrap();
        let profile = parse(&rig.fake.run_of("7.0").argv).profile;
        let said = format!("{cap:?} cap, {asked:?} asked");
        assert_eq!(profile.contains(loopback), has_loopback, "{said}");
        assert_eq!(profile.contains("(deny network*)"), denies, "{said}");
    }
}

/// Asserts that the paths the agent keeps from jobs are read-denied in the profile, as
/// parameters. It catches a profile that lets a run read the agent's state.
#[test]
fn the_agents_own_paths_are_denied_to_the_run() {
    let mut rig = Rig::new("denies");
    rig.start("r1", "/bin/true", &[]);
    let parsed = parse(&rig.fake.run_of("7.0").argv);
    assert!(
        parsed
            .params
            .contains(&("RD_0".to_owned(), "/srv/js-state".to_owned()))
    );
    assert!(
        parsed
            .profile
            .contains("(deny file-read* (subpath (param \"RD_0\")))")
    );
}

/// Asserts how a run's end is told: running until the helper reports it; then the exit
/// code; a process a signal ended has the status a shell gives it (128 and the signal); an
/// end with neither is 255; and the answer is the same on every later poll. It catches an
/// exit status lost between the helper and the lifecycle and a poll that blocks.
#[test]
fn a_run_is_running_until_the_helper_reports_its_end() {
    let mut rig = Rig::new("ends");
    let ends = [
        End::Exit(Some(3), None),
        End::Exit(None, Some(11)),
        End::Exit(None, None),
    ];
    let want = [exited(3), exited(139), exited(255)];
    for (n, (end, want)) in ends.into_iter().zip(want).enumerate() {
        let id = rig.start(&format!("r{n}"), "/bin/true", &[]);
        assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
        rig.fake.end(&id.0, end);
        assert_eq!(rig.settle(&id), want);
        assert_eq!(rig.rt.poll(&id).unwrap(), want, "asked again");
    }
}

/// Asserts that a helper connection that ends with no exit makes the run `Gone`, which the
/// lifecycle ends as failed and removes (killing whatever the user still runs). It catches
/// a run polled as running for ever after the helper went away.
#[test]
fn a_helper_that_hangs_up_leaves_a_run_gone() {
    let mut rig = Rig::new("hangup");
    let id = rig.start("r1", "/bin/true", &[]);
    rig.fake.end(&id.0, End::HangUp);
    assert_eq!(rig.settle(&id), Status::Gone);
}

/// Asserts that a handle the runtime never made is `Gone`, has no log and no gpu, and that
/// the runtime lists nothing to adopt. It catches a poll of a stranger that errs and a
/// runtime that claims runs it cannot find again.
#[test]
fn an_unknown_handle_is_gone_and_nothing_is_listed() {
    let mut rig = Rig::new("unknown");
    let stranger = handle("9.9");
    assert_eq!(rig.rt.poll(&stranger).unwrap(), Status::Gone);
    assert_eq!(rig.rt.log_tail(&stranger).unwrap(), None);
    assert!(rig.rt.list_labelled().unwrap().is_empty());
    assert!(rig.rt.list_ids().unwrap().is_empty());
    assert!(!rig.rt.supports_gpus());
}

/// Asserts that the two streams of the job are the two log files: what the helper's process
/// wrote to its standard output and standard error comes back, apart, as the tail. It
/// catches streams swapped or merged.
#[test]
fn the_tail_is_what_the_job_wrote_to_each_stream() {
    let mut rig = Rig::new("tail");
    rig.fake.print(b"to out\n", b"to err\n");
    let id = rig.start("r1", "/bin/true", &[]);
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "to out\n");
    assert_eq!(tail.stderr, "to err\n");
    assert!(!tail.truncated);
}

/// Asserts that a job that wrote more than the tail holds on either stream keeps the end of
/// that stream and is marked truncated, and that one that wrote less is not. It catches a
/// tail that grows with the log and a truncation nobody is told of.
#[test]
fn a_long_stream_is_cut_to_its_end_and_marked() {
    let mut rig = Rig::new("long");
    let long = vec![b'a'; MAX_LOG_STREAM_LEN + 100];
    let (one, two) = (b"o".as_slice(), b"e".as_slice());
    let cases = [
        (long.as_slice(), two, true),
        (one, long.as_slice(), true),
        (one, two, false),
    ];
    for (n, (out, err, truncated)) in cases.into_iter().enumerate() {
        rig.fake.print(out, err);
        let id = rig.start(&format!("r{n}"), "/bin/true", &[]);
        let tail = rig.rt.log_tail(&id).unwrap().unwrap();
        assert_eq!(tail.truncated, truncated, "case {n}");
        assert!(tail.stdout.len() <= MAX_LOG_STREAM_LEN);
        assert!(tail.stderr.len() <= MAX_LOG_STREAM_LEN);
    }
}

/// Asserts that a log file that cannot be read is an error of the log call, not a panic or
/// an empty tail. It catches a result that claims the job printed nothing.
#[test]
fn a_log_that_cannot_be_read_is_an_error() {
    let mut rig = Rig::new("nolog");
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let id = rig.rt.create_and_start(&spec).unwrap();
    fs::remove_file(spec.scratch.join("stdout")).unwrap();
    let error = rig.rt.log_tail(&id).unwrap_err();
    assert!(error.0.starts_with("the log: "), "{}", error.0);
}

/// Asserts that a stop asks the helper to kill the user (every process of it, not only the
/// one started), that the run then ends by the kill's signal, and that a refused kill is an
/// error. It catches a stop that signals one process and leaves its children.
#[test]
fn a_stop_kills_every_process_of_the_user() {
    let mut rig = Rig::new("stop");
    let id = rig.start("r1", "/bin/true", &[]);
    rig.rt
        .stop(&id, std::time::Duration::from_secs(10))
        .unwrap();
    assert_eq!(rig.fake.verbs().last().unwrap(), "kill-uid 7.0");
    assert_eq!(rig.settle(&id), exited(137));
    rig.fake.refuse("kill-uid", "no");
    let error = rig.rt.stop(&id, std::time::Duration::ZERO).unwrap_err();
    assert!(error.0.starts_with("kill-uid: "), "{}", error.0);
}

/// Asserts that a removal kills the user, deletes it (the helper sweeps its home), forgets
/// the run, and that a removal the helper refuses is an error that keeps the run and can be
/// tried again. It catches a lease user left on the Mac after its job.
#[test]
fn a_removal_kills_and_deletes_the_user_and_forgets_the_run() {
    let mut rig = Rig::new("remove");
    let id = rig.start("r1", "/bin/true", &[]);
    let home = rig.fake.home_of("7.0");
    assert!(home.is_dir());
    rig.fake.refuse("user-delete", "busy");
    let error = rig.rt.remove(&id).unwrap_err();
    assert!(error.0.starts_with("user-delete: "), "{}", error.0);
    assert!(home.is_dir(), "a refused delete removes nothing");
    assert_ne!(rig.rt.poll(&id).unwrap(), Status::Gone, "the run is kept");
    rig.fake.refuse("kill-uid", "stuck");
    let error = rig.rt.remove(&id).unwrap_err();
    assert!(error.0.starts_with("kill-uid: "), "{}", error.0);
    rig.fake.allow("kill-uid");
    rig.fake.allow("user-delete");
    rig.rt.remove(&id).unwrap();
    assert!(!home.exists());
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Gone);
    assert_eq!(rig.rt.log_tail(&id).unwrap(), None);
}

/// Asserts that a start the helper refuses at `user-create` removes what the helper may
/// have recorded before it refused: the helper records the lease in its ledger, holding a
/// uid, before it makes the home and the account, so a refusal after that point leaves a
/// live lease. The start asks for a kill and a delete of the lease, and when that fails too
/// the error says so. A refusal that the lease was used before is the one exception: that
/// lease is not ours to remove. It catches a helper whose uid range is used up by failed
/// starts.
#[test]
fn a_user_the_helper_refuses_to_make_is_removed_again() {
    let mut rig = Rig::new("nouser");
    rig.fake.refuse("user-create", "range full");
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("user-create: "), "{}", error.0);
    assert!(error.0.contains("range full"));
    assert_eq!(
        rig.fake.verbs(),
        [
            "boot-reset",
            "user-create 7.0",
            "kill-uid 7.0",
            "user-delete 7.0"
        ]
    );

    rig.fake.refuse("kill-uid", "stuck");
    rig.fake.refuse("user-delete", "busy");
    let spec = rig.spec("r2", native("/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.contains("range full"), "{}", error.0);
    for told in ["kill-uid: ", "stuck", "user-delete: ", "busy"] {
        assert!(error.0.contains(told), "{told}: {}", error.0);
    }

    let mut rig = Rig::new("used");
    rig.fake.refuse(
        "user-create",
        "lease 7.0 was used before; a lease user's name is never reused",
    );
    let spec = rig.spec("r1", native("/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(rig.fake.verbs(), ["boot-reset", "user-create 7.0"]);
}

/// Asserts that every failure after the user exists leaves nothing behind: the helper is
/// asked to kill and delete the user, and the start's error names the first failure. The
/// failures: the helper refusing `run`, no home to make the run directory in, none of the
/// three standard-stream files made (no scratch directory, or a directory in the file's
/// place) and a program the profile cannot name. It catches a lease user, and a home, left
/// on the Mac by a start that failed.
#[test]
fn a_start_that_fails_after_the_user_exists_removes_the_user() {
    let rollback = ["kill-uid 7.0", "user-delete 7.0"];
    let cases = [
        ("run refused", "run: "),
        ("no home", "/exec: "),
        ("no scratch directory", "/stdin: "),
        ("a directory for stdin", "/stdin: "),
        ("a directory for stdout", "/stdout: "),
        ("a directory for stderr", "/stderr: "),
        ("program the profile cannot name", "/tmp/tool: "),
    ];
    for (n, (why, text)) in cases.into_iter().enumerate() {
        let mut rig = Rig::new(&format!("fail{n}"));
        let mut spec = rig.spec("r1", native("/bin/true", &[]));
        match n {
            0 => rig.fake.refuse("run", "no"),
            1 => rig.fake.make_homes(false),
            2 => fs::remove_dir(&spec.scratch).unwrap(),
            3 => fs::create_dir(spec.scratch.join("stdin")).unwrap(),
            4 => fs::create_dir(spec.scratch.join("stdout")).unwrap(),
            5 => fs::create_dir(spec.scratch.join("stderr")).unwrap(),
            _ => spec.body = Body::Native(native("/tmp/tool", &[])),
        }
        let error = rig.rt.create_and_start(&spec).unwrap_err();
        assert!(error.0.contains(text), "{why}: {}", error.0);
        let verbs = rig.fake.verbs();
        assert_eq!(verbs[verbs.len() - 2..], rollback, "{why}");
    }
}

/// Asserts that a home the profile cannot name (here one spelled with `..`, which the
/// kernel resolves and a profile parameter may not hold) and a read model with no profile
/// builder each fail the start by name and remove the user. It catches a run started under
/// a profile built from a path that was never checked, and a pool configured for a model
/// that is not built that runs under the open one.
#[test]
fn a_home_or_a_read_model_the_profile_cannot_serve_fails_the_start() {
    let mut rig = Rig::with("oddhome", |config| {
        config.homes = config.homes.join("..").join("homes");
    });
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.contains("not in normal form"), "{}", error.0);
    assert_eq!(rig.fake.verbs().last().unwrap(), "user-delete 7.0");

    let mut rig = Rig::with("allow", |config| config.read_model = ReadModel::Allow);
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("the profile: "), "{}", error.0);
    assert_eq!(rig.fake.verbs().last().unwrap(), "user-delete 7.0");
}

/// Asserts that a policy over the profile's bounds (too many read-denied paths) fails the
/// start with the policy's name and removes the user. It catches a profile built from a
/// policy the builder refused.
#[test]
fn a_policy_over_the_bounds_fails_the_start_and_removes_the_user() {
    let mut rig = Rig::with("bounds", |config| {
        config.read_denies = (0..65)
            .map(|n| AbsPath::parse(&format!("/srv/deny{n}")).unwrap())
            .collect();
    });
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("the policy: "), "{}", error.0);
    assert_eq!(rig.fake.verbs().last().unwrap(), "user-delete 7.0");
}

/// Asserts that a body that is not native never reaches the helper. It catches a container
/// start handed to the Mac runtime that makes a user for nothing.
#[test]
fn a_container_body_is_refused_before_any_user_is_made() {
    let mut rig = Rig::new("container");
    let mut spec = rig.spec("r1", native("/bin/true", &[]));
    spec.body = Body::Container {
        image: "registry.example/job:1".to_owned(),
        args: Vec::new(),
    };
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(error.0, "this runtime runs native bodies only");
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
}

/// Asserts the boot reset at open: asked for once; a helper that has reset already is fine;
/// any other refusal, and a helper that cannot be reached, are errors that name the reset.
/// It catches a runtime that works against a helper that refuses every verb, and one that
/// fails because the helper outlived an earlier agent.
#[test]
fn opening_asks_for_the_reset_and_takes_one_that_ran_already() {
    let rig = Rig::new("open");
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
    rig.fake.refuse("boot-reset", RESET_ALREADY);
    let again = NativeRuntime::open(rig.fake.client(), config(&rig.fake), TERM + 1);
    assert!(again.is_ok(), "a reset that ran already is fine");
    rig.fake.refuse("boot-reset", "no");
    let refused = NativeRuntime::open(rig.fake.client(), config(&rig.fake), TERM + 2);
    let error = refused.err().unwrap();
    assert!(error.0.starts_with("the boot reset: "), "{}", error.0);
    assert!(error.0.contains("no"));
    let nowhere = Client::new(PathBuf::from("/nonexistent/js-helper"));
    let lost = NativeRuntime::open(nowhere, config(&rig.fake), TERM + 3);
    assert!(lost.err().unwrap().0.starts_with("the boot reset: "));
}

/// Asserts what the runtime answers a native start before anything is made: a program under
/// a tool root passes; one outside every root is `invalid-program`; inputs, outputs and a
/// working directory, which this runtime cannot stage, are each `native-unsupported`. It
/// catches a program run from anywhere on the Mac and a start whose inputs would be dropped
/// without a word.
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
    for body in [with_input, with_output, with_cwd] {
        let refused = rig.rt.check_native(&body).unwrap_err();
        assert_eq!(refused.code, refusal::NATIVE_UNSUPPORTED);
    }
}

/// Asserts that the first program of the command is `sandbox-exec` and that the limiting
/// shell comes after the profile: a shell that ran before the profile would run with the
/// job's environment as the lease user, outside the sandbox. The command line does not
/// depend on the environment, so a job's names cannot move the shell in front of the
/// profile. It catches a command that starts any other program first.
#[test]
fn nothing_but_sandbox_exec_runs_before_the_profile_applies() {
    let mut rig = Rig::new("first-program");
    let mut body = native("/usr/bin/id", &["-u"]);
    body.env = BTreeMap::from([("A".to_owned(), "b".to_owned())]);
    let spec = rig.spec("r1", body);
    rig.rt.create_and_start(&spec).unwrap();
    let argv = rig.fake.run_of("7.0").argv;
    assert_eq!(argv[0], "/usr/bin/sandbox-exec");
    let shells = argv.iter().filter(|word| *word == LIMITER).count();
    assert_eq!(shells, 1, "one limiting shell: {argv:?}");
    let profile_at = argv.iter().position(|word| word == "-p").unwrap();
    let shell_at = argv.iter().position(|word| word == LIMITER).unwrap();
    assert!(profile_at < shell_at, "the profile comes before the shell");
    assert_eq!(parse(&argv).tail, ["/usr/bin/id", "-u"]);

    let hostile = BTreeMap::from([
        ("SHELLOPTS".to_owned(), "xtrace".to_owned()),
        (
            "PS4".to_owned(),
            "$(touch /private/tmp/js-escape)".to_owned(),
        ),
    ]);
    let mut other = native("/usr/bin/id", &["-u"]);
    other.env = hostile;
    let home = rig.fake.home_of("7.0");
    let plain = native("/usr/bin/id", &["-u"]);
    let config = config(&rig.fake);
    assert_eq!(
        js_agent::native::profile_argv(&config, &other, &home).unwrap(),
        js_agent::native::profile_argv(&config, &plain, &home).unwrap(),
        "the command line does not read the environment"
    );
}

/// Asserts that a start refuses, by name, each environment name that steers the limiting
/// shell (`SHELLOPTS`, `BASHOPTS`, `BASH_ENV`, `ENV`, `PS4`, `PROMPT_COMMAND`, `IFS`,
/// `CDPATH`), any name starting `BASH_FUNC_`, and each name the agent owns, before the
/// helper hears of the run: no verb is sent after the reset that opening asked for, and the
/// lease journal is not made. It catches a body that skipped the wire's validation and
/// reaches the shell, which runs inside the profile with the job's environment before
/// `ulimit`.
#[test]
fn a_start_refuses_names_that_steer_the_shell_before_a_user_is_made() {
    use js_types::env::{OWNED_ENV_NAMES, SHELL_ENV_NAMES};

    let mut rig = Rig::new("shell-names");
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
    assert_eq!(rig.fake.verbs(), Vec::<String>::new());
    assert!(
        !rig.dir.path("native-leases").exists(),
        "the journal was touched"
    );
}

/// Asserts that the limits of the configuration reach the shell that starts the run: the
/// file size in blocks of 1024 bytes (at least one) and the process count. It catches a run
/// that starts without the limits an operator set.
#[test]
fn the_configured_limits_are_set_by_the_shell_that_starts_the_run() {
    let mut rig = Rig::with("limits", |config| {
        config.max_file_bytes = 10;
        config.max_processes = 7;
    });
    rig.start("r1", "/bin/true", &[]);
    let parsed = parse(&rig.fake.run_of("7.0").argv);
    assert_eq!(parsed.limits, ("1".to_owned(), "7".to_owned()));
    let mut big = Rig::with("limits-big", |config| config.max_file_bytes = 3 << 20);
    big.start("r1", "/bin/true", &[]);
    assert_eq!(parse(&big.fake.run_of("7.0").argv).limits.0, "3072");
}

/// Asserts that a job environment that names `TMPDIR`, which the agent sets, is refused before
/// anything is made, as it is for a run that pins a bundle. It catches a plain run that takes
/// the job's `TMPDIR` beside the agent's and leaves the choice to the program.
#[test]
fn a_job_that_sets_tmpdir_is_refused_before_a_user_is_made() {
    let mut rig = Rig::new("tmpdir");
    let mut body = native("/usr/bin/id", &[]);
    body.env
        .insert("TMPDIR".to_owned(), "/private/tmp".to_owned());
    let spec = rig.spec("r1", body);
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(why.0.contains("TMPDIR"), "{}", why.0);
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
}
