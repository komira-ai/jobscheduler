use std::cell::Cell;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt as _;

use super::*;

/// A clock the test moves: sleeping adds to the time and counts.
struct FakeClock {
    now: Duration,
    sleeps: Vec<Duration>,
}

impl Clock for FakeClock {
    fn elapsed(&self) -> Duration {
        self.now
    }

    fn sleep(&mut self, for_how_long: Duration) {
        self.now += for_how_long;
        self.sleeps.push(for_how_long);
    }
}

fn clock() -> FakeClock {
    FakeClock {
        now: Duration::ZERO,
        sleeps: Vec::new(),
    }
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Catches the commands drifting from what the page shows: the default recording's four command lines, word
/// for word (the server start has its `&`, the job service's address and no `--listen`).
#[test]
fn the_default_recording_runs_these_four_commands() {
    let base = "js --server http://127.0.0.1:7301 --token-file client-token job";
    let image = "registry.example/train@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(
        commands(DEFAULT_PORT, DEFAULT_JOB_PORT),
        [
            "printf 'a-local-token-of-at-least-32-bytes\\n' > client-token && chmod 600 client-token".to_owned(),
            "js-lease-server --state-file state --boot initialise --token-file client-token --agent-pool-config pool.toml --job-listen 127.0.0.1:7301 &".to_owned(),
            format!("{base} submit --id demo-1 --spec.image {image} --spec.needs.gpus 1 --spec.needs.gpu-class nvidia-a100 --spec.wall-limit-seconds 1h"),
            format!("{base} list"),
        ]
    );
}

/// Catches a port that reaches the client and not the server (or the reverse), and the default port given a
/// `--listen` the recording does not show; port 0 is asked of the server as 0, for either service, and the
/// client talks to the job service's port, not the health port.
#[test]
fn another_port_reaches_the_server_and_every_client_command() {
    assert!(!server_args(DEFAULT_PORT, DEFAULT_JOB_PORT).contains(&"--listen".to_owned()));
    let server = server_args(7411, 7412);
    assert_eq!(server[server.len() - 2..], ["--listen", "127.0.0.1:7411"]);
    let at = server
        .iter()
        .position(|a| a == "--job-listen")
        .expect("flag");
    assert_eq!(server[at + 1], "127.0.0.1:7412");
    let any = server_args(0, 0);
    assert_eq!(any[any.len() - 2..], ["--listen", "127.0.0.1:0"]);
    let at = any.iter().position(|a| a == "--job-listen").expect("flag");
    assert_eq!(any[at + 1], "127.0.0.1:0");
    for args in [submit_args(7412), list_args(7412)] {
        assert_eq!(args[..2], ["--server", "http://127.0.0.1:7412"]);
        assert_eq!(args[2..4], ["--token-file", TOKEN_FILE]);
    }
}

/// Catches the pool file changing without the recording: the file is one pool whose hosts have eight GPUs
/// of the class the job asks for, and the token is at least the 32 bytes the server asks for.
#[test]
fn the_pool_file_is_one_gpu_pool_and_the_token_is_long_enough() {
    let parsed: toml::Table = toml::from_str(POOL_TOML).expect("valid toml");
    assert_eq!(parsed["name"].as_str(), Some("gpu"));
    assert_eq!(parsed["shape"]["gpus"].as_integer(), Some(8));
    assert_eq!(parsed["shape"]["gpu_class"].as_str(), Some("nvidia-a100"));
    assert!(TOKEN.len() >= 32);
}

/// Catches an address read from the wrong line: the address after `listening for the job service addr=`
/// in a log line of the server, the first such line; none from lines that only look like it.
#[test]
fn the_job_service_address_is_the_one_the_server_logged() {
    assert_eq!(
        job_listening_addr(
            "INFO x: listening addr=127.0.0.1:7300\nINFO x: listening for the job service addr=127.0.0.1:41931 tls=false\nINFO x: listening for the job service addr=127.0.0.1:9 tls=false\n"
        ),
        Some(addr(41931))
    );
    for log in [
        "",
        "listening on 127.0.0.1:7300\n",
        "listening for the job service addr=\n",
        "listening for the job service addr=not-an-address tls=false\n",
        "listening for the job service\n",
    ] {
        assert_eq!(job_listening_addr(log), None, "{log:?}");
    }
}

