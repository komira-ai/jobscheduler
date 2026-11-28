//! The fake as the agent and a test meet it: the program started in this process on sockets of a
//! scratch directory, asked over its provider socket by a gRPC client and told what to do on its
//! control socket.

use std::ffi::OsString;
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use js_proto::provider::v1::mount_provider_client::MountProviderClient;
use js_proto::provider::v1::{
    MountMode, MountRequest, PrepareRequest, ReleaseRequest, Source, SweepRequest,
};
use js_testkit::dir::TestDir;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tonic::Code;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::transport::{Channel, Endpoint};

use super::*;
use crate::control::command;
use crate::main;
use crate::model::{RefuseAt, Store};

/// Connects a channel to a Unix socket.
#[derive(Clone)]
struct Unix(PathBuf);

impl Service<Uri> for Unix {
    type Response = TokioIo<UnixStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let socket = self.0.clone();
        Box::pin(async move { UnixStream::connect(socket).await.map(TokioIo::new) })
    }
}

/// Whether `check` holds within about ten seconds, asked every 10 ms.
fn eventually(mut check: impl FnMut() -> bool) -> bool {
    (0..1000).any(|_| {
        let holds = check();
        std::thread::sleep(Duration::from_millis(10));
        holds
    })
}

/// The program, running in a thread of this process, and its sockets.
struct Running {
    dir: TestDir,
    socket: PathBuf,
    control: PathBuf,
    ended: Option<JoinHandle<ExitCode>>,
}

impl Running {
    /// Starts the program over a new scratch directory with `extra` flags, and returns once it
    /// answers on its control socket.
    fn start(name: &str, extra: &[&str]) -> Running {
        // Under /tmp, not the system's temporary directory: a socket's path is at most 107
        // bytes, which a sandbox's deep TMPDIR (the build farm's) passes. The directory is the
        // test's own.
        let dir = TestDir::under("/tmp", name);
        let socket = dir.join("p.sock");
        let control = dir.join("c.sock");
        let mut words: Vec<OsString> = vec![
            "--socket".into(),
            socket.clone().into(),
            "--control".into(),
            control.clone().into(),
            "--root".into(),
            dir.join("trees").into(),
            "--mount-wait-ms".into(),
            "300".into(),
            "--restart-pause-ms".into(),
            "50".into(),
        ];
        words.extend(extra.iter().map(OsString::from));
        let ended = Some(std::thread::spawn(move || main(words)));
        let running = Running {
            dir,
            socket,
            control,
            ended,
        };
        // The control socket answers once the provider socket is bound.
        let up = eventually(|| running.try_tell("calls").is_some());
        assert!(up, "the fake did not start");
        running
    }

    /// The reply to `line` on the control socket, if it answered.
    fn try_tell(&self, line: &str) -> Option<String> {
        let mut stream = std::os::unix::net::UnixStream::connect(&self.control).ok()?;
        stream.write_all(format!("{line}\n").as_bytes()).ok()?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply).ok()?;
        Some(reply)
    }

    /// The reply to `line` on the control socket.
    fn tell(&self, line: &str) -> String {
        self.try_tell(line).expect("the control socket answers")
    }

    /// A gRPC client of the provider socket, if it is served.
    async fn try_client(&self) -> Option<MountProviderClient<Channel>> {
        let channel = Endpoint::from_static("http://fake.invalid")
            .connect_with_connector(Unix(self.socket.clone()))
            .await;
        channel.ok().map(MountProviderClient::new)
    }

    /// A gRPC client of the provider socket.
    async fn client(&self) -> MountProviderClient<Channel> {
        self.try_client()
            .await
            .expect("the provider socket answers")
    }

    /// Waits until the stall holds `count` Mount calls.
    fn wait_waiting(&self, count: usize) {
        let held = eventually(|| self.tell("waiting") == format!("{count}\n"));
        assert!(held, "the stall never held {count} Mount calls");
    }

    /// Ends the program with `quit` and returns its status.
    fn quit(mut self) -> ExitCode {
        assert_eq!(self.tell("quit"), "ok\n");
        let ended = self.ended.take().expect("running");
        ended.join().expect("the program's thread")
    }
}

/// The source `run`, index 0, of the agent `host-1`. Built field by field, so that it reads the
/// same whether or not the provider protocol has a field the fake does not read (a source's
/// attempt, which a provider may leave unread).
#[allow(clippy::field_reassign_with_default)] // The fields a test sets, whatever the message has.
fn source(run: &str) -> Option<Source> {
    let mut source = Source::default();
    source.agent = "host-1".to_owned();
    source.run = run.to_owned();
    Some(source)
}

