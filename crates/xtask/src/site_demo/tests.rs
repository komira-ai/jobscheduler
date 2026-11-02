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

/// Catches the commands drifting from what the page shows: the default recording's six command lines, word
/// for word (the server start has its `&` and no `--listen`, the release names the id).
#[test]
fn the_default_recording_runs_these_six_commands() {
    let base = "js --server http://127.0.0.1:7300 --token-file client-token lease";
    assert_eq!(
        commands(DEFAULT_PORT, "1-1"),
        [
            "printf 'a-local-token-of-at-least-32-bytes\\n' > client-token && chmod 600 client-token".to_owned(),
            "js-lease-server --state-file leases.toml --boot initialise --token-file client-token --pool-config hosts.toml &".to_owned(),
            format!("{base} acquire --class gpu-host --gpus 1 --gpu-class nvidia-a100 --max-wall 1h"),
            format!("{base} list"),
            format!("{base} release 1-1"),
            format!("{base} list"),
        ]
    );
}

/// Catches a port that reaches the client and not the server (or the reverse), and the default port given a
/// `--listen` the recording does not show; port 0 is asked of the server as 0.
#[test]
fn another_port_reaches_the_server_and_every_client_command() {
    assert!(!server_args(DEFAULT_PORT).contains(&"--listen".to_owned()));
    let server = server_args(7411);
    assert_eq!(server[server.len() - 2..], ["--listen", "127.0.0.1:7411"]);
    let any = server_args(0);
    assert_eq!(any[any.len() - 2..], ["--listen", "127.0.0.1:0"]);
    for args in [
        acquire_args(7411),
        list_args(7411),
        release_args(7411, "2-3"),
    ] {
        assert_eq!(args[..2], ["--server", "http://127.0.0.1:7411"]);
        assert_eq!(args[2..4], ["--token-file", TOKEN_FILE]);
    }
}

/// Catches the host file changing without the recording: the file is one host of eight GPUs and the token is
/// at least the 32 bytes the server asks for.
#[test]
fn the_host_file_is_one_host_and_the_token_is_long_enough() {
    let parsed: toml::Table = toml::from_str(HOSTS_TOML).expect("valid toml");
    let hosts = parsed["host"].as_array().expect("hosts");
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0]["id"].as_str(), Some("gpu-01"));
    assert_eq!(hosts[0]["resources"]["gpus"].as_integer(), Some(8));
    assert!(TOKEN.len() >= 32);
}

/// Catches an id read from the wrong line or the wrong word: the id of `lease <id> host ...`, none from
/// output that is not an acquire (a `lease` word elsewhere, a listing, a lease line without a host).
#[test]
fn the_lease_id_is_the_word_after_lease_on_the_acquire_line() {
    assert_eq!(
        lease_id("lease 4-2 host gpu-01 expires 17\n").as_deref(),
        Some("4-2")
    );
    assert_eq!(
        lease_id("noise\nlease 5-1 host h expires 1\n").as_deref(),
        Some("5-1")
    );
    for text in [
        "",
        "4-2 held host gpu-01 expires 17\n",
        "lease 4-2 freed\n",
        "lease\n",
        "lease 4-2\n",
        "the lease 4-2 host h\n",
    ] {
        assert_eq!(lease_id(text), None, "{text:?}");
    }
}

/// Catches a transcript in another format: the comment, the host file, `$ ` lines, the output as printed, a
/// missing final line end added, and no output adding nothing.
#[test]
fn the_recorder_writes_the_host_file_then_each_command_with_its_output() {
    let mut rec = Recorder::with_host_file();
    rec.command("one");
    rec.output("a\nb");
    rec.command("two");
    rec.output("");
    rec.command("three");
    rec.output("c\n");
    let text = rec.finish();
    assert!(text.starts_with(
        "# hosts.toml: one host, listed by hand; there is no agent\n[[host]]\nid = \"gpu-01\"\n"
    ));
    assert!(
        text.ends_with("driver = \"550\"\n$ one\na\nb\n$ two\n$ three\nc\n"),
        "{text}"
    );
}

/// Catches a wait that never ends or ends early: ready on the third probe after two sleeps of the
/// interval, with no sleep once ready, and the value the probe gave comes back.
#[test]
fn the_wait_ends_when_the_probe_says_ready() {
    let answers = Cell::new(0);
    let mut c = clock();
    let probe = || {
        answers.set(answers.get() + 1);
        Ok::<_, ()>((answers.get() == 3).then_some("up"))
    };
    let interval = Duration::from_millis(100);
    assert_eq!(
        wait_ready(probe, &mut c, Duration::from_secs(10), interval),
        Ok(Some("up"))
    );
    assert_eq!(answers.get(), 3);
    assert_eq!(c.sleeps, [interval; 2]);
}

