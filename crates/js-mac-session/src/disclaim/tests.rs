//! The spawner's tests: [`Program`] on the platform the tests run on (the standard library's
//! command off macOS, with the steps and the single use this module adds), and the
//! platform-free rules of `plan` (the vectors, the search `execvp` makes, the attribute
//! calls, the pid conversions), which macOS executes with.

use std::ffi::{OsStr, OsString, c_int, c_short};
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd as _, BorrowedFd, RawFd};
use std::os::unix::ffi::OsStringExt as _;

use super::Program;
use super::plan::{self, AttributeCalls, NUL_REFUSED, Plan, SETEXEC, Strings};

/// What `/bin/sh -c <script>` printed, run through a [`Program`] that `configure` set up.
fn printed(script: &str, configure: impl FnOnce(&mut Program)) -> String {
    let mut program = Program::new("/bin/sh", ["-c", script]);
    configure(&mut program);
    let out = program.output().unwrap();
    assert!(out.status.success(), "{script}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Catches: arguments lost or reordered on the way to the program, or the environment
/// calls not reaching it: a variable set and not seen, the cleared environment still
/// holding the test's own, or one of `envs`' pairs lost.
#[test]
fn the_program_gets_its_arguments_and_the_environment_it_is_given() {
    let out = Program::new("/bin/echo", ["one", "two words"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"one two words\n");
    let seen = printed(
        r#"echo "$A|$B|$C|${CARGO_MANIFEST_DIR-unset}""#,
        |program| {
            program
                .env_clear()
                .env("A", "a")
                .envs([("B", "b"), ("C", "c")]);
        },
    );
    assert_eq!(seen, "a|b|c|unset\n");
    // Without `env_clear`, the program gets this process's environment.
    let inherited = printed(r#"echo "${CARGO_MANIFEST_DIR-unset}""#, |_| {});
    assert_eq!(inherited.trim_end(), env!("CARGO_MANIFEST_DIR"));
}

/// The environment is the calls applied in order: the last value set wins, whether by
/// `env` or `envs`; `env_clear` drops what was set before it; an empty value is a value.
/// Catches an environment rebuilt from the calls out of order, or one that keeps a
/// variable set before `env_clear`.
#[test]
fn the_environment_is_the_calls_applied_in_order() {
    let out = Program::new("/usr/bin/env", [""; 0])
        .env("GONE", "set before the clear")
        .env_clear()
        .env("A", "first")
        .envs([("A", "second"), ("B", "b")])
        .env("B", "last")
        .env("EMPTY", "")
        .output()
        .unwrap();
    let mut lines: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort_unstable();
    assert_eq!(lines, ["A=second", "B=last", "EMPTY="]);
}

/// Catches: the working directory or the process group set on the builder and not on the
/// process, or the ids not passed on (the caller's own are accepted, and the start fails
/// for one no process may take).
#[test]
fn the_program_runs_where_and_as_whom_it_is_told() {
    let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let argv = crate::testing::act_argv("pwd ; pgid");
    let (mut out, out_end) = std::io::pipe().unwrap();
    let mut child = Program::new(&argv[0], &argv[1..])
        .current_dir(&dir)
        .uid(uid)
        .gid(gid)
        .process_group(0)
        .stdout(out_end)
        .spawn()
        .unwrap();
    let pid = child.id();
    assert!(child.wait().unwrap().success());
    let mut said = String::new();
    out.read_to_string(&mut said).unwrap();
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines, [dir.to_str().unwrap(), pid.to_string().as_str()]);
    // No one may take the id that means "no id" (-1): the start fails, so the uid reached
    // it.
    let refused = Program::new("/usr/bin/true", [""; 0]).uid(u32::MAX).spawn();
    assert!(refused.is_err(), "the uid did not reach the start");
}

/// Catches: a standard stream set on the builder and not given to the process.
#[test]
fn the_program_gets_the_streams_it_is_given() {
    let (mut out, out_end) = std::io::pipe().unwrap();
    let (mut err, err_end) = std::io::pipe().unwrap();
    let (in_end, mut feed) = std::io::pipe().unwrap();
    let mut child = Program::new(
        "/bin/sh",
        ["-c", "read line; echo \"out $line\"; echo err >&2"],
    )
    .stdin(in_end)
    .stdout(out_end)
    .stderr(err_end)
    .spawn()
    .unwrap();
    std::io::Write::write_all(&mut feed, b"fed\n").unwrap();
    drop(feed);
    assert!(child.wait().unwrap().success());
    let (mut said, mut warned) = (String::new(), String::new());
    out.read_to_string(&mut said).unwrap();
    err.read_to_string(&mut warned).unwrap();
    assert_eq!((said.as_str(), warned.as_str()), ("out fed\n", "err\n"));
}

/// A step that writes `step` and a newline to the descriptor `fd`: async-signal-safe, and
/// allocates nothing.
fn marker(fd: RawFd) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || {
        // SAFETY: `fd` is open for the call: the test's pipe in the test process, the
        // child's standard output in the child.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        rustix::io::write(fd, b"step\n")?;
        Ok(())
    }
}

/// A step that fails with `EPERM`.
fn refuse() -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::EPERM))
}

