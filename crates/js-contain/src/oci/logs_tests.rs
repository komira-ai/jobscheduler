//! The drain of a job's output, with real `sh` processes in place of `crun`: what is kept,
//! what is discarded, and that the writer is never held up. No test asserts how long
//! anything takes; the timeouts are hang guards.

use std::process::Stdio;

use super::*;

/// A hang guard, never a measurement.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn dir(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("logs-{name}"))
}

fn limits(file_cap: u64, tail: usize) -> LogLimits {
    LogLimits {
        file_cap,
        tail,
        drain_grace: Duration::from_millis(300),
    }
}

/// The launch of `sh -c script` with the pipes as its output; its status is how the run
/// ended.
fn sh(script: &'static str) -> Launch {
    Box::new(move |stdout, stderr| {
        Box::pin(async move {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .expect("sh");
            let status = child.wait().await.expect("status");
            RunEnd::Exited(status.code().expect("exited"))
        })
    })
}

/// Runs `script` with output kept as `limits` say in `dir`.
async fn run(dir: &Path, limits: LogLimits, script: &'static str) -> (RunEnd, LogTail) {
    let capture = Arc::new(Capture::create(dir, limits).expect("files"));
    let end = tokio::time::timeout(HANG_GUARD, supervise(capture.clone(), sh(script)))
        .await
        .expect("the run ended");
    (end, capture.tail())
}

/// `len` bytes of the stream `yes abcdefgh` prints, from byte `from`.
fn pattern(from: usize, len: usize) -> Vec<u8> {
    (from..from + len).map(|i| b"abcdefgh\n"[i % 9]).collect()
}

/// Catches a tail that is not the end of the stream, a file that is not capped (or not
/// the start of the stream), and a job that is held up by its own output: 10 MiB are
/// written through a pipe that holds 64 KiB, the job exits with its own status, the
/// file is the first MiB and the tail the last 64 KiB.
#[tokio::test]
async fn ten_mib_of_output_keeps_the_tail_and_a_capped_file() {
    const TEN_MIB: usize = 10 * 1024 * 1024;
    let dir = dir("ten");
    let (end, tail) = run(
        &dir,
        limits(DEFAULT_FILE_CAP, DEFAULT_TAIL),
        "yes abcdefgh | head -c 10485760; exit 7",
    )
    .await;
    assert_eq!(end, RunEnd::Exited(7));
    assert_eq!(tail.stdout.total, TEN_MIB as u64);
    assert_eq!(
        tail.stdout.bytes,
        pattern(TEN_MIB - DEFAULT_TAIL, DEFAULT_TAIL)
    );
    assert!(tail.stdout.file_cut);
    let file = std::fs::read(stdout_path(&dir)).expect("file");
    assert_eq!(file.len() as u64, DEFAULT_FILE_CAP);
    assert!(file == pattern(0, file.len()), "the file is the first MiB");
    assert_eq!(tail.stderr, StreamTail::default());
}

/// Catches a writer that stalls once the cap is reached because the drain stopped: ten
/// times the cap is written, far more than a pipe holds, and the job finishes; the file
/// holds the cap and nothing more.
#[tokio::test]
async fn output_past_the_cap_is_discarded_but_still_drained() {
    let dir = dir("past-cap");
    let (end, tail) = run(
        &dir,
        limits(65_536, 100),
        "head -c 655360 /dev/zero; exit 0",
    )
    .await;
    assert_eq!(end, RunEnd::Exited(0));
    assert_eq!(tail.stdout.total, 655_360);
    assert_eq!(tail.stdout.bytes, vec![0_u8; 100]);
    assert!(tail.stdout.file_cut);
    let size = std::fs::metadata(stdout_path(&dir)).expect("file").len();
    assert_eq!(size, 65_536);
}

/// Catches one stream's bytes in the other's tail or file, and a last line lost for
/// want of a newline: the two streams are written interleaved, each ends without one.
#[tokio::test]
async fn the_streams_are_kept_apart_and_an_unterminated_last_line_is_kept() {
    let dir = dir("apart");
    let (end, tail) = run(
        &dir,
        LogLimits::default(),
        "echo out1; echo err1 >&2; printf out2; printf err2 >&2",
    )
    .await;
    assert_eq!(end, RunEnd::Exited(0));
    assert_eq!(tail.stdout.bytes, b"out1\nout2");
    assert_eq!(tail.stderr.bytes, b"err1\nerr2");
    assert_eq!((tail.stdout.total, tail.stderr.total), (9, 9));
    assert!(!tail.stdout.file_cut && !tail.stderr.file_cut);
    assert_eq!(
        std::fs::read(stdout_path(&dir)).expect("out"),
        b"out1\nout2"
    );
    assert_eq!(
        std::fs::read(stderr_path(&dir)).expect("err"),
        b"err1\nerr2"
    );
}

