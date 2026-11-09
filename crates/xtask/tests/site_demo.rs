//! `cargo xtask site-demo` against `xtask-fake` as the server, the client and cargo (a server that
//! checks the files it is given, binds ports of the system's choosing and says where, and answers
//! `200 ok`; a client that refuses a job for want of a host and lists none). The verb is run in this process, so each failure is
//! matched as the variant of `Error` it is, and once through the `xtask` binary for its flags and the text
//! it prints. A fake fails as a test asks by a file named `defect` next to it. Nothing here waits on a
//! clock: the server says when it listens, and the verb reads that.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use xtask::site_demo::{Error, SiteDemo, run};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");

/// One test's directories: the root cargo runs in, the directory of the fake programs, the directory the
/// verb makes its scratch directory in (to show it is left empty), and the transcript's place.
struct Setup {
    root: PathBuf,
    bins: PathBuf,
    fakes: PathBuf,
    cargo: PathBuf,
    scratch: PathBuf,
    out: PathBuf,
    /// Removes everything when the test ends.
    _dir: js_testkit::dir::TestDir,
}

impl Setup {
    /// Fakes for `js-lease-server`, `js` and `cargo`, and an empty `cargo-messages` for the fake cargo.
    fn new(name: &str) -> Self {
        let dir = js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let root = dir.path().to_path_buf();
        let bins = root.join("target").join("release");
        let fakes = root.join("fakes");
        let scratch = root.join("tmp");
        for dir in [&bins, &fakes, &scratch] {
            std::fs::create_dir_all(dir).expect("scratch");
        }
        for (dir, program) in [(&bins, "js-lease-server"), (&bins, "js"), (&fakes, "cargo")] {
            std::fs::hard_link(FAKE, dir.join(program)).expect("link the fake");
        }
        std::fs::write(root.join("cargo-messages"), "").expect("messages");
        let out = root.join("out").join("demo.transcript");
        Self {
            root,
            bins,
            cargo: fakes.join("cargo"),
            fakes,
            scratch,
            out,
            _dir: dir,
        }
    }

    /// Asks the fakes to fail as `defect` says.
    fn defect(&self, defect: &str) {
        for dir in [&self.bins, &self.fakes] {
            std::fs::write(dir.join("defect"), defect).expect("defect");
        }
    }

    /// The verb against the fake programs in `bins`, on a port of the server's choosing.
    fn demo(&self) -> SiteDemo<'_> {
        SiteDemo {
            cargo: self.cargo.as_os_str(),
            root: &self.root,
            bin_dir: Some(&self.bins),
            out: &self.out,
            scratch: &self.scratch,
            port: 0,
            job_port: 0,
            ready_timeout: Duration::from_secs(30),
        }
    }

    /// Whether the verb left anything in its scratch directory.
    fn scratch_left(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.scratch)
            .expect("tmp")
            .map(|e| e.expect("entry").path())
            .collect()
    }
}

/// The committed recording, read.
fn committed() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/test/demo.transcript"),
    )
    .expect("site/test/demo.transcript")
}