/// A step runs in the child, before the program and in its streams: the program's output
/// follows the step's. A step that fails ends the start with its own error and the program
/// does not run. The scratch path holds a space and reaches the shell as an argument, not
/// as shell text. Catches: steps that run after the program or not at all, a failing step
/// that lets the program run, or its cause lost.
#[test]
fn steps_run_before_the_program_and_a_failing_one_stops_it() {
    let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "disclaim steps");
    let marker_file = dir.join("ran here");
    let script = r#"echo program; echo ran > "$1""#;
    let file = marker_file.as_os_str();
    // The steps, run here too: a step run in the child leaves no coverage record.
    let (mut check, check_end) = std::io::pipe().unwrap();
    marker(check_end.as_raw_fd())().unwrap();
    let mut wrote = [0; 5];
    check.read_exact(&mut wrote).unwrap();
    assert_eq!(&wrote, b"step\n");
    assert_eq!(refuse().unwrap_err().raw_os_error(), Some(libc::EPERM));
    let args = [OsStr::new("-c"), OsStr::new(script), OsStr::new("sh"), file];
    // SAFETY: the step writes to a descriptor and allocates nothing.
    let mut started = unsafe { Program::with_steps("/bin/sh", args, marker(1)) };
    let out = started.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"step\nprogram\n");
    assert_eq!(std::fs::read(&marker_file).unwrap(), b"ran\n");
    std::fs::remove_file(&marker_file).unwrap();
    // SAFETY: the step allocates nothing and calls nothing.
    let mut refused = unsafe { Program::with_steps("/bin/sh", args, refuse) };
    let error = refused.spawn().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    assert!(
        !marker_file.exists(),
        "the program ran after a failing step"
    );
    // A failed attempt uses the program up.
    assert_eq!(
        refused.output().unwrap_err().to_string(),
        "a Program starts its program once"
    );
}