/// Catches a run that waits for the end of a pipe a process of the job holds: `sh`
/// leaves a background `sleep` that inherited both pipes and exits. The run ends with the
/// shell's status after the grace, and what was written before is kept.
#[tokio::test]
async fn the_drain_ends_when_a_process_left_behind_holds_the_pipe() {
    let dir = dir("grandchild");
    let (end, tail) = run(&dir, limits(1024, 1024), "sleep 20 & echo before; exit 3").await;
    assert_eq!(end, RunEnd::Exited(3));
    assert_eq!(tail.stdout.bytes, b"before\n");
}

/// Catches `finish` reporting a drain that did not close as closed (and the reverse), and
/// readers left running after the wait: one pipe is closed, the other held open.
#[tokio::test]
async fn a_drain_says_whether_both_pipes_closed() {
    let dir = dir("finish");
    let capture = Capture::create(&dir, limits(10, 10)).expect("files");
    let (held, rx) = pipe::pipe().expect("pipe");
    let (shut, rx_shut) = pipe::pipe().expect("pipe");
    drop(shut);
    let drain = Drain {
        out: tokio::spawn(pump(rx, capture.out.clone())),
        err: tokio::spawn(pump(rx_shut, capture.err.clone())),
    };
    assert!(!drain.finish(Duration::from_millis(100)).await);
    drop(held);
    let (none, rx) = pipe::pipe().expect("pipe");
    let (also, rx_also) = pipe::pipe().expect("pipe");
    drop((none, also));
    let drain = Drain {
        out: tokio::spawn(pump(rx, capture.out.clone())),
        err: tokio::spawn(pump(rx_also, capture.err.clone())),
    };
    assert!(drain.finish(HANG_GUARD).await);
}

/// Catches a ring that keeps the wrong bytes across chunk boundaries (an old chunk not
/// pushed out, a chunk longer than the ring not cut), a file that goes past the cap or
/// stops being the start of the stream, and a tail of zero bytes that panics.
#[test]
fn the_ring_holds_the_last_bytes_and_the_file_the_first() {
    let dir = dir("sink");
    let file = std::fs::File::create(dir.join("f")).expect("file");
    let sink = Sink::new(file, limits(5, 4));
    sink.take(b"ab");
    assert_eq!(sink.tail().bytes, b"ab");
    sink.take(b"cdef");
    assert_eq!(sink.tail().bytes, b"cdef");
    sink.take(b"0123456789");
    let tail = sink.tail();
    assert_eq!(tail.bytes, b"6789");
    assert_eq!(tail.total, 16);
    assert!(tail.file_cut);
    assert_eq!(std::fs::read(dir.join("f")).expect("read"), b"abcde");

    let file = std::fs::File::create(dir.join("g")).expect("file");
    let none = Sink::new(file, limits(5, 0));
    none.take(b"abc");
    assert_eq!(none.tail().bytes, b"");
    assert_eq!(none.tail().total, 3);
}

/// Catches a full disk that stops the job or loses the tail: a file that cannot be written
/// is given up on, marked cut, and the ring and the count go on.
#[test]
fn a_file_that_cannot_be_written_is_given_up_on_and_the_tail_goes_on() {
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("/dev/full");
    let sink = Sink::new(full, limits(1024, 8));
    sink.take(b"first");
    sink.take(b"second");
    let tail = sink.tail();
    assert_eq!(tail.bytes, b"stsecond");
    assert_eq!(tail.total, 11);
    assert!(tail.file_cut);
}

/// Catches the pipes failing taken for a run: the run does not start, and says why.
#[tokio::test]
async fn pipes_that_cannot_be_made_fail_the_run_before_it_starts() {
    fn no_pipe() -> std::io::Result<(pipe::Sender, pipe::Receiver)> {
        Err(std::io::Error::other("no pipes"))
    }
    let dir = dir("nopipe");
    let capture = Arc::new(Capture::create(&dir, limits(1, 1)).expect("files"));
    let end = supervise_with(no_pipe, capture, sh("exit 0")).await;
    assert_eq!(
        end,
        RunEnd::Failed("the pipes of the output: no pipes".to_owned())
    );
}

