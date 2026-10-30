//! A stand-in for the root helper that speaks its real socket protocol. The runtime under
//! test connects to a Unix socket exactly as it connects to the helper, so the frames, the
//! nonce, the descriptors a `run` carries and the answers are the helper's own; only what
//! the verbs do on the machine is faked: a user is a directory, a run waits until a test
//! ends it, a kill ends it with `SIGKILL`.
//!
//! On a Mac a test can ask the fake to start the program of a `run` for real, as the
//! user running the tests (`real_runs`), which is how the `sandbox-exec` command line the
//! runtime builds is applied without root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use js_mac_session::client::Client;
use js_mac_session::lease::{parse_lease, user_name};
use js_mac_session::proto::{self, Call, RUN_FDS, Reply, Request};

use crate::support::Scratch;

/// How a scripted run ends.
#[derive(Debug, Clone, Copy)]
pub enum End {
    /// The process exited with a code, or was ended by a signal.
    Exit(Option<i32>, Option<i32>),
    /// The helper's connection ends with no exit reported.
    HangUp,
}

/// One request the fake saw.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// The verb, as the protocol names it.
    pub verb: String,
    /// The lease the request names; empty for a verb that names none.
    pub lease: String,
    /// For `run`: the argument vector.
    pub argv: Vec<String>,
    /// For `run`: the environment.
    pub env: Vec<(String, String)>,
    /// For `run`: whether the fourth descriptor (the working directory) is a directory.
    pub cwd_is_dir: bool,
    /// For `run`: how many bytes the first descriptor (standard input) holds.
    pub stdin_bytes: usize,
}

#[derive(Default)]
struct State {
    seen: Vec<Seen>,
    /// Verbs refused, with the reason.
    refused: BTreeMap<String, String>,
    /// What every `run` writes to standard output and standard error before it starts.
    prints: (Vec<u8>, Vec<u8>),
    ends: BTreeMap<String, End>,
    killed: BTreeSet<String>,
    /// Whether `user-create` leaves out the home directory the real helper makes.
    skip_homes: bool,
    /// Whether a `run` starts its program for real.
    real: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    homes: PathBuf,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes a request and returns the reason it is refused, when it is.
    fn take(&self, seen: Seen) -> Option<String> {
        let mut state = self.lock();
        let refusal = state.refused.get(&seen.verb).cloned();
        state.seen.push(seen);
        refusal
    }
}

/// The fake helper, serving on a socket of its own until the test ends.
pub struct FakeHelper {
    socket: PathBuf,
    shared: Arc<Shared>,
}

/// Makes the socket names of one test binary differ.
static SOCKETS: AtomicU64 = AtomicU64::new(0);