/// Catches a wait without a deadline (the check can fail): a probe that never says ready gives up once the
/// timeout has passed by the clock, after timeout/interval + 1 probes, and says nothing was ready.
#[test]
fn a_server_that_never_answers_ends_the_wait_at_the_deadline() {
    let answers = Cell::new(0);
    let mut c = clock();
    let probe = || {
        answers.set(answers.get() + 1);
        Ok::<Option<()>, ()>(None)
    };
    let result = wait_ready(
        probe,
        &mut c,
        Duration::from_secs(1),
        Duration::from_millis(250),
    );
    assert_eq!(result, Ok(None));
    assert_eq!(answers.get(), 5);
    assert_eq!(c.now, Duration::from_secs(1));
}

/// Catches a dead server waited for until the deadline: the probe's error ends the wait at once, with no sleep.
#[test]
fn a_probe_error_ends_the_wait_without_sleeping() {
    let mut c = clock();
    let result = wait_ready(
        || Err::<Option<()>, _>("it died"),
        &mut c,
        Duration::from_secs(9),
        Duration::from_secs(1),
    );
    assert_eq!(result, Err("it died"));
    assert!(c.sleeps.is_empty());
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

/// Catches scratch files made wrong: the host file, an owner-only token file and an empty log are made, and
/// a directory that is not there is a failure, not a panic.
#[test]
fn the_scratch_files_are_made_and_a_missing_directory_fails() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::under(std::env::temp_dir(), "site-demo-prepare");
    prepare(&dir).expect("prepare");
    assert_eq!(
        std::fs::read_to_string(dir.join("hosts.toml")).expect("hosts"),
        HOSTS_TOML
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
                command: "js lease list".to_owned(),
                status: failed,
                output: "refused".to_owned(),
            },
            "`js lease list` ended with exit status: 1: refused",
        ),
        (
            Error::NoLeaseId {
                output: "nothing".to_owned(),
            },
            "`js lease acquire` printed no lease id: nothing",
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
        Error::NoLeaseId {
            output: String::new()
        }
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
    look(ended, log, Path::new("s/server.log"), listening, |_| {
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
    let ok = look(Ok(None), log(), Path::new("l"), &mut listening, |at| {
        asked.set(Some(at));
        Ok("HTTP/1.1 200 OK\r\n".to_owned())
    });
    assert_eq!(ok.expect("ready"), Some(addr(4100)));
    assert_eq!(asked.get(), Some(addr(4100)));
    let refused = look(Ok(None), log(), Path::new("l"), &mut listening, |_| {
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

/// Catches a wall clock that runs backwards, which would keep `wait_ready` waiting past its deadline: what
/// it says has passed after a sleep is never less than before it. No length of time is asked of it.
#[test]
fn the_wall_clock_never_runs_backwards_across_a_sleep() {
    let mut wall = SystemClock(Instant::now());
    let before = wall.elapsed();
    wall.sleep(Duration::ZERO);
    assert!(wall.elapsed() >= before);
}

/// The committed recording, read: its commands and its first lines.
fn committed() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/test/demo.transcript"),
    )
    .expect("site/test/demo.transcript")
}

/// Catches the recording on the landing page and the verb that makes it drifting apart (this is the check
/// that cannot be forgotten: it reads the committed file, so it fails whichever was edited alone): the host
/// block opens the file, and every `$ ` line is exactly the command the verb runs on the default port for
/// the lease the recording acquired.
#[test]
fn committed_transcript_shows_the_commands_this_verb_runs() {
    let text = committed();
    assert!(
        text.starts_with(&Recorder::with_host_file().finish()),
        "the host file block"
    );
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("lease ")?.split(' ').next())
        .expect("an acquired lease");
    let shown: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("$ ")).collect();
    assert_eq!(shown, commands(7300, id));
}

/// Catches a check that passes for any file (the check can fail): the same comparison on a transcript with
/// one flag changed does not hold.
#[test]
fn a_transcript_with_a_changed_command_does_not_match_the_verb() {
    let edited = committed().replace("--gpus 1", "--gpus 2");
    let shown: Vec<&str> = edited
        .lines()
        .filter_map(|l| l.strip_prefix("$ "))
        .collect();
    assert_ne!(shown, commands(7300, "1-1"));
}