/// Catches the files made anywhere but in the lease directory, and an error that does not
/// name the file.
#[test]
fn the_files_are_in_the_lease_directory_and_a_missing_one_is_named() {
    let dir = dir("paths");
    assert_eq!(stdout_path(&dir), dir.join("stdout"));
    assert_eq!(stderr_path(&dir), dir.join("stderr"));
    Capture::create(&dir, LogLimits::default()).expect("both");
    let missing = &dir.join("none");
    let why = Capture::create(missing, LogLimits::default()).expect_err("no directory");
    assert!(why.contains(&missing.display().to_string()), "{why}");
}

/// Catches defaults drifting from the documented bounds (the podman path's 1 MiB).
#[test]
fn the_default_limits_are_a_mebibyte_of_file_and_sixty_four_kibibytes_of_tail() {
    let limits = LogLimits::default();
    assert_eq!(limits.file_cap, 1_048_576);
    assert_eq!(limits.tail, 65_536);
    assert_eq!(limits.drain_grace, DEFAULT_DRAIN_GRACE);
}

/// Catches an agent that keeps a write end of a pipe after the child is gone: the drain
/// would then wait out the whole grace for an end that never comes. With a grace of an
/// hour the run completes (the hang guard would fire otherwise) and the tail is whole.
#[tokio::test]
async fn the_agent_holds_no_write_end_after_the_run() {
    let dir = dir("no-leak");
    let hour = LogLimits {
        drain_grace: Duration::from_secs(3600),
        ..limits(1024, 1024)
    };
    let (end, tail) = run(&dir, hour, "echo whole; echo err >&2").await;
    assert_eq!(end, RunEnd::Exited(0));
    assert_eq!(tail.stdout.bytes, b"whole\n");
    assert_eq!(tail.stderr.bytes, b"err\n");
}

/// Catches an abort that waits for the drain or loses what was read: a run that has
/// written and then hangs is dropped; the tail is what was written, and the readers end
/// with the pipes although the grace is an hour.
#[tokio::test]
async fn an_aborted_run_leaves_the_partial_tail_and_does_not_wait() {
    use std::io::Write as _;
    let dir = dir("abort");
    let hour = LogLimits {
        drain_grace: Duration::from_secs(3600),
        ..limits(1024, 1024)
    };
    let capture = Arc::new(Capture::create(&dir, hour).expect("files"));
    let (said, heard) = tokio::sync::oneshot::channel::<()>();
    let launch: Launch = Box::new(move |stdout, _stderr| {
        Box::pin(async move {
            let mut out = std::fs::File::from(stdout);
            out.write_all(b"partial").expect("write");
            let _ = said.send(());
            std::future::pending::<RunEnd>().await
        })
    });
    let supervised = supervise(capture.clone(), launch);
    tokio::select! {
        _ = supervised => panic!("a run that hangs ended"),
        _ = heard => {}
    }
    // The run is dropped; its readers finish what is in the pipe.
    tokio::time::timeout(HANG_GUARD, async {
        while capture.tail().stdout.total < 7 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the readers read what was written");
    assert_eq!(capture.tail().stdout.bytes, b"partial");
}

/// Catches a log file that truncates or follows what is already there: a file in the way,
/// and a symbolic link in its place, are errors naming the path; the files are private.
#[test]
fn an_existing_file_or_link_is_refused_and_the_files_are_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("refuse");
    let made = Capture::create(&dir, LogLimits::default());
    let mode = |name| {
        std::fs::metadata(dir.join(name))
            .expect("file")
            .permissions()
            .mode()
    };
    assert!(made.is_ok());
    assert_eq!(mode("stdout") & 0o777, 0o600);
    assert_eq!(mode("stderr") & 0o777, 0o600);
    let why = Capture::create(&dir, LogLimits::default()).expect_err("exists");
    assert!(
        why.contains(&stdout_path(&dir).display().to_string()),
        "{why}"
    );

    let linked = self::dir("refuse-link");
    std::os::unix::fs::symlink(dir.join("stdout"), stdout_path(&linked)).expect("link");
    let why = Capture::create(&linked, LogLimits::default()).expect_err("link");
    assert!(
        why.contains(&stdout_path(&linked).display().to_string()),
        "{why}"
    );
}