impl FakeHelper {
    /// A helper whose lease users' homes are made in `dir/homes`.
    pub fn start(dir: &Scratch) -> Self {
        let homes = dir.path("homes");
        fs::create_dir(&homes).unwrap();
        // Short and outside the test's directory: a socket path is limited to about a
        // hundred bytes, which a checkout's path may already use up.
        let socket = std::env::temp_dir().join(format!(
            "jsn-{}-{}",
            std::process::id(),
            SOCKETS.fetch_add(1, Ordering::SeqCst)
        ));
        let listener = UnixListener::bind(&socket).unwrap();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            homes: fs::canonicalize(&homes).unwrap(),
        });
        let serving = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let (stream, shared) = (stream.unwrap(), Arc::clone(&serving));
                thread::spawn(move || serve(&stream, &shared));
            }
        });
        Self { socket, shared }
    }

    /// A client of this helper, as the agent holds one.
    pub fn client(&self) -> Client {
        Client::new(&self.socket)
    }

    /// The socket this helper serves on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Where the lease users' homes are, canonical.
    pub fn homes(&self) -> &Path {
        &self.shared.homes
    }

    /// The home of the user of `lease`.
    pub fn home_of(&self, lease: &str) -> PathBuf {
        let id = parse_lease(lease).unwrap();
        self.shared.homes.join(user_name(id))
    }

    /// Every request seen, in order.
    pub fn seen(&self) -> Vec<Seen> {
        self.shared.lock().seen.clone()
    }

    /// The verbs seen, each with its lease when it names one: `kill-uid 7.0`.
    pub fn verbs(&self) -> Vec<String> {
        let named = |one: &Seen| match one.lease.is_empty() {
            true => one.verb.clone(),
            false => format!("{} {}", one.verb, one.lease),
        };
        self.seen().iter().map(named).collect()
    }

    /// The `run` of `lease`.
    pub fn run_of(&self, lease: &str) -> Seen {
        let seen = self.seen();
        let found = seen
            .iter()
            .find(|one| one.verb == "run" && one.lease == lease);
        found.expect("no such run").clone()
    }

    /// From now on refuses `verb` with `reason`.
    pub fn refuse(&self, verb: &str, reason: &str) {
        let mut state = self.shared.lock();
        state.refused.insert(verb.to_owned(), reason.to_owned());
    }

    /// From now on answers `verb` again.
    pub fn allow(&self, verb: &str) {
        self.shared.lock().refused.remove(verb);
    }

    /// Every `run` from now on writes `out` and `err` to the job's standard streams.
    pub fn print(&self, out: &[u8], err: &[u8]) {
        self.shared.lock().prints = (out.to_vec(), err.to_vec());
    }

    /// Whether `user-create` makes the user's home (it does by default).
    pub fn make_homes(&self, yes: bool) {
        self.shared.lock().skip_homes = !yes;
    }

    /// The run of `lease` ends as `end` says.
    pub fn end(&self, lease: &str, end: End) {
        self.shared.lock().ends.insert(lease.to_owned(), end);
        self.shared.changed.notify_all();
    }

    /// The run of `lease` exits with `code`.
    pub fn exit(&self, lease: &str, code: i32) {
        self.end(lease, End::Exit(Some(code), None));
    }

    /// From now on a `run` starts its program for real, as the user running the test, and
    /// reports how it ended.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn real_runs(&self) {
        self.shared.lock().real = true;
    }
}