/// Catches a transcript in another shape than the page shows: the pool file, the four commands of the
/// verb with the ports the server really bound (it was asked for port 0 twice), and what the client printed
/// after each, in order (the refusal of the job, and nothing for the list); with the scratch directory (and
/// the server) gone after.
#[test]
fn the_recording_is_the_pool_file_then_each_command_with_what_it_printed() {
    let s = Setup::new("records");
    let said = run(&s.demo()).expect("recorded");
    assert_eq!(said, format!("recorded {}", s.out.display()));
    let text = std::fs::read_to_string(&s.out).expect("transcript");
    let port_after = |flag: &str| -> u16 {
        text.split(flag)
            .nth(1)
            .and_then(|rest| rest.split([' ', '\n']).next())
            .expect("the port")
            .parse()
            .expect("a number")
    };
    let port = port_after("--listen 127.0.0.1:");
    let job_port = port_after("--job-listen 127.0.0.1:");
    assert_ne!(port, 0);
    assert_ne!(job_port, 0);
    assert_ne!(port, job_port);
    let committed = committed();
    let pool = committed.split("$ ").next().expect("the pool block");
    let base = format!("js --server http://127.0.0.1:{job_port} --token-file client-token job");
    let image = "registry.example/train@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let expected = format!(
        "{pool}\
$ printf 'a-local-token-of-at-least-32-bytes\\n' > client-token && chmod 600 client-token\n\
$ js-lease-server --state-file state --boot initialise --token-file client-token --agent-pool-config pool.toml --job-listen 127.0.0.1:{job_port} --listen 127.0.0.1:{port} &\n\
$ {base} submit --id demo-1 --spec.image {image} --spec.needs.gpus 1 --spec.needs.gpu-class nvidia-a100 --spec.wall-limit-seconds 1h\n\
js: refused: no-capable-host: no host and no catalog could ever serve the job\n\
$ {base} list\n"
    );
    assert_eq!(text, expected);
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a build that is not the one the script ran: cargo is asked for the release build of the two
/// packages in the root, and the programs it leaves in `target/release` are the ones run.
#[test]
fn without_a_bin_dir_it_builds_the_two_packages_in_the_root_and_runs_them() {
    let s = Setup::new("builds");
    let mut demo = s.demo();
    demo.bin_dir = None;
    run(&demo).expect("recorded");
    let args = std::fs::read_to_string(s.root.join("target/debug/cargo-args")).expect("cargo ran");
    assert_eq!(args, "build --release -p js-lease-server -p js-cli\n");
    assert!(s.out.is_file());
}

/// Catches a build run when `--bin-dir` says not to.
#[test]
fn with_a_bin_dir_it_does_not_build() {
    let s = Setup::new("no-build");
    run(&s.demo()).expect("recorded");
    assert!(!s.root.join("target/debug/cargo-args").exists());
}

