//! Tests of `__account-cycle`: its command line, its refusal off macOS, and the cycle itself
//! over the fake host and a fake that runs programs as the account, with the output compared
//! whole.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::sync::Mutex;

use super::*;
use crate::mac_accounts::tests::rig::{Rig, me, my_gid};
use crate::mac_accounts::{AccountError, generated_uid};

/// The slots every test command line names; `drive` runs over the rig's own settings.
fn slots() -> Slots {
    Slots::new(700, 100).unwrap()
}

fn words(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn command(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_owned()).collect()
}

fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
    Ran {
        code: Some(code),
        signal: None,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

/// What a command run as the account was asked: the account's name, uid and group, the home,
/// and the program with its arguments.
type Call = (String, u32, u32, PathBuf, Vec<String>);

/// A fake that runs nothing: it answers each command from a script, in order, and does what a
/// job would do to the machine before it answers.
struct Script {
    answers: Mutex<VecDeque<io::Result<Ran>>>,
    calls: Mutex<Vec<Call>>,
    job: Box<dyn Fn() + Send + Sync>,
}

impl Script {
    fn new(answers: Vec<io::Result<Ran>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
            job: Box::new(|| {}),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl Runner for Script {
    fn run(&self, account: &Account, home: &Path, command: &[String]) -> io::Result<Ran> {
        self.calls.lock().unwrap().push((
            account.name.clone(),
            account.uid,
            account.gid,
            home.to_path_buf(),
            command.to_vec(),
        ));
        (self.job)();
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("a command was run that the test did not expect")
    }
}

fn default_answers() -> Vec<io::Result<Ran>> {
    vec![
        Ok(ran(0, "/var/folders/xx/hash/0/\n", "")),
        Ok(ran(0, "", "")),
    ]
}

/// The cycle over `rig`: its status, what it printed, and what it printed to the error stream.
fn cycle(rig: &Rig, args: &Args, runner: &Script) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = drive(
        args,
        rig.settings.clone(),
        rig.fake.clone(),
        runner,
        &mut out,
        &mut err,
    );
    (
        status,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn args_of(rig: &Rig) -> Args {
    Args {
        state: rig.dir.join("state"),
        commands: default_commands(),
        range: slots(),
        lookups: 0,
    }
}

/// Asserts the command line: `--state` is required and takes a directory once; with nothing
/// else the two default commands are run; the one command given replaces them, as every word
/// after `--` or after `--command` (words that look like flags included); and a word that is
/// not understood, a missing value, a command with no program and a word that is not UTF-8
/// are each refused in words.
#[test]
fn the_command_line_takes_a_state_directory_and_optionally_one_command() {
    let args = parse(&words(&["--state", "/s", "--range", "700-799"])).unwrap();
    assert_eq!(
        args,
        Args {
            state: PathBuf::from("/s"),
            commands: vec![
                command(&["/usr/bin/getconf", "DARWIN_USER_DIR"]),
                command(&[
                    "/usr/bin/defaults",
                    "write",
                    "com.example.cycle",
                    "written",
                    "-bool",
                    "yes"
                ]),
            ],
            range: slots(),
            lookups: 0,
        }
    );
    for form in ["--", "--command"] {
        let args = parse(&words(&[
            "--state",
            "/s",
            "--range",
            "700-799",
            form,
            "/bin/echo",
            "--state",
            "-n",
        ]))
        .unwrap();
        assert_eq!(args.state, PathBuf::from("/s"));
        assert_eq!(args.commands, [command(&["/bin/echo", "--state", "-n"])]);
    }
    let refused = |list: &[&str]| parse(&words(list)).unwrap_err();
    assert_eq!(refused(&[]), "--state is required");
    assert_eq!(
        refused(&["--", "/bin/true", "--range", "700-799"]),
        "--state is required"
    );
    assert_eq!(
        refused(&["--range", "700-799", "--", "/bin/true", "--state", "/s"]),
        "--state is required",
        "what comes after the separator is the command's"
    );
    assert_eq!(
        refused(&["--state", "/s", "--", "/bin/true", "--range", "700-799"]),
        "--range is required",
        "what comes after the separator is the command's"
    );
    assert_eq!(refused(&["--state"]), "--state needs a directory");
    assert_eq!(
        refused(&["--state", "/a", "--state", "/b"]),
        "--state is given twice"
    );
    let with = |rest: &[&str]| {
        let mut list = vec!["--state", "/s", "--range", "700-799"];
        list.extend(rest);
        refused(&list)
    };
    assert_eq!(with(&["--"]), "-- needs a program");
    assert_eq!(with(&["--command"]), "--command needs a program");
    assert_eq!(with(&["--verbose"]), "unknown argument \"--verbose\"");
    let bad = vec![OsString::from_vec(vec![0xff, b'x'])];
    let mut list = words(&["--state", "/s", "--range", "700-799", "--"]);
    list.extend(bad);
    assert_eq!(parse(&list).unwrap_err(), "\"\\xFFx\" is not UTF-8");
    let mut list = words(&["--range", "700-799", "--state"]);
    list.push(OsString::from_vec(vec![0xff]));
    assert_eq!(
        parse(&list).unwrap().state,
        PathBuf::from(OsString::from_vec(vec![0xff])),
        "a directory need not be UTF-8"
    );
}

/// Asserts that a wrong command line is refused with status 64, naming what is wrong and the
/// usage, and prints nothing else; and that the agent answers the mode word before anything
/// else.
#[test]
fn a_wrong_command_line_is_a_usage_error() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(&["js-agent", VERB, "--verbose"]), &mut out, &mut err);
    assert_eq!(code, ExitCode::from(status::USAGE));
    assert_eq!(status::USAGE, 64);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!("js-agent: {VERB}: unknown argument \"--verbose\"\n{USAGE}\n")
    );
    assert!(out.is_empty());

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(&["js-agent"]), &mut out, &mut err);
    assert_eq!(code, ExitCode::from(status::USAGE));
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("--state is required")
    );

    let install = || -> io::Result<crate::SystemSleeper> {
        Err(io::Error::other("the mode installs no signal handler"))
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::main_with(&install, words(&["js-agent", VERB]), &mut out, &mut err);
    assert_eq!(code, ExitCode::from(status::USAGE));
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("--state is required")
    );
}