/// A NUL anywhere, in a program named with or without a directory, an argument, or a
/// variable's name or value, refuses the start, without echoing what held it; and the
/// refused attempt uses the program up. Catches: a NUL cut short silently (a different
/// program, argument or value would reach the process), a bare name looked up before it is
/// checked, or a secret in the message.
#[test]
fn a_nul_anywhere_is_refused_at_the_start() {
    let makers: [fn() -> Program; 5] = [
        || Program::new("/bin/echo", ["token=s3cret\0tail"]),
        || Program::new("/bin/e\0cho", [""; 0]),
        || Program::new("bad\0name", [""; 0]),
        || {
            let mut program = Program::new("/bin/echo", [""; 0]);
            program.env("TOKEN", "s3cret\0tail");
            program
        },
        || {
            let mut program = Program::new("/bin/echo", [""; 0]);
            program.env("s3cret\0NAME", "x");
            program
        },
    ];
    for make in makers {
        let mut program = make();
        let error = program.spawn().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!error.to_string().contains("s3cret"), "{error}");
        assert_eq!(
            program.spawn().unwrap_err().to_string(),
            "a Program starts its program once"
        );
        assert_eq!(
            make().output().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

/// Catches: a program started twice from one [`Program`] (on macOS the second start would
/// reuse what the first made), or a second start that is not refused the same way by
/// `spawn` and `output`.
#[test]
fn a_program_starts_once() {
    let mut program = Program::new("/usr/bin/true", [""; 0]);
    assert!(program.output().unwrap().status.success());
    for again in [program.spawn().map(drop), program.output().map(drop)] {
        assert_eq!(
            again.unwrap_err().to_string(),
            "a Program starts its program once"
        );
    }
    let shown = format!("{program:?}");
    assert!(shown.starts_with("Program {"), "{shown}");
    assert!(shown.contains("started: true"), "{shown}");
}

/// Catches: a missing program reported as anything but not found.
#[test]
fn a_missing_program_is_not_found() {
    let error = Program::new("/no/such/program", [""; 0])
        .spawn()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

/// An argument that is not UTF-8 reaches the program byte for byte. Catches a conversion
/// to text on the way.
#[test]
fn an_argument_that_is_not_text_reaches_the_program_as_it_is() {
    let arg = OsString::from_vec(vec![0xff, b'a', 0xfe]);
    let out = Program::new("/bin/echo", [&arg]).output().unwrap();
    assert_eq!(out.stdout, [0xff, b'a', 0xfe, b'\n']);
}

/// Starts made at the same moment from several threads keep their own arguments,
/// environments and streams, and one that fails leaves the others alone. Catches state of
/// one start reaching another.
#[test]
fn starts_at_the_same_moment_keep_their_own_state() {
    let barrier = std::sync::Barrier::new(3);
    let run = |name: &str| {
        let mut program = Program::new("/bin/sh", ["-c", "echo \"$1 $WHO\"", "sh", name]);
        program.env_clear().env("WHO", format!("env-{name}"));
        barrier.wait();
        program.output().unwrap().stdout
    };
    let (a, b, missing) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run("a"));
        let b = scope.spawn(|| run("b"));
        let missing = scope.spawn(|| {
            let mut program = Program::new("/no/such/program", ["x"]);
            barrier.wait();
            program.spawn().map(drop)
        });
        (
            a.join().unwrap(),
            b.join().unwrap(),
            missing.join().unwrap(),
        )
    });
    assert_eq!(a, b"a env-a\n");
    assert_eq!(b, b"b env-b\n");
    assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
}

