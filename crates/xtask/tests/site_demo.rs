//! `cargo xtask site-demo` as a person runs it: the real `xtask` binary, with `xtask-fake` as the lease
//! server, the client and cargo (a server that checks the files it is given and answers `200 ok`, a client
//! that keeps one lease in a file), and the committed recording held to the commands the verb runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use xtask::site_demo::{HOSTS_TOML, Recorder, commands};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");

/// A port nothing holds right now and no other test of this process was given.
fn free_port() -> u16 {
    static GIVEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("address")
            .port();
        let mut given = GIVEN.lock().expect("lock");
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

/// One test's directories: the root cargo runs in, the directory of the programs, the scratch directory
/// the verb is given as `TMPDIR` (to show it is left empty), and the transcript's place.
struct Setup {
    root: PathBuf,
    bins: PathBuf,
    tmp: PathBuf,
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
        let tmp = root.join("tmp");
        for dir in [&bins, &tmp, &root.join("fakes")] {
            std::fs::create_dir_all(dir).expect("scratch");
        }
        for (dir, program) in [
            (&bins, "js-lease-server"),
            (&bins, "js"),
            (&root.join("fakes"), "cargo"),
        ] {
            std::fs::hard_link(FAKE, dir.join(program)).expect("link the fake");
        }
        std::fs::write(root.join("cargo-messages"), "").expect("messages");
        let out = root.join("out").join("demo.transcript");
        Self {
            root,
            bins,
            tmp,
            out,
            _dir: dir,
        }
    }

    /// `site-demo` against the fakes in `bins` on `port`, failing as `defect` says (or not), with `more` flags.
    fn run(&self, port: u16, defect: Option<&str>, more: &[&str]) -> Output {
        let mut cmd = Command::new(XTASK);
        cmd.arg("site-demo")
            .arg("--root")
            .arg(&self.root)
            .arg("--out")
            .arg(&self.out)
            .arg("--port")
            .arg(port.to_string())
            .args(more)
            .env("TMPDIR", &self.tmp)
            .env("CARGO", self.root.join("fakes").join("cargo"));
        match defect {
            Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
            None => cmd.env_remove("XTASK_FAKE_DEFECT"),
        };
        cmd.output().expect("run xtask")
    }

    /// `run` with the programs in `target/release` taken as given (no build).
    fn run_with_bins(&self, port: u16, defect: Option<&str>, more: &[&str]) -> Output {
        let mut flags = vec!["--bin-dir", self.bins.to_str().expect("utf-8")];
        flags.extend_from_slice(more);
        self.run(port, defect, &flags)
    }

    /// Whether the verb left anything in its scratch directory.
    fn scratch_left(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.tmp)
            .expect("tmp")
            .map(|e| e.expect("entry").path())
            .collect()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Catches a transcript in another shape than the page shows: the host file, the six commands of the
/// verb with the port it was given, and what the client printed after each, in order; with the scratch
/// directory (and the server) gone after.
#[test]
fn the_recording_is_the_host_file_then_each_command_with_what_it_printed() {
    let s = Setup::new("records");
    let port = free_port();
    let out = s.run_with_bins(port, None, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("recorded "),
        "{:?}",
        out.stdout
    );
    let steps = commands(port, "1-1");
    let mut expected = Recorder::with_host_file();
    expected.command(&steps[0]);
    expected.command(&steps[1]);
    expected.command(&steps[2]);
    expected.output("lease 1-1 host gpu-01 expires 1793614960284\n");
    expected.command(&steps[3]);
    expected.output("1-1 held host gpu-01 expires 1793614960284\n");
    expected.command(&steps[4]);
    expected.output("lease 1-1 freed\n");
    expected.command(&steps[5]);
    assert_eq!(
        std::fs::read_to_string(&s.out).expect("transcript"),
        expected.finish()
    );
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a build that is not the one the script ran, or one run when `--bin-dir` says not to: cargo is
/// asked for the release build of the two packages in the root, and the programs it leaves in
/// `target/release` are the ones run.
#[test]
fn without_a_bin_dir_it_builds_the_two_packages_in_the_root_and_runs_them() {
    let s = Setup::new("builds");
    let out = s.run(free_port(), None, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let args = std::fs::read_to_string(s.root.join("target/debug/cargo-args")).expect("cargo ran");
    assert_eq!(args, "build --release -p js-lease-server -p js-cli\n");
    assert!(s.out.is_file());

    // A relative root (the default, `.`) leaves relative program paths: they are resolved before the
    // server is started in a directory of its own.
    let relative = Setup::new("relative-root");
    let out = Command::new(XTASK)
        .args([
            "site-demo",
            "--root",
            ".",
            "--out",
            "t.transcript",
            "--port",
        ])
        .arg(free_port().to_string())
        .current_dir(&relative.root)
        .env("TMPDIR", &relative.tmp)
        .env("CARGO", relative.root.join("fakes").join("cargo"))
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run xtask");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(relative.root.join("t.transcript").is_file());

    let with_dir = Setup::new("no-build");
    assert!(
        with_dir
            .run_with_bins(free_port(), None, &[])
            .status
            .success()
    );
    assert!(!with_dir.root.join("target/debug/cargo-args").exists());
}

/// Catches a failed build that goes on to record (the check can fail): the verb fails with the build's
/// reason and writes no transcript; a cargo that cannot be started is named too.
#[test]
fn a_failed_build_fails_the_verb_and_writes_nothing() {
    let s = Setup::new("build-fails");
    let out = s.run(free_port(), Some("build-fails"), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("site-demo: cargo build failed"),
        "{}",
        stderr(&out)
    );
    assert!(!s.out.exists());

    let mut cmd = Command::new(XTASK);
    cmd.args(["site-demo", "--root"])
        .arg(&s.root)
        .env("CARGO", s.root.join("no-such-cargo"));
    let out = cmd.output().expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("site-demo: cannot start cargo"),
        "{}",
        stderr(&out)
    );
}

/// Catches a server that dies being waited for until the deadline (the check can fail): the verb fails at
/// once with the server's own words, and leaves nothing in the scratch directory.
#[test]
fn a_server_that_exits_fails_the_verb_with_its_log_and_leaves_no_scratch() {
    let s = Setup::new("server-dies");
    let out = s.run_with_bins(free_port(), Some("server-dies"), &[]);
    assert_eq!(out.status.code(), Some(1));
    let why = stderr(&out);
    assert!(
        why.contains("js-lease-server ended with") && why.contains("asked to die"),
        "{why}"
    );
    assert!(!s.out.exists());
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a server that never says 200 hanging the verb: with a one-second deadline it fails naming it, and
/// the server it started is stopped and the scratch directory removed.
#[test]
fn a_server_that_stays_unhealthy_fails_the_verb_at_the_deadline() {
    let s = Setup::new("server-unhealthy");
    let out = s.run_with_bins(
        free_port(),
        Some("server-unhealthy"),
        &["--ready-timeout", "1"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("the server did not answer within 1s"),
        "{}",
        stderr(&out)
    );
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a server started without the files the recording shows (the check can fail): the fake server
/// refuses a start without the host file and an owner-only token, so a token file written 0644 fails here.
/// The fake is the real server's checks in miniature; this shows the verb's files pass them.
#[test]
fn the_server_is_given_a_host_file_and_an_owner_only_token_file() {
    let s = Setup::new("files");
    assert!(s.run_with_bins(free_port(), None, &[]).status.success());
    // The fake server exits 5 when the files are missing or too open: run it by hand in a bare directory.
    let bare = s.root.join("bare");
    std::fs::create_dir_all(&bare).expect("bare");
    std::fs::write(bare.join("client-token"), "x").expect("token");
    let refused = Command::new(s.bins.join("js-lease-server"))
        .args(["--token-file", "client-token"])
        .current_dir(&bare)
        .output()
        .expect("run the fake");
    assert_eq!(refused.status.code(), Some(5));
    // An owner-only token and no host file is refused too.
    std::fs::set_permissions(
        bare.join("client-token"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("mode");
    let refused = Command::new(s.bins.join("js-lease-server"))
        .args(["--token-file", "client-token"])
        .current_dir(&bare)
        .output()
        .expect("run the fake");
    assert_eq!(refused.status.code(), Some(5));
}

/// Catches a client failure swallowed: the verb fails naming the command that failed and what it said, and
/// writes no transcript.
#[test]
fn a_failing_client_command_fails_the_verb_naming_it() {
    let s = Setup::new("js-fails");
    let out = s.run_with_bins(free_port(), Some("js-fails"), &[]);
    assert_eq!(out.status.code(), Some(1));
    let why = stderr(&out);
    assert!(
        why.contains("`js --server http://127.0.0.1:") && why.contains("lease acquire"),
        "{why}"
    );
    assert!(why.contains("asked to fail"), "{why}");
    assert!(!s.out.exists());
}

/// Catches a release of an unknown lease: an acquire that prints no lease fails the verb rather than
/// releasing an empty id.
#[test]
fn an_acquire_that_prints_no_lease_fails_the_verb() {
    let s = Setup::new("no-lease");
    let out = s.run_with_bins(free_port(), Some("js-no-lease"), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("`js lease acquire` printed no lease: nothing for you"),
        "{}",
        stderr(&out)
    );
}

/// Catches a missing program reported as a panic or silence: a bin directory without the server, or without
/// the client, fails naming the program.
#[test]
fn a_missing_program_is_named() {
    let s = Setup::new("missing");
    std::fs::remove_file(s.bins.join("js-lease-server")).expect("remove");
    let out = s.run_with_bins(free_port(), None, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("cannot start js-lease-server"),
        "{}",
        stderr(&out)
    );

    let s = Setup::new("missing-js");
    std::fs::remove_file(s.bins.join("js")).expect("remove");
    let out = s.run_with_bins(free_port(), None, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("cannot start") && stderr(&out).contains("/js:"),
        "{}",
        stderr(&out)
    );
    assert_eq!(s.scratch_left(), Vec::<PathBuf>::new());
}

/// Catches a transcript path that is refused: a bare file name (no directory part) is written in the
/// working directory, and a path under a file is reported rather than panicking.
#[test]
fn the_transcript_goes_where_out_says() {
    let s = Setup::new("out");
    let mut cmd = Command::new(XTASK);
    cmd.args(["site-demo", "--out", "t.transcript", "--bin-dir"])
        .arg(&s.bins)
        .arg("--port")
        .arg(free_port().to_string())
        .current_dir(&s.root)
        .env("TMPDIR", &s.tmp)
        .env_remove("XTASK_FAKE_DEFECT");
    assert!(cmd.output().expect("run xtask").status.success());
    assert!(s.root.join("t.transcript").is_file());

    let blocked = s.root.join("t.transcript").join("deeper").join("x");
    let out = Command::new(XTASK)
        .args(["site-demo", "--bin-dir"])
        .arg(&s.bins)
        .arg("--out")
        .arg(&blocked)
        .arg("--port")
        .arg(free_port().to_string())
        .env("TMPDIR", &s.tmp)
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("cannot make the directory of the transcript"),
        "{}",
        stderr(&out)
    );

    // Without `--out` the default place under `--root` is used.
    let out = s.run_with_bins(free_port(), None, &[]);
    assert!(out.status.success());
    let default = Command::new(XTASK)
        .args(["site-demo", "--root"])
        .arg(&s.root)
        .arg("--bin-dir")
        .arg(&s.bins)
        .arg("--port")
        .arg(free_port().to_string())
        .env("TMPDIR", &s.tmp)
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run xtask");
    assert!(default.status.success(), "{}", stderr(&default));
    assert!(s.root.join("site/test/demo.transcript").is_file());
}

/// Catches the fake client answering a verb it was not asked: an unknown verb exits 2 (so a test that
/// misspells a command fails loudly).
#[test]
fn the_fake_client_refuses_a_verb_it_does_not_know() {
    let s = Setup::new("fake-js");
    let out = Command::new(s.bins.join("js"))
        .args(["--server", "x", "--token-file", "t", "lease", "grab"])
        .current_dir(&s.root)
        .output()
        .expect("run the fake");
    assert_eq!(out.status.code(), Some(2));
}

/// Catches a fake server that does not behave as the verb's probe needs: it answers `GET /healthz` with a
/// 200 and exits 0 when asked to quit (so the fake itself is shown to work, apart from the verb).
#[test]
fn the_fake_server_answers_healthz_and_quits_when_asked() {
    assert!(healthz_then_quit("fake-server", None).starts_with("HTTP/1.1 200 OK"));
    // Unhealthy, it says 503 (what the deadline test makes the verb wait on) and still quits.
    let answer = healthz_then_quit("fake-server-503", Some("server-unhealthy"));
    assert!(
        answer.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{answer}"
    );
}

/// Runs the fake server, asks it `/healthz`, then `/quit`, checks that it exited 0, and returns the answer to
/// `/healthz`.
fn healthz_then_quit(name: &str, defect: Option<&str>) -> String {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    let s = Setup::new(name);
    let dir = s.root.join("server");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("hosts.toml"), HOSTS_TOML).expect("hosts");
    std::fs::write(dir.join("client-token"), "t").expect("token");
    std::fs::set_permissions(
        dir.join("client-token"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("mode");
    let port = free_port();
    // Stopped when the test ends, whichever way.
    let mut server = js_testkit::reaped::spawn(
        Command::new(s.bins.join("js-lease-server"))
            .args([
                "--token-file",
                "client-token",
                "--listen",
                &format!("127.0.0.1:{port}"),
            ])
            .current_dir(&dir)
            .env("XTASK_FAKE_DEFECT", defect.unwrap_or("")),
        3,
    )
    .expect("start the fake");
    let ask = |path: &str| {
        for _ in 0..100 {
            if let Ok(mut c) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                c.write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes())
                    .expect("write");
                let mut answer = String::new();
                c.read_to_string(&mut answer).expect("read");
                return answer;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("the fake server never listened");
    };
    let answer = ask("/healthz");
    assert!(ask("/quit").starts_with("HTTP/1.1"));
    assert!(server.wait().expect("wait").success());
    answer
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

/// The recording made with the real programs: the same as the committed one but for the lease id and the
/// expiry. Builds release binaries and binds port 7300, so it runs only when asked for.
#[test]
#[ignore = "builds the release binaries of js-lease-server and js and uses port 7300"]
fn a_new_recording_differs_from_the_committed_one_only_in_ids_and_times() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("demo.recorded");
    let status = Command::new(XTASK)
        .args(["site-demo", "--root"])
        .arg(&root)
        .arg("--out")
        .arg(&out)
        .env("CARGO", env!("CARGO"))
        .status()
        .expect("run xtask");
    assert!(status.success());
    let mask = |text: String| {
        text.lines()
            .map(|l| {
                let l = l.split(" expires ").next().expect("a line");
                l.split(' ')
                    .map(|w| {
                        if w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('-') {
                            "ID"
                        } else {
                            w
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
    };
    let recorded = std::fs::read_to_string(&out).expect("recorded");
    assert_eq!(mask(recorded), mask(committed()));
}
