use std::cell::Cell;

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
/// `--listen` the recording does not show.
#[test]
fn another_port_reaches_the_server_and_every_client_command() {
    assert!(!server_args(DEFAULT_PORT).contains(&"--listen".to_owned()));
    let server = server_args(7411);
    assert_eq!(server[server.len() - 2..], ["--listen", "127.0.0.1:7411"]);
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
/// interval, with no sleep once ready.
#[test]
fn the_wait_ends_when_the_probe_says_ready() {
    let answers = Cell::new(0);
    let mut c = clock();
    let probe = || {
        answers.set(answers.get() + 1);
        Ok(answers.get() == 3)
    };
    let interval = Duration::from_millis(100);
    assert_eq!(
        wait_ready(probe, &mut c, Duration::from_secs(10), interval),
        Ok(())
    );
    assert_eq!(answers.get(), 3);
    assert_eq!(c.sleeps, [interval; 2]);
}

/// Catches a wait without a deadline (the check can fail): a probe that never says ready gives up once the
/// timeout has passed, after about timeout/interval probes, and names the timeout.
#[test]
fn a_server_that_never_answers_ends_the_wait_at_the_deadline() {
    let answers = Cell::new(0);
    let mut c = clock();
    let probe = || {
        answers.set(answers.get() + 1);
        Ok(false)
    };
    let result = wait_ready(
        probe,
        &mut c,
        Duration::from_secs(1),
        Duration::from_millis(250),
    );
    assert_eq!(
        result,
        Err("the server did not answer within 1s".to_owned())
    );
    assert_eq!(answers.get(), 5);
    assert_eq!(c.now, Duration::from_secs(1));
}

/// Catches a dead server waited for until the deadline: the probe's error ends the wait at once, with no sleep.
#[test]
fn a_probe_error_ends_the_wait_without_sleeping() {
    let mut c = clock();
    let result = wait_ready(
        || Err("it died".to_owned()),
        &mut c,
        Duration::from_secs(9),
        Duration::from_secs(1),
    );
    assert_eq!(result, Err("it died".to_owned()));
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

/// Catches the real clock standing still or sleeping nothing.
#[test]
fn the_system_clock_counts_time_and_sleeps() {
    let mut c = SystemClock::start();
    c.sleep(Duration::from_millis(5));
    assert!(c.elapsed() >= Duration::from_millis(5));
}

/// Catches a probe that calls a port nothing listens on healthy.
#[test]
fn a_port_nothing_listens_on_is_not_healthy() {
    // The reservation is dropped with the statement: the port is free again.
    let port = js_testkit::port::Reserved::new().port();
    assert!(!probe_healthz(port));
}

/// Serves one connection on a port of its own with `answer` (nothing: it reads the request and holds the
/// connection open without a word), and returns the port.
fn serve_once(answer: &'static str) -> u16 {
    use std::io::Read as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 256];
        let _ = stream.read(&mut request);
        if answer.is_empty() {
            std::thread::sleep(Duration::from_millis(900));
        } else {
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    port
}

/// Catches a probe that takes any answer, or a silent one, for healthy: a 200 is, a 503 is not, and a server
/// that accepts and says nothing within the read timeout is not.
#[test]
fn the_probe_wants_a_200_from_the_server() {
    assert!(probe_healthz(serve_once("HTTP/1.1 200 OK\r\n\r\n")));
    assert!(!probe_healthz(serve_once(
        "HTTP/1.1 503 Service Unavailable\r\n\r\n"
    )));
    assert!(!probe_healthz(serve_once("")));
}

/// Catches the error's text losing its verb name.
#[test]
fn an_error_names_the_verb() {
    assert_eq!(Error("boom".to_owned()).to_string(), "site-demo: boom");
}