/// A Sweep of every source of `host-1`, built as [`source`] is.
#[allow(clippy::field_reassign_with_default)] // The fields a test sets, whatever the message has.
fn sweep_all() -> SweepRequest {
    let mut sweep = SweepRequest::default();
    sweep.agent = "host-1".to_owned();
    sweep
}

fn prepare(run: &str, reference: &str) -> PrepareRequest {
    PrepareRequest {
        source: source(run),
        r#ref: reference.to_owned(),
        mode: MountMode::ReadOnly.into(),
    }
}

fn mount(run: &str) -> MountRequest {
    MountRequest {
        source: source(run),
    }
}

/// Asserts a source's way through the socket: Prepare, Mount (the tree made under the root
/// with its file, at the path answered), Release and Sweep; the refusals' codes; and the calls
/// the control socket reports.
#[tokio::test(flavor = "multi_thread")]
async fn the_provider_answers_on_its_socket() {
    let fake = Running::start("jsfmp-serve", &["--max-placements", "1"]);
    let mut client = fake.client().await;
    client
        .prepare(prepare("js.a", "cas/l/1"))
        .await
        .expect("prepared");
    let path = client
        .mount(mount("js.a"))
        .await
        .expect("mounted")
        .into_inner()
        .path;
    assert_eq!(
        path,
        fake.dir.join("trees/host-1/js.a/0").display().to_string()
    );
    assert!(Path::new(&path).join(READY_FILE).is_file());
    let refused = client
        .prepare(prepare("js.b", "refused/x"))
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    let other = client
        .prepare(prepare("js.a", "cas/l/2"))
        .await
        .expect_err("another ref");
    assert_eq!(other.code(), Code::FailedPrecondition);
    client
        .prepare(prepare("js.b", "cas/l/1"))
        .await
        .expect("prepared");
    let full = client.mount(mount("js.b")).await.expect_err("at the limit");
    assert_eq!(
        (full.code(), full.message()),
        (
            Code::Unavailable,
            "the tree was not made: too many placements: 1 are alive"
        )
    );
    client
        .release(ReleaseRequest {
            source: source("js.a"),
        })
        .await
        .expect("released");
    let swept = client.sweep(sweep_all()).await.expect("swept");
    assert_eq!(swept.into_inner().released, 1);
    let calls: Vec<String> = fake
        .tell("calls")
        .lines()
        .map(|line| line.split_once(' ').expect("a time").1.to_owned())
        .collect();
    assert_eq!(
        calls,
        [
            "Prepare ok",
            "Mount ok",
            "Prepare invalid",
            "Prepare precondition",
            "Prepare ok",
            "Mount unavailable",
            "Release ok",
            "Sweep ok"
        ]
    );
    assert_eq!(fake.quit(), ExitCode::SUCCESS);
}

/// Asserts a stalled store: a Mount waits and is placed when the test serves again; another
/// waits out the wait and is answered not ready; an unavailable store refuses every kind of
/// call; and a Mount whose tree cannot be made is `INTERNAL`.
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_holds_mount_and_unavailable_refuses_every_call() {
    // A wait long enough that the held Mount is still held when the test serves again.
    let fake = Running::start("jsfmp-stall", &["--mount-wait-ms", "3000"]);
    let mut client = fake.client().await;
    client
        .prepare(prepare("js.a", "cas/l/1"))
        .await
        .expect("prepared");
    client
        .prepare(prepare("js.b", "cas/l/1"))
        .await
        .expect("prepared");
    assert_eq!(fake.tell("stall"), "ok\n");
    let mut waiting = client.clone();
    let held = tokio::spawn(async move { waiting.mount(mount("js.a")).await });
    tokio::task::block_in_place(|| fake.wait_waiting(1));
    assert!(!held.is_finished());
    assert_eq!(fake.tell("serve"), "ok\n");
    held.await.expect("the task").expect("placed once served");
    assert_eq!(fake.tell("stall"), "ok\n");
    let not_ready = client.mount(mount("js.b")).await.expect_err("not ready");
    assert_eq!(
        (not_ready.code(), not_ready.message()),
        (Code::Unavailable, "the tree is not ready")
    );
    assert_eq!(fake.tell("unavailable"), "ok\n");
    let down = [
        client.prepare(prepare("js.c", "cas/l/1")).await.map(|_| ()),
        client.mount(mount("js.c")).await.map(|_| ()),
        client
            .release(ReleaseRequest {
                source: source("js.c"),
            })
            .await
            .map(|_| ()),
        client.sweep(sweep_all()).await.map(|_| ()),
    ];
    for answer in down {
        assert_eq!(answer.expect_err("unavailable").code(), Code::Unavailable);
    }
    assert_eq!(fake.tell("serve"), "ok\n");
    let trees = fake.dir.join("trees");
    std::fs::remove_dir_all(&trees).expect("the trees made so far");
    std::fs::write(&trees, b"a file where the trees go").expect("block the root");
    let unplaced = client.mount(mount("js.b")).await.expect_err("no tree");
    assert_eq!(unplaced.code(), Code::Internal);
    assert_eq!(fake.quit(), ExitCode::SUCCESS);
}