/// `vars` as the environment `plan` takes.
fn env(vars: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    vars.iter()
        .map(|&(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// The vectors are what `posix_spawn` takes, byte for byte: the argument vector is the
/// program as given, then each argument (an empty one and one that is not UTF-8
/// included); the environment vector is `NAME=value` (a value holding `=`, an empty
/// value, bytes that are not UTF-8); each ends with a null pointer; the pointers stay
/// valid once the plan has moved. Catches a vector that is not null-ended, an argument or
/// variable lost, reordered or converted, or pointers that dangle in the child.
#[test]
fn the_vectors_are_what_posix_spawn_takes() {
    let mut args = os(&["-c", "echo a b", ""]);
    args.push(OsString::from_vec(vec![0xff, b'x']));
    let mut vars = env(&[("A", "b=c"), ("EMPTY", "")]);
    vars.push((OsString::from("RAW"), OsString::from_vec(vec![0xfe, 0xfd])));
    let plan = Box::new(Plan::new(OsStr::new("/bin/sh"), &args, &vars).unwrap());
    let mut seen = Vec::new();
    let code = plan.run(&mut |file: &Strings, argv: &Strings| {
        // SAFETY: the file is a NUL-terminated string the plan owns.
        let first = unsafe { std::ffi::CStr::from_ptr(file.first()) };
        seen.push((first.to_bytes().to_vec(), argv.bytes()));
        libc::E2BIG
    });
    assert_eq!(code, libc::E2BIG);
    let argv: Vec<Vec<u8>> = vec![
        b"/bin/sh".to_vec(),
        b"-c".to_vec(),
        b"echo a b".to_vec(),
        Vec::new(),
        vec![0xff, b'x'],
    ];
    assert_eq!(seen, [(b"/bin/sh".to_vec(), argv)]);
    assert_eq!(
        plan.envp().bytes(),
        [
            b"A=b=c".to_vec(),
            b"EMPTY=".to_vec(),
            b"RAW=\xfe\xfd".to_vec()
        ]
    );
    let none = Plan::new(OsStr::new("/usr/bin/true"), &[], &[]).unwrap();
    assert!(none.envp().bytes().is_empty());
}

/// A NUL in the program (named with or without a directory), an argument, or a variable's
/// name or value is refused with the one message, which names none of them. Catches a NUL
/// cut short silently, or a secret echoed in the refusal.
#[test]
fn a_nul_anywhere_in_a_plan_is_refused_without_naming_it() {
    let secret = OsString::from("token=s3cret\0tail");
    let refusals = [
        Plan::new(OsStr::new("/bin/e\0cho"), &[], &[]),
        Plan::new(OsStr::new("bad\0name"), &[], &[]),
        Plan::new(OsStr::new("/bin/echo"), std::slice::from_ref(&secret), &[]),
        Plan::new(
            OsStr::new("echo"),
            &[],
            &[(OsString::from("TOKEN"), secret.clone())],
        ),
        Plan::new(OsStr::new("echo"), &[], &[(secret, OsString::from("x"))]),
    ];
    for refused in refusals {
        let why = refused.err().expect("a NUL was accepted");
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(why.to_string(), NUL_REFUSED);
    }
}

/// The files the plan tries, and the argument vectors it tries them with, given the error
/// number each attempt returns in turn (`EIO` once `codes` runs out); with the answer.
fn tried(plan: &Plan, codes: &[c_int]) -> (c_int, Vec<(String, Vec<String>)>) {
    let mut seen = Vec::new();
    let mut codes = codes.iter().copied();
    let code = plan.run(&mut |file: &Strings, argv: &Strings| {
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
        let file = text(file.bytes().remove(0));
        let argv = argv.bytes().into_iter().map(text).collect();
        seen.push((file, argv));
        codes.next().unwrap_or(libc::EIO)
    });
    (code, seen)
}

fn attempt(file: &str, argv: &[&str]) -> (String, Vec<String>) {
    (
        file.to_owned(),
        argv.iter().map(|&arg| arg.to_owned()).collect(),
    )
}

/// A bare name is searched for in the environment's `PATH`, in order, relative and empty
/// directories included (an empty one is the working directory, `./`); with no `PATH`, in
/// `/usr/bin:/bin`. Nothing is resolved before the attempts, so a relative directory is
/// the child's. Catches: the parent's `PATH` used, a directory skipped or reordered, an
/// empty directory dropped, or no default.
#[test]
fn a_bare_name_is_searched_for_in_the_jobs_path() {
    let vars = env(&[("PATH", "bin::/usr/bin")]);
    let plan = Plan::new(OsStr::new("prog"), &os(&["a"]), &vars).unwrap();
    let missing = [libc::ENOENT, libc::ENOTDIR, libc::ENOENT];
    assert_eq!(
        tried(&plan, &missing),
        (
            libc::ENOENT,
            vec![
                attempt("bin/prog", &["prog", "a"]),
                attempt("./prog", &["prog", "a"]),
                attempt("/usr/bin/prog", &["prog", "a"]),
            ]
        )
    );
    let plain = Plan::new(OsStr::new("prog"), &[], &[]).unwrap();
    let (code, seen) = tried(&plain, &[libc::ENOENT, libc::ENOENT]);
    assert_eq!(code, libc::ENOENT);
    let files: Vec<&str> = seen.iter().map(|(file, _)| file.as_str()).collect();
    assert_eq!(files, ["/usr/bin/prog", "/bin/prog"]);
}

/// The search passes over a file it may not execute (`EACCES`) or cannot reach (`ELOOP`,
/// `ENAMETOOLONG`) to a later one that runs; when every file was passed over, the answer
/// is `EACCES` if one was refused so and `ENOENT` otherwise; any other error ends the
/// search with it. Catches: a non-executable first file that hides a good later one, a
/// refusal reported as not found, or an error the search should stop at passed over.
#[test]
fn the_search_passes_over_files_it_may_not_execute() {
    let vars = env(&[("PATH", "/one:/two:/three")]);
    let plan = Plan::new(OsStr::new("prog"), &[], &vars).unwrap();
    let (code, seen) = tried(&plan, &[libc::EACCES, 0]);
    assert_eq!((code, seen.len()), (0, 2), "a later file runs");
    let (code, seen) = tried(&plan, &[libc::EACCES, libc::ELOOP, libc::ENAMETOOLONG]);
    assert_eq!(
        (code, seen.len()),
        (libc::EACCES, 3),
        "all refused or unreachable"
    );
    let (code, seen) = tried(&plan, &[libc::ENOENT, libc::ENOENT, libc::ENOENT]);
    assert_eq!((code, seen.len()), (libc::ENOENT, 3), "none there");
    let (code, seen) = tried(&plan, &[libc::ENOENT, libc::E2BIG]);
    assert_eq!(
        (code, seen.len()),
        (libc::E2BIG, 2),
        "another error ends it"
    );
}

/// A file the kernel does not recognise (`ENOEXEC`, a script with no `#!` line) is run by
/// `/bin/sh` as `sh <file> <arguments>`, for a searched file and for one named with a
/// directory, and that answer is final. A program named with a directory is the one file
/// tried, and its error is the answer as it is. Catches: a script without `#!` refused,
/// the shell given the wrong file or arguments, or an explicit path searched or its error
/// changed.
#[test]
fn a_file_the_kernel_does_not_recognise_is_run_by_the_shell() {
    let vars = env(&[("PATH", "/one:/two")]);
    let searched = Plan::new(OsStr::new("script"), &os(&["x", "y"]), &vars).unwrap();
    assert_eq!(
        tried(&searched, &[libc::ENOENT, libc::ENOEXEC, libc::ENOENT]),
        (
            libc::ENOENT,
            vec![
                attempt("/one/script", &["script", "x", "y"]),
                attempt("/two/script", &["script", "x", "y"]),
                attempt("/bin/sh", &["sh", "/two/script", "x", "y"]),
            ]
        )
    );
    let explicit = Plan::new(OsStr::new("/x/script"), &os(&["z"]), &vars).unwrap();
    assert_eq!(
        tried(&explicit, &[libc::ENOEXEC, 0]),
        (
            0,
            vec![
                attempt("/x/script", &["/x/script", "z"]),
                attempt("/bin/sh", &["sh", "/x/script", "z"]),
            ]
        )
    );
    assert_eq!(
        tried(&explicit, &[libc::ENOTDIR]),
        (
            libc::ENOTDIR,
            vec![attempt("/x/script", &["/x/script", "z"])]
        )
    );
}

/// The attribute calls the macOS start makes, recorded, failing at `fail` if given.
#[derive(Default)]
struct Calls {
    asked: Vec<String>,
    fail: Option<&'static str>,
}

impl Calls {
    fn ask(&mut self, what: String, name: &str) -> c_int {
        self.asked.push(what);
        match self.fail == Some(name) {
            true => libc::EINVAL,
            false => 0,
        }
    }
}

impl AttributeCalls for Calls {
    fn init(&mut self) -> c_int {
        self.ask("init".to_owned(), "init")
    }

    fn set_flags(&mut self, flags: c_short) -> c_int {
        self.ask(format!("flags {flags:#x}"), "flags")
    }

    fn set_disclaim(&mut self, disclaim: c_int) -> c_int {
        self.ask(format!("disclaim {disclaim}"), "disclaim")
    }

    fn destroy(&mut self) {
        self.asked.push("destroy".to_owned());
    }
}

/// The attributes are made as init, then the flag that executes in place
/// (`POSIX_SPAWN_SETEXEC`, 0x40), then the disclaim (1); a call that fails ends it with its
/// error and destroys what init made, so no start goes on without the disclaim. Catches:
/// the disclaim not asked for or asked off, the wrong flags, an error swallowed (the start
/// would go on undisclaimed), or attributes leaked or destroyed when never made.
#[test]
fn the_attributes_disclaim_or_the_start_fails() {
    assert_eq!(SETEXEC, 0x40);
    let all = ["init", "flags 0x40", "disclaim 1"];
    for (fail, asked) in [
        (None, &all[..]),
        (Some("init"), &["init"][..]),
        (Some("flags"), &["init", "flags 0x40", "destroy"][..]),
        (
            Some("disclaim"),
            &["init", "flags 0x40", "disclaim 1", "destroy"][..],
        ),
    ] {
        let mut calls = Calls {
            fail,
            ..Calls::default()
        };
        let made = plan::attributes(&mut calls);
        assert_eq!(calls.asked, asked, "{fail:?}");
        assert_eq!(
            made.err().map(|why| why.raw_os_error()),
            fail.map(|_| Some(libc::EINVAL)),
            "{fail:?}"
        );
    }
}

/// A pid above the system's largest is refused before any query, and a failed query is its
/// error, not a pid. Catches an out-of-range pid wrapped to another process's, or a failed
/// query read as an answer.
#[test]
fn a_pid_query_refuses_what_is_not_a_pid() {
    let error = plan::pid_of(u32::MAX).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "4294967295 is not a pid");
    assert_eq!(plan::pid_of(42).unwrap(), 42);
    let failed = plan::answered(-1, io::Error::from_raw_os_error(libc::ESRCH));
    assert_eq!(failed.unwrap_err().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(
        plan::answered(7, io::Error::from_raw_os_error(libc::ESRCH)).unwrap(),
        7
    );
    // A failure that set no error number is named, not "Success".
    let unset = plan::answered(-1, io::Error::from_raw_os_error(0)).unwrap_err();
    assert_eq!(unset.to_string(), plan::NO_ANSWER);
}

/// A file found through a relative directory is handed to the shell as it was tried, so
/// the shell opens the same file, in the directory the child is in. Catches the shell given
/// a different spelling of the file than the attempt that found it.
#[test]
fn the_shell_is_given_the_file_as_it_was_tried() {
    let vars = env(&[("PATH", "rel")]);
    let plan = Plan::new(OsStr::new("s"), &[], &vars).unwrap();
    let (_, seen) = tried(&plan, &[libc::ENOEXEC]);
    assert_eq!(
        seen,
        [
            attempt("rel/s", &["s"]),
            attempt("/bin/sh", &["sh", "rel/s"])
        ]
    );
}

/// A program and `PATH` directories that are not UTF-8 reach the attempts byte for byte,
/// for a searched name and for a path, in the files and in the shell's vector. Catches a
/// conversion to text of a path on its way to `posix_spawn`.
#[test]
fn paths_that_are_not_text_are_tried_as_they_are() {
    let raw = |bytes: &[u8]| OsString::from_vec(bytes.to_vec());
    let vars = vec![(OsString::from("PATH"), raw(b"/d\xff:rel\xfe"))];
    let mut seen: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    let mut codes = [libc::ENOENT, libc::ENOEXEC, libc::ENOENT].into_iter();
    let searched = Plan::new(&raw(b"pr\xffg"), &[raw(b"a\xfe")], &vars).unwrap();
    searched.run(&mut |file: &Strings, argv: &Strings| {
        seen.push((file.bytes().remove(0), argv.bytes()));
        codes.next().unwrap_or(libc::EIO)
    });
    let argv = vec![b"pr\xffg".to_vec(), b"a\xfe".to_vec()];
    let shell = vec![
        b"sh".to_vec(),
        b"rel\xfe/pr\xffg".to_vec(),
        b"a\xfe".to_vec(),
    ];
    assert_eq!(
        seen,
        [
            (b"/d\xff/pr\xffg".to_vec(), argv.clone()),
            (b"rel\xfe/pr\xffg".to_vec(), argv),
            (b"/bin/sh".to_vec(), shell),
        ]
    );
    let explicit = Plan::new(&raw(b"/x\xff/p"), &[], &vars).unwrap();
    let mut files = Vec::new();
    explicit.run(&mut |file: &Strings, _: &Strings| {
        files.push(file.bytes().remove(0));
        libc::ENOENT
    });
    assert_eq!(files, [b"/x\xff/p".to_vec()]);
}

/// An empty program is not found, at once: no directory of the `PATH` is tried (a
/// directory joined to an empty name is the directory, which would answer `EACCES`).
/// Catches an empty name searched for, or answered with anything but not found.
#[test]
fn an_empty_program_is_not_found_without_a_search() {
    let plan = Plan::new(
        OsStr::new(""),
        &os(&["a"]),
        &env(&[("PATH", "/bin:/usr/bin")]),
    )
    .unwrap();
    assert_eq!(tried(&plan, &[libc::EACCES]), (libc::ENOENT, Vec::new()));
    let error = Program::new("", [""; 0]).spawn().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}