/// Catches a transcript in another format: the comment, the pool file, `$ ` lines, the output as printed, a
/// missing final line end added, and no output adding nothing.
#[test]
fn the_recorder_writes_the_pool_file_then_each_command_with_its_output() {
    let mut rec = Recorder::with_pool_file();
    rec.command("one");
    rec.output("a\nb");
    rec.command("two");
    rec.output("");
    rec.command("three");
    rec.output("c\n");
    let text = rec.finish();
    assert!(
        text.starts_with("# pool.toml: the pool agents would join; no agent has\nname = \"gpu\"\n")
    );
    assert!(
        text.ends_with("gpu_class = \"nvidia-a100\"\n$ one\na\nb\n$ two\n$ three\nc\n"),
        "{text}"
    );
}

/// Catches a wait that never ends or ends early: ready on the third probe, each probe after a sleep of the
/// interval, with no sleep once ready, and the value the probe gave comes back.
#[test]
fn the_wait_ends_when_the_probe_says_ready() {
    let answers = Cell::new(0);
    let mut c = clock();
    let mut probe = || {
        answers.set(answers.get() + 1);
        Ok((answers.get() == 3).then_some(addr(4100)))
    };
    let interval = Duration::from_millis(100);
    let ready = wait_ready(&mut probe, &mut c, Duration::from_secs(10), interval);
    assert_eq!(ready.expect("ready"), Some(addr(4100)));
    assert_eq!(answers.get(), 3);
    assert_eq!(c.sleeps, [interval; 3]);
}

/// Catches a wait without a deadline (the check can fail): a probe that never says ready gives up once the
/// timeout has passed by the clock, after timeout/interval probes (the last one at the deadline), and says
/// nothing was ready.
#[test]
fn a_server_that_never_answers_ends_the_wait_at_the_deadline() {
    let answers = Cell::new(0);
    let mut c = clock();
    let mut probe = || {
        answers.set(answers.get() + 1);
        Ok(None)
    };
    let result = wait_ready(
        &mut probe,
        &mut c,
        Duration::from_secs(1),
        Duration::from_millis(250),
    );
    assert_eq!(result.expect("no error"), None);
    assert_eq!(answers.get(), 4);
    assert_eq!(c.now, Duration::from_secs(1));
}

/// Catches a dead server waited for until the deadline: the probe's error ends the wait at once, with no
/// sleep after the one before the first probe.
#[test]
fn a_probe_error_ends_the_wait_at_once() {
    let mut c = clock();
    let result = wait_ready(
        &mut || {
            Err(Error::ServerWait {
                source: ErrorKind::BrokenPipe.into(),
            })
        },
        &mut c,
        Duration::from_secs(9),
        Duration::from_secs(1),
    );
    assert!(
        matches!(result, Err(Error::ServerWait { source }) if source.kind() == ErrorKind::BrokenPipe)
    );
    assert_eq!(c.sleeps, [Duration::from_secs(1)]);
}

/// Catches a status line mistaken for healthy: only a 200 of an HTTP response is.
#[test]
fn only_a_200_status_line_is_healthy() {
    assert!(healthy("HTTP/1.1 200 OK\r\n"));
    assert!(healthy("HTTP/1.0 200\n"));
    for line in [
        "HTTP/1.1 503 Service Unavailable\r\n",
        "200 OK",
        "HTTP/1.1\r\n",
        "",
        "garbage 200",
    ] {
        assert!(!healthy(line), "{line:?}");
    }
}