/// Asserts a restart: the socket closes and comes back, and a source prepared before it must be
/// prepared again (its Mount is answered not prepared here). Also the limit met at Prepare,
/// after a reset.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_forgets_the_sources_and_a_reset_moves_the_limit() {
    let fake = Running::start(
        "jsfmp-restart",
        &["--restart-pause-ms", "500", "--mount-wait-ms", "60000"],
    );
    let mut client = fake.client().await;
    client
        .prepare(prepare("js.a", "cas/l/1"))
        .await
        .expect("prepared");
    // A Mount held by a stall is answered at once by the restart, not after its wait.
    assert_eq!(fake.tell("stall"), "ok\n");
    let mut waiting = client.clone();
    let held = tokio::spawn(async move { waiting.mount(mount("js.a")).await });
    tokio::task::block_in_place(|| fake.wait_waiting(1));
    assert_eq!(fake.tell("restart"), "ok\n");
    let cut = held
        .await
        .expect("the task")
        .expect_err("cut by the restart");
    assert_eq!(
        (cut.code(), cut.message()),
        (Code::Unavailable, "the provider is restarting")
    );
    tokio::task::block_in_place(|| fake.wait_waiting(0));
    // The socket is gone for the pause and then bound again.
    tokio::task::block_in_place(|| {
        assert!(
            eventually(|| !fake.socket.exists()),
            "the provider socket closed"
        );
        assert!(
            eventually(|| fake.socket.exists()),
            "the provider socket came back"
        );
    });
    let mut client = fake.client().await;
    let lost = client.mount(mount("js.a")).await.expect_err("forgotten");
    assert_eq!(
        (lost.code(), lost.message()),
        (Code::Unavailable, "the source is not prepared here")
    );
    assert_eq!(fake.tell("reset 0 prepare"), "ok\n");
    let full = client
        .prepare(prepare("js.a", "cas/l/1"))
        .await
        .expect_err("at the limit");
    assert_eq!(full.code(), Code::Unavailable);
    let calls = fake.tell("calls");
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(calls.ends_with(" Prepare unavailable\n"), "{calls}");
    assert_eq!(fake.quit(), ExitCode::SUCCESS);
}

/// Asserts that a provider socket that cannot be bound again after a restart ends the program
/// with status 1.
#[test]
fn a_socket_not_bound_again_after_a_restart_ends_the_program() {
    let mut fake = Running::start("jsfmp-rebind", &["--restart-pause-ms", "500"]);
    assert_eq!(fake.tell("restart"), "ok\n");
    assert!(
        eventually(|| !fake.socket.exists()),
        "the provider socket closed"
    );
    std::fs::create_dir(&fake.socket).expect("a directory where the socket goes");
    let ended = fake.ended.take().expect("running");
    assert_eq!(
        ended.join().expect("the program's thread"),
        ExitCode::from(1)
    );
}

/// Asserts the control socket's refusals, by their full text, and that a client that sends
/// nothing ends only its own connection.
#[test]
fn the_control_socket_refuses_what_it_does_not_know() {
    let fake = Running::start("jsfmp-control", &[]);
    assert_eq!(
        fake.tell("reset two mount"),
        "error: reset takes a count and prepare or mount: \"reset two mount\"\n"
    );
    assert_eq!(
        fake.tell("reset 2 release"),
        "error: reset takes a count and prepare or mount: \"reset 2 release\"\n"
    );
    assert_eq!(fake.tell("reset 2 mount"), "ok\n");
    assert_eq!(fake.tell("dance"), "error: unknown command \"dance\"\n");
    drop(std::os::unix::net::UnixStream::connect(&fake.control).expect("connects"));
    assert_eq!(fake.tell("calls"), "");
    assert_eq!(fake.quit(), ExitCode::SUCCESS);
}

/// Asserts that a restart asked after a quit changes nothing: the server still ends.
#[test]
fn a_restart_after_quit_leaves_quit() {
    let shared = Shared::new(
        Store::new(1, RefuseAt::Mount),
        PathBuf::from("/t"),
        Duration::ZERO,
    );
    let (signal, signals) = watch::channel(Signal::Run(0));
    assert_eq!(command("restart", &shared, &signal), "ok\n");
    assert_eq!(*signals.borrow(), Signal::Run(1));
    assert_eq!(command("quit", &shared, &signal), "ok\n");
    assert_eq!(command("restart", &shared, &signal), "ok\n");
    assert_eq!(*signals.borrow(), Signal::Quit);
}