impl Drop for FakeHelper {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

fn reply(stream: &UnixStream, answer: &Reply) {
    proto::send(stream.as_fd(), answer, &[]).unwrap();
}

fn refused(stream: &UnixStream, reason: String) {
    reply(stream, &Reply::Refused { reason });
}

/// What a `run` asks: the lease, the argument vector and the environment.
type RunAsk = (String, Vec<String>, Vec<(String, String)>);

/// One connection: the helper's hello, one request, its answer.
fn serve(stream: &UnixStream, shared: &Shared) {
    let hello = Reply::Hello {
        nonce: "n".to_owned(),
    };
    reply(stream, &hello);
    let Some((call, fds)) = proto::recv::<Call>(stream.as_fd(), RUN_FDS).unwrap() else {
        return;
    };
    assert_eq!(call.nonce, "n");
    let seen = |verb: &str, lease: &str| Seen {
        verb: verb.to_owned(),
        lease: lease.to_owned(),
        ..Seen::default()
    };
    match call.request {
        Request::BootReset { .. } => match shared.take(seen("boot-reset", "")) {
            Some(reason) => refused(stream, reason),
            None => reply(
                stream,
                &Reply::Reset {
                    kept: None,
                    deleted: Vec::new(),
                },
            ),
        },
        Request::UserCreate { lease, .. } => match shared.take(seen("user-create", &lease)) {
            Some(reason) => refused(stream, reason),
            None => {
                if !shared.lock().skip_homes {
                    let id = parse_lease(&lease).unwrap();
                    fs::create_dir(shared.homes.join(user_name(id))).unwrap();
                }
                let uid = rustix::process::getuid().as_raw();
                reply(stream, &Reply::Created { uid });
            }
        },
        Request::Run {
            lease, argv, env, ..
        } => run(stream, shared, (lease, argv, env), fds),
        Request::KillUid { lease } => match shared.take(seen("kill-uid", &lease)) {
            Some(reason) => refused(stream, reason),
            None => {
                shared.lock().killed.insert(lease);
                shared.changed.notify_all();
                reply(stream, &Reply::Killed);
            }
        },
        Request::UserDelete { lease } => match shared.take(seen("user-delete", &lease)) {
            Some(reason) => refused(stream, reason),
            None => {
                let id = parse_lease(&lease).unwrap();
                let existed = fs::remove_dir_all(shared.homes.join(user_name(id))).is_ok();
                reply(stream, &Reply::Deleted { existed });
            }
        },
        other => panic!("the fake helper does not answer {other:?}"),
    }
}

/// A `run`: the descriptors are looked at, the scripted output is written, and the process
/// runs until a test ends it (or, for a real run, until it exits).
fn run(stream: &UnixStream, shared: &Shared, ask: RunAsk, fds: Vec<OwnedFd>) {
    let (lease, argv, env) = ask;
    let [stdin, stdout, stderr, cwd]: [OwnedFd; 4] = fds.try_into().expect("four descriptors");
    let mut held = Vec::new();
    let mut input = File::from(stdin.try_clone().unwrap());
    input.read_to_end(&mut held).unwrap();
    let cwd_is_dir = File::from(cwd.try_clone().unwrap())
        .metadata()
        .unwrap()
        .is_dir();
    let one = Seen {
        verb: "run".to_owned(),
        lease: lease.clone(),
        argv: argv.clone(),
        env: env.clone(),
        cwd_is_dir,
        stdin_bytes: held.len(),
    };
    if let Some(reason) = shared.take(one) {
        return refused(stream, reason);
    }
    let (real, (out, err)) = {
        let state = shared.lock();
        (state.real, state.prints.clone())
    };
    if real {
        return real_run(stream, (lease, argv, env), [stdin, stdout, stderr, cwd]);
    }
    File::from(stdout).write_all(&out).unwrap();
    File::from(stderr).write_all(&err).unwrap();
    reply(stream, &Reply::Started { pid: 4242 });
    if let End::Exit(code, signal) = wait_for_end(shared, &lease) {
        reply(stream, &Reply::Exited { code, signal });
    }
}

/// Waits until a test ends the run of `lease` or a kill does.
fn wait_for_end(shared: &Shared, lease: &str) -> End {
    let mut state = shared.lock();
    loop {
        if let Some(end) = state.ends.get(lease) {
            return *end;
        }
        if state.killed.contains(lease) {
            return End::Exit(None, Some(9));
        }
        state = shared
            .changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

#[cfg(target_os = "macos")]
fn real_run(stream: &UnixStream, ask: RunAsk, [stdin, stdout, stderr, cwd]: [OwnedFd; 4]) {
    use std::os::unix::process::ExitStatusExt as _;

    use js_mac_session::proto::Session;
    use js_mac_session::spawn::{self, Spawn};

    let (lease, argv, env) = ask;
    let name = user_name(parse_lease(&lease).unwrap());
    let child = spawn::spawn(Spawn {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        home: std::env::temp_dir().join(&name),
        user: name,
        argv,
        env,
        stdin,
        stdout,
        stderr,
        cwd,
        session: Session::Background,
    });
    let mut child = match child {
        Ok(child) => child,
        Err(why) => return refused(stream, format!("starting: {why}")),
    };
    reply(stream, &Reply::Started { pid: child.id() });
    let status = child.wait().unwrap();
    let (code, signal) = (status.code(), status.signal());
    reply(stream, &Reply::Exited { code, signal });
}

#[cfg(not(target_os = "macos"))]
fn real_run(_: &UnixStream, _: RunAsk, _: [OwnedFd; 4]) {
    panic!("real runs are for a Mac");
}