/// Catches the server's address read from the wrong line: the first `listening on <address>` line among
/// other output; none from lines that only look like it.
#[test]
fn the_listening_address_is_the_one_the_server_printed() {
    assert_eq!(
        listening_addr(
            "server: warning: x\nlistening on 127.0.0.1:41234\nlistening on 127.0.0.1:9\n"
        ),
        Some(addr(41234))
    );
    for log in [
        "",
        "listening on\n",
        "listening on not-an-address\n",
        "bound 127.0.0.1:41234\n",
        " listening on 127.0.0.1:41234\n",
    ] {
        assert_eq!(listening_addr(log), None, "{log:?}");
    }
}

/// Catches an answer of the probe misread, with every outcome injected and no socket: a 200 is ready, any
/// other status is not yet, the failures of a server still coming up (refused, reset, no answer in time, an
/// early close) are not yet, and any other failure ends the wait naming the address and keeping its cause.
#[test]
fn a_probe_answer_is_ready_not_yet_or_a_failure() {
    let at = addr(4000);
    assert!(matches!(
        judge(at, Ok("HTTP/1.1 200 OK\r\n".to_owned())),
        Ok(Some(a)) if a == at
    ));
    assert!(matches!(
        judge(at, Ok("HTTP/1.1 503 Service Unavailable\r\n".to_owned())),
        Ok(None)
    ));
    for kind in [
        ErrorKind::ConnectionRefused,
        ErrorKind::ConnectionReset,
        ErrorKind::TimedOut,
        ErrorKind::UnexpectedEof,
        ErrorKind::WouldBlock,
    ] {
        assert!(matches!(judge(at, Err(kind.into())), Ok(None)), "{kind:?}");
    }
    match judge(at, Err(ErrorKind::PermissionDenied.into())) {
        Err(Error::Probe { url, source }) => {
            assert_eq!(url, "http://127.0.0.1:4000");
            assert_eq!(source.kind(), ErrorKind::PermissionDenied);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches scratch files made wrong: the pool file, an owner-only token file and an empty log are made, and
/// a directory that is not there is a failure, not a panic.
#[test]
fn the_scratch_files_are_made_and_a_missing_directory_fails() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::under(std::env::temp_dir(), "site-demo-prepare");
    prepare(&dir).expect("prepare");
    assert_eq!(
        std::fs::read_to_string(dir.join(POOL_FILE)).expect("pool"),
        POOL_TOML
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(TOKEN_FILE)).expect("token"),
        format!("{TOKEN}\n")
    );
    let mode = std::fs::metadata(dir.join(TOKEN_FILE))
        .expect("token")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(dir.join("server.log")).expect("log"),
        ""
    );
    // A second preparation of the same directory finds the token file there.
    let again = prepare(&dir).expect_err("the token file exists");
    assert_eq!(again.kind(), ErrorKind::AlreadyExists);
    let missing = prepare(&dir.join("missing")).expect_err("no directory");
    assert_eq!(missing.kind(), ErrorKind::NotFound);
}