/// Asserts that off macOS a well-formed command line is refused by name with status 2, and
/// prints nothing.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_a_well_formed_command_line_is_refused() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        &words(&[
            "js-agent",
            VERB,
            "--state",
            "/s",
            "--range",
            "700-799",
            "--",
            "/bin/true",
        ]),
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::from(status::REFUSED));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!("js-agent: {VERB}: macOS only\n")
    );
    assert!(out.is_empty());
}

/// Asserts the whole cycle and its output: the grant, the account (lent from the pool, so not
/// made on the spot), its home, each command asked of the runner as the account with its
/// home, what each printed, the slot's folder as the job left it, the retirement with every
/// item of the proof, the pids killed and the times, and the slot's folder emptied and kept;
/// and that the status is 0.
#[test]
fn a_cycle_prints_the_account_the_commands_and_the_folder_before_and_after() {
    let rig = Rig::new("cycle", 3, 1);
    rig.fake.state().cost.insert("delete_account", 5);
    rig.fake.state().cost.insert("create_account", 7);
    let (uid, gid) = (me(), my_gid());
    let mut runner = Script::new(default_answers());
    let hash = rig.dir.join("folders/ab/hash");
    let job = (rig.folder("ab", "hash"), rig.fake.clone());
    runner.job = Box::new(move || {
        std::fs::write(job.0.join("T/scratch"), "x").unwrap();
        if job.1.state().processes.is_empty() {
            job.1.add_process(me(), 77, "sleep");
        }
    });
    let (status, out, err) = cycle(&rig, &args_of(&rig), &runner);
    assert_eq!(err, "");
    assert_eq!(status, status::OK);
    let guid = generated_uid(uid);
    let home = rig.home(1);
    let folder = hash.display();
    assert_eq!(
        out,
        format!(
            "full disk access: granted\n\
             account: name=kbf-acct-1 uid={uid} gid={gid} generated_uid={guid} made_now=false\n\
             home: {home}\n\
             command: /usr/bin/getconf DARWIN_USER_DIR\n\
             \x20 exit: 0\n\
             \x20 stdout: /var/folders/xx/hash/0/\n\
             \x20 stderr: (nothing)\n\
             command: /usr/bin/defaults write com.example.cycle written -bool yes\n\
             \x20 exit: 0\n\
             \x20 stdout: (nothing)\n\
             \x20 stderr: (nothing)\n\
             per-user folder before: {folder}\n\
             \x20 0/\n\
             \x20 C/\n\
             \x20 T/\n\
             \x20 T/scratch\n\
             \x20 X/\n\
             retire: ok\n\
             \x20 killed: [77]\n\
             \x20 proof.record: true\n\
             \x20 proof.processes: true\n\
             \x20 proof.home: true\n\
             \x20 proof.folders: true\n\
             \x20 proof.places: true\n\
             \x20 proof.ipc: true\n\
             \x20 delete_ms: 5\n\
             \x20 refill_ms: 7\n\
             per-user folder after: {folder}\n\
             \x20 0/\n\
             \x20 C/\n\
             \x20 T/\n\
             \x20 X/\n",
            home = home.display(),
        )
    );
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0],
        (
            "kbf-acct-1".to_owned(),
            uid,
            gid,
            home.clone(),
            command(&["/usr/bin/getconf", "DARWIN_USER_DIR"])
        )
    );
    assert_eq!(calls[1].4[0], "/usr/bin/defaults");
    assert!(
        rig.lines()
            .contains(&"lent kbf-acct-1 account-cycle".to_owned()),
        "{:?}",
        rig.lines()
    );
    assert!(rig.lines().contains(&"gone kbf-acct-1".to_owned()));
}