/// Catches a failed build that goes on to record (the check can fail): the verb fails with the build's
/// status and the command, and writes no transcript.
#[test]
fn a_failed_build_fails_the_verb_and_writes_nothing() {
    let s = Setup::new("build-fails");
    s.defect("build-fails");
    let mut demo = s.demo();
    demo.bin_dir = None;
    match run(&demo) {
        Err(Error::BuildFailed { command, status }) => {
            assert!(
                command.ends_with("cargo build --release -p js-lease-server -p js-cli"),
                "{command}"
            );
            assert_eq!(status.code(), Some(101));
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.out.exists());
}

/// Catches a cargo that cannot be started reported as anything else: the program is named and the I/O
/// error is kept.
#[test]
fn a_cargo_that_cannot_start_is_named() {
    let s = Setup::new("no-cargo");
    let missing = s.root.join("no-such-cargo");
    let mut demo = s.demo();
    demo.bin_dir = None;
    demo.cargo = missing.as_os_str();
    match run(&demo) {
        Err(Error::Spawn { program, source }) => {
            assert_eq!(program, missing.to_string_lossy());
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches a server that dies being waited for until the deadline (the check can fail): the verb fails at
/// once with the server's status and its own words, and leaves nothing in the scratch directory.
#[test]
fn a_server_that_exits_fails_the_verb_with_its_log_and_leaves_no_scratch() {
    let s = Setup::new("server-dies");
    s.defect("server-dies");
    match run(&s.demo()) {
        Err(Error::ServerExited { status, log }) => {
            assert_eq!(status.code(), Some(3));
            assert!(log.contains("asked to die"), "{log}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.out.exists());
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a server started without the files and flags the recording shows (the check can fail): the fake
/// server refuses a start without the pool file, an owner-only token and `--job-listen`, so a token file
/// written 0644 fails here.
/// The fake is the real server's checks in miniature; this shows the verb's files pass them.
#[test]
fn the_server_is_given_a_pool_file_an_owner_only_token_file_and_a_job_address() {
    let s = Setup::new("files");
    run(&s.demo()).expect("recorded");
    // The fake server exits 5 when a file or flag is missing or the token too open: run it by hand in a
    // bare directory, each time lacking one thing.
    let bare = s.root.join("bare");
    std::fs::create_dir_all(&bare).expect("bare");
    let token = bare.join("client-token");
    std::fs::write(&token, "x").expect("token");
    let mode = |m| {
        std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(m))
            .expect("mode");
    };
    let start = |extra: &[&str]| {
        Command::new(s.bins.join("js-lease-server"))
            .args(["--token-file", "client-token"])
            .args(extra)
            .current_dir(&bare)
            .output()
            .expect("run the fake")
            .status
            .code()
    };
    // A token anyone can read, no pool file, no job address.
    mode(0o644);
    assert_eq!(start(&[]), Some(5));
    // An owner-only token and the job address, but no pool file.
    mode(0o600);
    assert_eq!(start(&["--job-listen", "127.0.0.1:0"]), Some(5));
    // The pool file, but no job address.
    std::fs::write(bare.join("pool.toml"), "").expect("pool");
    assert_eq!(start(&[]), Some(5));
    // The pool file and the job address, but a token anyone can read.
    mode(0o644);
    assert_eq!(start(&["--job-listen", "127.0.0.1:0"]), Some(5));
}

/// Catches a client failure swallowed: the verb fails naming the command that failed, its status and what
/// it said, and writes no transcript.
#[test]
fn a_failing_client_command_fails_the_verb_naming_it() {
    let s = Setup::new("js-fails");
    s.defect("js-fails");
    match run(&s.demo()) {
        Err(Error::Client {
            command,
            status,
            output,
        }) => {
            assert!(
                command.starts_with("js --server http://127.0.0.1:")
                    && command.contains(" job submit --id demo-1 --spec.image "),
                "{command}"
            );
            assert_eq!(status.code(), Some(4));
            assert_eq!(output, "js: the fake client was asked to fail");
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.out.exists());
}

/// Catches a submit that is accepted where the recording shows the refusal (the check can fail): the
/// verb fails naming the command and keeping what it printed, and writes no transcript.
#[test]
fn a_submit_that_is_not_refused_fails_the_verb() {
    let s = Setup::new("accepts");
    s.defect("js-accepts");
    match run(&s.demo()) {
        Err(Error::NotRefused { command, output }) => {
            assert!(command.contains(" job submit --id demo-1 "), "{command}");
            assert_eq!(output, "job.id: demo-1");
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.out.exists());
}

/// Catches a failing list swallowed: a `js job list` that fails fails the verb, naming it, its status and
/// what it said.
#[test]
fn a_failing_list_fails_the_verb_naming_it() {
    let s = Setup::new("list-fails");
    s.defect("js-list-fails");
    match run(&s.demo()) {
        Err(Error::Client {
            command,
            status,
            output,
        }) => {
            assert!(command.ends_with(" job list"), "{command}");
            assert_eq!(status.code(), Some(4));
            assert_eq!(output, "js: the fake client was asked to fail the list");
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.out.exists());
}

/// Catches a server that never says where the job service listens being recorded against a guessed port:
/// the verb fails with the server's log, and leaves no scratch and no transcript.
#[test]
fn a_server_that_does_not_say_where_the_job_service_listens_fails_the_verb() {
    let s = Setup::new("no-job-address");
    s.defect("server-no-job-address");
    match run(&s.demo()) {
        Err(Error::NoJobAddress { log }) => {
            assert!(log.starts_with("listening on 127.0.0.1:"), "{log}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.out.exists());
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a missing program reported as a panic or silence: a bin directory without the server, or without
/// the client, fails naming the program and keeping the I/O error, and leaves no scratch.
#[test]
fn a_missing_program_is_named() {
    let s = Setup::new("missing");
    std::fs::remove_file(s.bins.join("js-lease-server")).expect("remove");
    match run(&s.demo()) {
        Err(Error::Spawn { program, source }) => {
            assert_eq!(
                program,
                s.bins.join("js-lease-server").display().to_string()
            );
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }

    let s = Setup::new("missing-js");
    std::fs::remove_file(s.bins.join("js")).expect("remove");
    match run(&s.demo()) {
        Err(Error::Spawn { program, source }) => {
            assert_eq!(program, s.bins.join("js").display().to_string());
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a bin directory that cannot be resolved reported as a start failure: an empty path is a
/// `ResolveBinDir` error naming it.
#[test]
fn a_bin_dir_that_cannot_be_made_absolute_is_named() {
    let s = Setup::new("bin-dir");
    let mut demo = s.demo();
    demo.bin_dir = Some(Path::new(""));
    match run(&demo) {
        Err(Error::ResolveBinDir { path, source }) => {
            assert_eq!(path, PathBuf::new());
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches a scratch place that cannot be made: a scratch parent that is a file is a `Scratch` error
/// naming it.
#[test]
fn a_scratch_directory_that_cannot_be_made_is_named() {
    let s = Setup::new("scratch");
    let file = s.root.join("a-file");
    std::fs::write(&file, "").expect("file");
    let mut demo = s.demo();
    demo.scratch = &file;
    match run(&demo) {
        Err(Error::Scratch { path, .. }) => assert_eq!(path, file),
        other => panic!("{other:?}"),
    }
}

/// Catches a transcript place that is refused reported as a panic: a path under a file is a `CreateDir`
/// error and a path that is a directory is a `WriteTranscript` error, each naming the path and keeping the
/// I/O error.
#[test]
fn a_transcript_that_cannot_be_written_is_named() {
    let s = Setup::new("out");
    let file = s.root.join("a-file");
    std::fs::write(&file, "").expect("file");
    let blocked = file.join("deeper").join("x");
    let mut demo = s.demo();
    demo.out = &blocked;
    match run(&demo) {
        Err(Error::CreateDir { path, source }) => {
            assert_eq!(path, file.join("deeper"));
            assert_eq!(source.kind(), std::io::ErrorKind::NotADirectory);
        }
        other => panic!("{other:?}"),
    }

    demo.out = &s.scratch;
    match run(&demo) {
        Err(Error::WriteTranscript { path, source }) => {
            assert_eq!(path, s.scratch);
            assert_eq!(source.kind(), std::io::ErrorKind::IsADirectory);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches the flags not reaching the verb, and the text it prints: through the `xtask` binary in a
/// repository root, a relative `--root`, the default `--out` under it, the cargo named by `CARGO`,
/// `--port`, `--ready-timeout` and `--scratch` build, run and record, and the verb says where; with
/// `--bin-dir` and a bare `--out` file name the transcript lands in the working directory without a build,
/// and without `--scratch` the run's directory is made under the root's `target` and removed.
#[test]
fn the_xtask_binary_records_with_its_flags() {
    let s = Setup::new("cli");
    let run_cli = |flags: &[&str]| {
        Command::new(XTASK)
            .arg("site-demo")
            .args(flags)
            .args(["--port", "0", "--job-port", "0"])
            .current_dir(&s.root)
            .arg("--scratch")
            .arg(&s.scratch)
            .env("CARGO", s.fakes.join("cargo"))
            .output()
            .expect("run xtask")
    };
    let out = run_cli(&["--root", ".", "--ready-timeout", "30"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "recorded ./site/test/demo.transcript\n"
    );
    assert!(s.root.join("site/test/demo.transcript").is_file());
    assert!(s.root.join("target/debug/cargo-args").is_file());

    let out = run_cli(&["--bin-dir", "target/release", "--out", "t.transcript"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "recorded t.transcript\n"
    );
    assert!(s.root.join("t.transcript").is_file());

    // Without `CARGO` in the environment the program is `cargo`, which is not run when the programs are given.
    let out = Command::new(XTASK)
        .args([
            "site-demo",
            "--bin-dir",
            "target/release",
            "--out",
            "u.transcript",
        ])
        .args(["--port", "0", "--job-port", "0"])
        .current_dir(&s.root)
        .env_remove("CARGO")
        .output()
        .expect("run xtask");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(s.root.join("u.transcript").is_file());
    // Without --scratch the run's directory is made in `target` under the root, and removed from it.
    let left: Vec<_> = std::fs::read_dir(s.root.join("target"))
        .expect("target")
        .map(|e| e.expect("entry").file_name())
        .filter(|n| n.to_string_lossy().starts_with("xtask-site-demo"))
        .collect();
    assert_eq!(left, Vec::<std::ffi::OsString>::new());
}

/// Catches a failure shown wrong at the command line: exit status 1 and the whole reason, prefixed with
/// the program and the verb, on standard error only.
#[test]
fn the_xtask_binary_prints_a_failure_as_one_line_and_exits_1() {
    let s = Setup::new("cli-fails");
    s.defect("build-fails");
    let cargo = s.fakes.join("cargo");
    let out = Command::new(XTASK)
        .args(["site-demo", "--root"])
        .arg(&s.root)
        .env("CARGO", &cargo)
        .arg("--scratch")
        .arg(&s.scratch)
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "xtask: site-demo: `{} build --release -p js-lease-server -p js-cli` ended with exit status: 101\n",
            cargo.display()
        )
    );
}

/// Catches the fake client answering a verb it was not asked: an unknown verb exits 2 (so a test that
/// misspells a command fails loudly).
#[test]
fn the_fake_client_refuses_a_verb_it_does_not_know() {
    let s = Setup::new("fake-js");
    let out = Command::new(s.bins.join("js"))
        .args(["--server", "x", "--token-file", "t", "job", "grab"])
        .current_dir(&s.root)
        .output()
        .expect("run the fake");
    assert_eq!(out.status.code(), Some(2));
}

/// Runs the fake server on a port of its own, reads the line where it says where it listens (the event
/// that it can be reached), asks it `/healthz`, then `/quit`, checks that it exited 0, and returns the
/// first word group of that line (`listening on` or `bound`) and the answer to `/healthz`.
fn healthz_then_quit(name: &str, defect: Option<&str>) -> (String, String) {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Setup::new(name);
    if let Some(defect) = defect {
        s.defect(defect);
    }
    let dir = s.root.join("server");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("pool.toml"), "").expect("pool");
    std::fs::write(dir.join("client-token"), "t").expect("token");
    std::fs::set_permissions(
        dir.join("client-token"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("mode");
    // Stopped when the test ends, whichever way.
    let mut server = js_testkit::reaped::spawn(
        Command::new(s.bins.join("js-lease-server"))
            .args(["--token-file", "client-token", "--listen", "127.0.0.1:0"])
            .args(["--job-listen", "127.0.0.1:0"])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        3,
    )
    .expect("start the fake");
    let stdout = server.stdout.take().expect("piped");
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).expect("a line");
    let (said, address) = line.trim().rsplit_once(' ').expect("words and an address");
    let ask = |path: &str| {
        let mut c = std::net::TcpStream::connect(address).expect("the server listens");
        c.write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes())
            .expect("write");
        let mut answer = String::new();
        c.read_to_string(&mut answer).expect("read");
        answer
    };
    let answer = ask("/healthz");
    assert!(ask("/quit").starts_with("HTTP/1.1"));
    assert!(server.wait().expect("wait").success());
    (said.to_owned(), answer)
}

/// Catches a fake server that does not behave as the verb's probe needs: it says where it listens, answers
/// `GET /healthz` with a 200 and exits 0 when asked to quit (so the fake itself is shown to work, apart
/// from the verb).
#[test]
fn the_fake_server_says_where_it_listens_answers_healthz_and_quits_when_asked() {
    let (said, answer) = healthz_then_quit("fake-server", None);
    assert_eq!(said, "listening on");
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
}

/// Catches the fake's other modes being the wrong ones: unhealthy it says 503 and still quits,
/// unannounced it serves but says `bound` where the verb looks for `listening on`, and without a job
/// address it serves and quits as usual.
#[test]
fn the_fake_server_can_answer_503_or_leave_its_address_unannounced() {
    let (said, answer) = healthz_then_quit("fake-server-503", Some("server-unhealthy"));
    assert_eq!(said, "listening on");
    assert!(
        answer.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{answer}"
    );
    let (said, answer) = healthz_then_quit("fake-server-bound", Some("server-unannounced"));
    assert_eq!(said, "bound");
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    // Without a job address in its log it still serves and quits (its log, not its listening, is what differs).
    let (said, answer) = healthz_then_quit("fake-server-no-job", Some("server-no-job-address"));
    assert_eq!(said, "listening on");
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
}

/// The recording made with the real programs: the same as the committed one, word for word (the refusal
/// carries no id or time). Builds release binaries and binds the default ports 7300 and 7301 (the ports the
/// recording shows), so it runs only when asked for.
#[test]
#[ignore = "builds the release binaries of js-lease-server and js and uses the default ports 7300 and 7301"]
fn a_new_recording_equals_the_committed_one() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scratch = js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "demo-recorded");
    let out = scratch.join("demo.recorded");
    let status = Command::new(XTASK)
        .args(["site-demo", "--root"])
        .arg(&root)
        .arg("--out")
        .arg(&out)
        .env("CARGO", env!("CARGO"))
        .status()
        .expect("run xtask");
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&out).expect("recorded"),
        committed()
    );
}