/// Catches an error that tells less than it knows: each variant's whole message, word for word.
#[test]
fn each_error_says_what_failed_and_with_what() {
    let failed = ExitStatus::from_raw(1 << 8);
    let denied = || std::io::Error::from(ErrorKind::PermissionDenied);
    let cases = [
        (
            Error::BuildFailed {
                command: "cargo build --release".to_owned(),
                status: failed,
            },
            "`cargo build --release` ended with exit status: 1",
        ),
        (
            Error::Spawn {
                program: "cargo".to_owned(),
                source: denied(),
            },
            "cannot start cargo: permission denied",
        ),
        (
            Error::ResolveBinDir {
                path: PathBuf::from("bins"),
                source: denied(),
            },
            "cannot resolve bins: permission denied",
        ),
        (
            Error::Scratch {
                path: PathBuf::from("scratch"),
                source: denied(),
            },
            "cannot prepare the scratch directory scratch: permission denied",
        ),
        (
            Error::ServerExited {
                status: failed,
                log: "no token".to_owned(),
            },
            "js-lease-server ended with exit status: 1 before it answered: no token",
        ),
        (
            Error::NotReady {
                url: "http://127.0.0.1:7300".to_owned(),
                timeout: Duration::from_secs(30),
            },
            "http://127.0.0.1:7300 did not answer 200 within 30s",
        ),
        (
            Error::Probe {
                url: "http://127.0.0.1:7300".to_owned(),
                source: denied(),
            },
            "cannot ask http://127.0.0.1:7300: permission denied",
        ),
        (
            Error::Client {
                command: "js job list".to_owned(),
                status: failed,
                output: "refused".to_owned(),
            },
            "`js job list` ended with exit status: 1: refused",
        ),
        (
            Error::NotRefused {
                command: "js job submit".to_owned(),
                output: "job.id: demo-1".to_owned(),
            },
            "`js job submit` was not refused, and the recording shows the refusal: job.id: demo-1",
        ),
        (
            Error::NoJobAddress {
                log: "starting".to_owned(),
            },
            "js-lease-server did not say where the job service listens: starting",
        ),
        (
            Error::CreateDir {
                path: PathBuf::from("out"),
                source: denied(),
            },
            "cannot make the directory out: permission denied",
        ),
        (
            Error::WriteTranscript {
                path: PathBuf::from("out/demo.transcript"),
                source: denied(),
            },
            "cannot write out/demo.transcript: permission denied",
        ),
        (
            Error::ServerWait { source: denied() },
            "cannot ask whether js-lease-server is running: permission denied",
        ),
        (
            Error::ServerLog {
                path: PathBuf::from("scratch/server.log"),
                source: denied(),
            },
            "cannot read the server log scratch/server.log: permission denied",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}

/// Catches a cause lost: an error built from an I/O failure hands that failure back as its source.
#[test]
fn an_io_error_stays_the_source_of_its_variant() {
    use std::error::Error as _;
    let error = Error::WriteTranscript {
        path: PathBuf::from("x"),
        source: ErrorKind::NotFound.into(),
    };
    let source = error.source().expect("a source");
    assert_eq!(source.to_string(), "entity not found");
    assert!(
        Error::NoJobAddress { log: String::new() }
            .source()
            .is_none()
    );
}

/// One look at a server, with the try_wait answer and log given, and a probe that must not be called.
fn look_at(
    ended: std::io::Result<Option<ExitStatus>>,
    log: std::io::Result<String>,
    listening: &mut Option<SocketAddr>,
) -> Result<Option<SocketAddr>, Error> {
    look(ended, log, Path::new("s/server.log"), listening, &|_| {
        panic!("no address to ask")
    })
}

/// Catches a failure of the look at a starting server going unnoticed: a try_wait that fails and a log
/// that cannot be read are each their own error, and a server that has ended is reported with its log,
/// before any address is asked.
#[test]
fn a_look_that_cannot_be_taken_or_finds_the_server_gone_is_an_error() {
    let mut listening = None;
    let wait = look_at(
        Err(ErrorKind::PermissionDenied.into()),
        Ok(String::new()),
        &mut listening,
    );
    assert!(
        matches!(wait, Err(Error::ServerWait { source }) if source.kind() == ErrorKind::PermissionDenied)
    );
    let log = look_at(Ok(None), Err(ErrorKind::InvalidData.into()), &mut listening);
    assert!(
        matches!(log, Err(Error::ServerLog { path, source }) if path == Path::new("s/server.log") && source.kind() == ErrorKind::InvalidData)
    );
    let gone = look_at(
        Ok(Some(ExitStatus::from_raw(3 << 8))),
        Ok(format!("{LISTENING}127.0.0.1:9\nbye\n")),
        &mut listening,
    );
    assert!(
        matches!(gone, Err(Error::ServerExited { status, log }) if status.code() == Some(3) && log == "listening on 127.0.0.1:9\nbye")
    );
    assert_eq!(listening, None);
}

/// Catches the address of a running server lost or asked too early: with no address printed yet the
/// look is not ready and asks nothing; once printed, the address is kept and asked, and its answer
/// decides (a 200 is ready, a refused connection is not yet).
#[test]
fn a_running_server_is_asked_at_the_address_it_printed() {
    let mut listening = None;
    assert!(matches!(
        look_at(Ok(None), Ok("starting\n".to_owned()), &mut listening),
        Ok(None)
    ));
    assert_eq!(listening, None);
    let log = || Ok(format!("{LISTENING}127.0.0.1:4100\n"));
    let asked = Cell::new(None);
    let ok = look(Ok(None), log(), Path::new("l"), &mut listening, &|at| {
        asked.set(Some(at));
        Ok("HTTP/1.1 200 OK\r\n".to_owned())
    });
    assert_eq!(ok.expect("ready"), Some(addr(4100)));
    assert_eq!(asked.get(), Some(addr(4100)));
    let refused = look(Ok(None), log(), Path::new("l"), &mut listening, &|_| {
        Err(ErrorKind::ConnectionRefused.into())
    });
    assert!(matches!(refused, Ok(None)));
    assert_eq!(listening, Some(addr(4100)));
}

/// Catches a deadline that names the wrong server: the wait's answer is the address that answered;
/// with none, the error names the address the server printed, or, when it printed none, the port asked
/// for, with the deadline.
#[test]
fn the_end_of_the_wait_names_the_server_that_did_not_answer() {
    let timeout = Duration::from_secs(7);
    assert_eq!(
        settle(Some(addr(5)), Some(addr(6)), 7300, timeout).expect("ready"),
        addr(5)
    );
    let named = |listening| match settle(None, listening, 7300, timeout) {
        Err(Error::NotReady { url, timeout }) => (url, timeout),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        named(Some(addr(4100))),
        ("http://127.0.0.1:4100".to_owned(), timeout)
    );
    assert_eq!(named(None), ("http://127.0.0.1:7300".to_owned(), timeout));
}

/// The committed recording, read: its commands and its first lines.
fn committed() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/test/demo.transcript"),
    )
    .expect("site/test/demo.transcript")
}