/// Asserts that a command given on the command line is the only one run, that how a command
/// ended is printed whether it exited or a signal ended it, that a multi-line stream is
/// printed indented, and that an account made on the spot (none was ready) says so.
#[test]
fn the_command_given_is_run_and_how_it_ended_is_printed() {
    let rig = Rig::new("cycle-command", 3, 0);
    let args = Args {
        state: rig.dir.join("state"),
        commands: vec![command(&["/bin/sh", "-c", "exit 3"])],
        range: slots(),
        lookups: 0,
    };
    let runner = Script::new(vec![Ok(Ran {
        code: Some(3),
        signal: None,
        stdout: "a\nb\n".to_owned(),
        stderr: "oops\n".to_owned(),
    })]);
    let (status, out, err) = cycle(&rig, &args, &runner);
    assert_eq!((status, err.as_str()), (status::OK, ""));
    assert!(out.contains("made_now=true\n"), "{out}");
    assert!(
        out.contains("command: /bin/sh -c exit 3\n  exit: 3\n  stdout: a\n    b\n  stderr: oops\n"),
        "{out}"
    );
    assert_eq!(runner.calls().len(), 1);

    let rig = rig.next("cycle-signal", 3, 0);
    for (signal, text) in [(Some(9), "  signal: 9\n"), (None, "  signal: 0\n")] {
        let runner = Script::new(vec![Ok(Ran {
            code: None,
            signal,
            stdout: String::new(),
            stderr: String::new(),
        })]);
        let args = Args {
            commands: vec![command(&["/bin/sleep", "100"])],
            ..args_of(&rig)
        };
        let (status, out, _) = cycle(&rig, &args, &runner);
        assert_eq!(status, status::OK);
        assert!(out.contains(text), "{out}");
    }
}

/// Asserts that a command that could not be started is printed and does not stop the cycle:
/// the account is retired all the same, and the status is 1.
#[test]
fn a_command_that_cannot_be_started_is_printed_and_the_account_is_retired() {
    let rig = Rig::new("cycle-not-started", 3, 0);
    let runner = Script::new(vec![
        Err(io::Error::other("no such program")),
        Ok(ran(0, "", "")),
    ]);
    let (status, out, err) = cycle(&rig, &args_of(&rig), &runner);
    assert_eq!((status, err.as_str()), (status::HELD, ""));
    assert!(
        out.contains("command: /usr/bin/getconf DARWIN_USER_DIR\n  not started: no such program\n"),
        "{out}"
    );
    assert!(out.contains("retire: ok\n"), "{out}");
    assert!(rig.lines().contains(&"gone kbf-acct-1".to_owned()));
    assert_eq!(runner.calls().len(), 2);
}

/// Asserts that an account that is held after its retirement is printed with the step and why
/// (and no proof), the slot's folder is printed again, and the status is 1.
#[test]
fn a_held_account_is_printed_and_the_status_is_1() {
    let rig = Rig::new("cycle-held", 3, 0);
    let mut runner = Script::new(default_answers());
    let fake = rig.fake.clone();
    runner.job = Box::new(move || {
        if fake.state().processes.is_empty() {
            fake.add_process(me(), 77, "sleep");
        }
        fake.state().immortal.insert(me());
    });
    let (status, out, err) = cycle(&rig, &args_of(&rig), &runner);
    assert_eq!((status, err.as_str()), (status::HELD, ""));
    let uid = me();
    assert!(
        out.contains(&format!(
            "retire: held: account kbf-acct-1 (uid {uid}) is held at kill: \
             1 processes of uid {uid} remain after 10 rounds: 77 (sleep)\n"
        )),
        "{out}"
    );
    assert!(!out.contains("proof."), "{out}");
    assert!(out.ends_with(&format!("per-user folders after: none of uid {uid}'s\n")));
}

/// Asserts that a pool that refuses to open ends the cycle with status 2 and the reason on the
/// error stream: without the grant the grant is printed as not granted and the error says what
/// to do; a grant that could not be asked about is printed as undetermined; and nothing is
/// made or run.
#[test]
fn a_pool_that_refuses_to_open_ends_the_cycle_with_status_2() {
    let rig = Rig::new("cycle-refused", 3, 1);
    rig.fake.state().grant = Some(io::ErrorKind::PermissionDenied);
    let runner = Script::new(vec![]);
    let (status, out, err) = cycle(&rig, &args_of(&rig), &runner);
    assert_eq!(status, status::REFUSED);
    assert_eq!(out, "full disk access: not-granted\n");
    assert_eq!(
        err,
        format!("js-agent: {VERB}: {}\n", AccountError::NoFullDiskAccess)
    );
    assert!(err.contains("does not hold Full Disk Access"));
    assert!(rig.fake.state().users.is_empty());

    rig.fake.state().grant = Some(io::ErrorKind::NotFound);
    let (status, out, err) = cycle(&rig, &args_of(&rig), &runner);
    assert_eq!(status, status::REFUSED);
    assert_eq!(out, "full disk access: undetermined\n");
    assert!(
        err.contains("asking whether Full Disk Access is held"),
        "{err}"
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a pool that opens but has no uid to lend ends the cycle with status 2 and says
/// why, running nothing.
#[test]
fn a_loan_that_is_refused_ends_the_cycle_with_status_2() {
    let rig = Rig::new("cycle-no-uid", 1, 0);
    rig.fake.add_user("someone", me(), 20, &rig.settings.homes);
    let runner = Script::new(vec![]);
    let (status, out, err) = cycle(&rig, &args_of(&rig), &runner);
    assert_eq!(status, status::REFUSED);
    assert_eq!(out, "full disk access: granted\n");
    assert_eq!(
        err,
        format!(
            "js-agent: {VERB}: no free uid in {uid}-{uid}: {uid}: a user record has it\n",
            uid = me()
        )
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a state directory that is not there is made (root's, closed to group and
/// others, as the pool requires), and that one that cannot be made ends the cycle with
/// status 2, naming it.
#[test]
fn a_missing_state_directory_is_made_and_one_that_cannot_be_is_refused() {
    let rig = Rig::new("cycle-state", 3, 0);
    let state = rig.dir.join("fresh/inner/state");
    let args = Args {
        state: state.clone(),
        commands: vec![command(&["/bin/true"])],
        range: slots(),
        lookups: 0,
    };
    let runner = Script::new(vec![Ok(ran(0, "", ""))]);
    let (status, _, err) = cycle(&rig, &args, &runner);
    assert_eq!((status, err.as_str()), (status::OK, ""));
    let mode = std::fs::metadata(&state).unwrap().permissions().mode();
    assert_eq!(mode & 0o022, 0, "{mode:o}");
    assert!(state.join("accounts").is_file());

    std::fs::write(rig.dir.join("a-file"), "").unwrap();
    for path in ["a-file", "a-file/state"] {
        let args = Args {
            state: rig.dir.join(path),
            commands: vec![command(&["/bin/true"])],
            range: slots(),
            lookups: 0,
        };
        let (status, out, err) = cycle(&rig, &args, &Script::new(vec![]));
        assert_eq!(status, status::REFUSED, "{path}");
        assert_eq!(out, "full disk access: granted\n");
        assert!(
            err.starts_with(&format!(
                "js-agent: {VERB}: making the state directory {}: ",
                rig.dir.join(path).display()
            )),
            "{err}"
        );
    }
}

/// Asserts how the slot's folder is printed: nothing for a uid that has none, an empty folder
/// as such, at most twenty entries and a line saying there are more, directories with a `/`
/// and in order, a link listed and not followed, one that cannot be listed named with why, and
/// folders that cannot be searched named too.
#[test]
fn the_slots_folder_is_listed_with_at_most_twenty_entries() {
    let rig = Rig::new("cycle-listing", 1, 0);
    let print = |rig: &Rig| {
        let mut out = Vec::new();
        print_folders(&mut out, "before", &rig.settings, me());
        String::from_utf8(out).unwrap()
    };
    assert_eq!(
        print(&rig),
        format!("per-user folders before: none of uid {}'s\n", me())
    );

    let hash = rig.dir.join("folders/ab/hash");
    std::fs::create_dir_all(&hash).unwrap();
    assert_eq!(
        print(&rig),
        format!("per-user folder before: {}\n  (empty)\n", hash.display())
    );

    std::fs::create_dir_all(hash.join("b/inner")).unwrap();
    std::fs::write(hash.join("b/inner/file"), "x").unwrap();
    std::fs::write(hash.join("a"), "x").unwrap();
    std::fs::create_dir_all(rig.dir.join("elsewhere")).unwrap();
    std::fs::write(rig.dir.join("elsewhere/precious"), "x").unwrap();
    symlink(rig.dir.join("elsewhere"), hash.join("c-link")).unwrap();
    assert_eq!(
        print(&rig),
        format!(
            "per-user folder before: {}\n  a\n  b/\n  b/inner/\n  b/inner/file\n  c-link\n",
            hash.display()
        )
    );

    for n in 0..LISTED {
        std::fs::write(hash.join(format!("f{n:02}")), "x").unwrap();
    }
    let text = print(&rig);
    let entries: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(entries.len(), LISTED + 1, "{text}");
    assert_eq!(entries[LISTED], "  ... and more");
    assert_eq!(entries[0], "  a");
    assert_eq!(entries[19], "  f14");

    if me() != 0 {
        std::fs::set_permissions(&hash, std::fs::Permissions::from_mode(0o000)).unwrap();
        let text = print(&rig);
        std::fs::set_permissions(&hash, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(text.contains("\n  cannot list: "), "{text}");
    } else {
        eprintln!("skipped: permission bits do not stop root");
    }

    let folders = rig.dir.join("folders");
    std::fs::remove_dir_all(&folders).unwrap();
    std::fs::write(&folders, "").unwrap();
    let text = print(&rig);
    assert!(
        text.starts_with(&format!(
            "per-user folders before: cannot look: {}: ",
            folders.display()
        )),
        "{text}"
    );
}

/// Asserts the listing alone: a root that is not a directory is an error, and a path an entry
/// of which cannot be looked at (a directory that cannot be read, below the first level) is an
/// error too, not a shorter list.
#[test]
fn a_listing_that_cannot_be_read_is_an_error() {
    let rig = Rig::new("cycle-listing-error", 1, 0);
    let file = rig.dir.join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(listing(&file, LISTED).is_err());
    assert_eq!(
        listing(&rig.dir.join("Users"), LISTED).unwrap(),
        (vec![], false)
    );
    if me() != 0 {
        let top = rig.dir.join("Users");
        std::fs::create_dir_all(top.join("closed")).unwrap();
        std::fs::set_permissions(top.join("closed"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let why = listing(&top, LISTED).unwrap_err();
        std::fs::set_permissions(top.join("closed"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(why.kind(), io::ErrorKind::PermissionDenied);
    } else {
        eprintln!("skipped: permission bits do not stop root");
    }
}

/// Asserts that `--lookups <n>` is optional, 0 when not given, and takes a count once; a
/// missing value, one that is not a count and one given twice are refused in words.
#[test]
fn the_lookups_are_optional_and_take_a_count_once() {
    let base = ["--state", "/s", "--range", "700-799"];
    let with = |rest: &[&str]| parse(&words(&[&base[..], rest].concat()));
    assert_eq!(with(&[]).unwrap().lookups, 0);
    assert_eq!(with(&["--lookups", "20"]).unwrap().lookups, 20);
    assert_eq!(
        with(&["--lookups"]),
        Err("--lookups needs a count".to_owned())
    );
    for wrong in ["x", "-1", ""] {
        assert_eq!(
            with(&["--lookups", wrong]),
            Err(format!("--lookups {wrong:?} is not a count")),
            "{wrong}"
        );
    }
    assert_eq!(
        with(&["--lookups", "1", "--lookups", "2"]),
        Err("--lookups is given twice".to_owned())
    );
}

/// Asserts that with `--lookups 2` the cycle prints the uid's processes after the loan (none)
/// and after the commands (the job's), and after the retirement two rounds a pause apart, by
/// the host's clock: the directory no longer has the record, the lookup cache answers for it
/// once more (the proof spent two of its three lookups) and then no longer, and no process of
/// the uid is left.
#[test]
fn the_lookups_after_the_retirement_are_printed_round_by_round() {
    let rig = Rig::new("cycle-lookups", 3, 1);
    rig.fake.state().getpw_lingers = 3;
    let (uid, gid) = (me(), my_gid());
    let mut runner = Script::new(default_answers());
    let fake = rig.fake.clone();
    runner.job = Box::new(move || {
        if fake.state().processes.is_empty() {
            fake.add_process(me(), 77, "sleep");
        }
    });
    let args = Args {
        lookups: 2,
        ..args_of(&rig)
    };
    let (status, out, err) = cycle(&rig, &args, &runner);
    assert_eq!((status, err.as_str()), (status::OK, ""));
    for line in [
        "processes after the loan: []".to_owned(),
        "processes after the commands: [77]".to_owned(),
        format!(
            "lookup 1 at 0 ms: directory name=false uid=false; every lookup \
             name=Some(UserRecord {{ uid: {uid}, gid: {gid} }}) uid=false; processes=[]"
        ),
        "lookup 2 at 100 ms: directory name=false uid=false; every lookup name=None uid=false; \
         processes=[]"
            .to_owned(),
    ] {
        assert!(out.lines().any(|said| said == line), "{line}\n{out}");
    }
    assert_eq!(rig.fake.state().pauses, 1);
}

/// Asserts that `--range <first>-<last>` is required and gives the slots' uids, both
/// included, and that a range given twice, with no value, not two uids, one a range may not
/// take (reversed, below 501, above 60000) or not UTF-8 is a usage error naming what is wrong.
#[test]
fn the_range_is_required_and_is_the_slots_the_pool_takes() {
    let parsed = parse(&words(&["--state", "/s", "--range", "700-709"])).unwrap();
    assert_eq!(parsed.range, Slots::new(700, 10).unwrap());
    assert_eq!(parsed.range.range(), 700..=709);
    assert_eq!(
        parse(&words(&["--state", "/s", "--range", "501-501"]))
            .unwrap()
            .range
            .count(),
        1
    );
    assert_eq!(
        parse(&words(&["--state", "/s"])),
        Err("--range is required".to_owned())
    );
    assert_eq!(
        parse(&words(&[
            "--state", "/s", "--range", "700-709", "--range", "800-809"
        ])),
        Err("--range is given twice".to_owned())
    );
    assert_eq!(
        parse(&words(&["--state", "/s", "--range"])),
        Err("--range needs <first>-<last>".to_owned())
    );
    for (wrong, why) in [
        ("700", "\"700\" is not <first>-<last>, two uids"),
        ("a-b", "\"a-b\" is not <first>-<last>, two uids"),
        ("700-", "\"700-\" is not <first>-<last>, two uids"),
        ("-700", "\"-700\" is not <first>-<last>, two uids"),
        ("709-700", "the first uid, 709, is above the last, 700"),
        (
            "500-700",
            "the first uid, 500, is below 501: macOS keeps the uids up to 500 for the system \
             and the first user",
        ),
        ("700-60001", "the last uid, 60001, is above 60000"),
    ] {
        assert_eq!(
            parse(&words(&["--state", "/s", "--range", wrong])),
            Err(format!("--range {wrong:?}: the account uid range: {why}")),
            "{wrong}"
        );
    }
    let mut list = words(&["--state", "/s", "--range"]);
    list.push(OsString::from_vec(vec![0xff]));
    assert_eq!(
        parse(&list),
        Err("--range \"\\xFF\" is not UTF-8".to_owned())
    );
}

/// Asserts that a command line with no `--range` ends with status 64, naming the flag and the
/// usage, and prints nothing else.
#[test]
fn a_missing_range_is_a_usage_error_naming_it() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        &words(&["js-agent", VERB, "--state", "/s"]),
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::from(status::USAGE));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!("js-agent: {VERB}: --range is required\n{USAGE}\n")
    );
    assert!(out.is_empty());
}