/// Catches the recording on the landing page and the verb that makes it drifting apart (this is the check
/// that cannot be forgotten: it reads the committed file, so it fails whichever was edited alone): the pool
/// file block opens the file, every `$ ` line is exactly the command the verb runs on the default ports,
/// the submit is followed by the server's refusal, and the list by nothing.
#[test]
fn committed_transcript_shows_the_commands_this_verb_runs() {
    let text = committed();
    assert!(
        text.starts_with(&Recorder::with_pool_file().finish()),
        "the pool file block"
    );
    let shown: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("$ ")).collect();
    assert_eq!(shown, commands(DEFAULT_PORT, DEFAULT_JOB_PORT));
    let lines: Vec<&str> = text.lines().collect();
    let submitted = lines
        .iter()
        .position(|l| l.starts_with("$ js ") && l.contains(" job submit "))
        .expect("the submit");
    assert!(
        lines[submitted + 1].starts_with("js: refused: no-capable-host"),
        "{}",
        lines[submitted + 1]
    );
    assert!(lines[submitted + 2].ends_with(" job list"));
    assert_eq!(lines.len(), submitted + 3, "the list printed nothing");
}

/// Catches a check that passes for any file (the check can fail): the same comparison on a transcript with
/// one flag changed does not hold.
#[test]
fn a_transcript_with_a_changed_command_does_not_match_the_verb() {
    let edited = committed().replace("--spec.needs.gpus 1", "--spec.needs.gpus 2");
    let shown: Vec<&str> = edited
        .lines()
        .filter_map(|l| l.strip_prefix("$ "))
        .collect();
    assert_ne!(shown, commands(DEFAULT_PORT, DEFAULT_JOB_PORT));
}
