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
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use js_mac_session::client::Client;
use js_mac_session::lease::{parse_lease, user_name};
use js_mac_session::proto::{self, Call, Limits, RUN_FDS, Reply, Request};

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
    /// For `run`: the limits the request carried, if any.
    pub limits: Option<Limits>,
    /// For `run`: whether the fourth descriptor (the working directory) is a directory.
    pub cwd_is_dir: bool,
    /// For `run`: the device and inode of the fourth descriptor, the directory the process
    /// starts in.
    pub cwd_id: Option<(u64, u64)>,
    /// For `run`: how many bytes the first descriptor (standard input) holds.
    pub stdin_bytes: usize,
    /// For `user-create`: whether the watched journal named the lease when the request
    /// arrived.
    pub journaled: bool,
    /// For `run`: the deadline the request carried, in milliseconds after the start. For
    /// `stop`: the grace, in milliseconds.
    pub millis: Option<u64>,
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
    /// The journal to look in when a `user-create` arrives.
    journal: Option<PathBuf>,
    /// The file a `user-delete` reads before it deletes, and what it read there (`None`
    /// inside when the file was not there).
    read_at_delete: Option<(PathBuf, Option<Option<Vec<u8>>>)>,
    /// The fake's clock, in milliseconds, which only a test moves.
    clock_ms: u64,
    /// The pid of each lease's run.
    pids: BTreeMap<String, u32>,
    /// When the run of a lease is due to be ended by its own deadline, on the fake's clock.
    deadlines: BTreeMap<String, u64>,
    /// The leases whose run got a `SIGTERM` from a `stop`.
    termed: BTreeSet<String>,
    /// Whether a run ends on its `SIGTERM` (it does unless a test says otherwise).
    ignores_term: bool,
    /// What the fake did to runs, in order: `term <lease>`, `kill <lease>` (a stop past its
    /// grace) and `deadline <lease>` (a deadline that passed).
    signals: Vec<String>,
    /// Whether the fake is a helper from before `watch`, `stop` and the deadline: it reads
    /// each request as such a helper does, and refuses one it cannot read.
    old: bool,
    /// A journal a `run` turns into a directory before it starts, so nothing can be added
    /// to it any more.
    break_at_run: Option<PathBuf>,
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

    /// Whether the watched journal holds `+ lease`.
    fn journal_names(&self, lease: &str) -> bool {
        let path = self.lock().journal.clone();
        let text = path.and_then(|path| fs::read_to_string(path).ok());
        text.is_some_and(|text| text.lines().any(|line| line == format!("+ {lease}")))
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
        // A short path of its own: a socket path is limited to about a hundred bytes, which
        // a checkout's path and a test's directory may use up. It is below the target
        // directory, never `/tmp`, which a profile cannot name (it is `/private/tmp`).
        let folder = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-agent")
            .join(format!(
                "s{}-{}",
                std::process::id(),
                SOCKETS.fetch_add(1, Ordering::SeqCst)
            ));
        fs::create_dir_all(&folder).unwrap();
        let socket = folder.join("k");
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

    /// From now on a `user-create` notes whether the journal at `path` names its lease.
    pub fn watch_journal(&self, path: &Path) {
        self.shared.lock().journal = Some(path.to_path_buf());
    }

    /// From now on a `user-delete` reads the file `path` before it deletes the user.
    pub fn read_at_delete(&self, path: &Path) {
        self.shared.lock().read_at_delete = Some((path.to_path_buf(), None));
    }

    /// What the last `user-delete` read at the path [`FakeHelper::read_at_delete`] named:
    /// `None` when no `user-delete` came, `Some(None)` when the file was not there.
    pub fn read_when_deleted(&self) -> Option<Option<Vec<u8>>> {
        let state = self.shared.lock();
        state
            .read_at_delete
            .as_ref()
            .and_then(|(_, read)| read.clone())
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

    /// Moves the fake's clock on by `ms`: a deadline or a grace it passes takes effect.
    pub fn advance(&self, ms: u64) {
        self.shared.lock().clock_ms += ms;
        self.shared.changed.notify_all();
    }

    /// From now on a run does not end on the `SIGTERM` of a `stop`.
    pub fn ignore_term(&self) {
        self.shared.lock().ignores_term = true;
    }

    /// What the fake did to runs so far: `term <lease>`, `kill <lease>`, `deadline <lease>`.
    pub fn signals(&self) -> Vec<String> {
        self.shared.lock().signals.clone()
    }

    /// From now on a `run` turns the journal at `path` into a directory first: the runtime
    /// cannot record the run.
    pub fn break_journal_at_run(&self, path: &Path) {
        self.shared.lock().break_at_run = Some(path.to_path_buf());
    }

    /// From now on the fake is a helper from before `watch`, `stop` and the deadline.
    pub fn be_old(&self) {
        self.shared.lock().old = true;
    }

    /// Forgets every run, as a helper that restarted has: `watch` then finds none.
    pub fn forget_runs(&self) {
        self.shared.lock().pids.clear();
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
        if let Some(folder) = self.socket.parent() {
            let _ = fs::remove_dir(folder);
        }
    }
}

fn reply(stream: &UnixStream, answer: &Reply) {
    proto::send(stream.as_fd(), answer, &[]).unwrap();
}

fn refused(stream: &UnixStream, reason: String) {
    reply(stream, &Reply::Refused { reason });
}

/// What a `run` asks: the lease, the argument vector, the environment and the limits.
type RunAsk = (String, Vec<String>, Vec<(String, String)>, Option<Limits>);

/// One connection: the helper's hello, one request, its answer.
fn serve(stream: &UnixStream, shared: &Shared) {
    let hello = Reply::Hello {
        nonce: "n".to_owned(),
    };
    reply(stream, &hello);
    let Some((value, fds)) = proto::recv::<serde_json::Value>(stream.as_fd(), RUN_FDS).unwrap()
    else {
        return;
    };
    if shared.lock().old {
        // As a helper from before the new verbs and fields reads a request: the old shape,
        // unknown fields refused, the refusal the real server gives a request it cannot read.
        if let Err(why) = serde_json::from_value::<old::Call>(value.clone()) {
            shared.take(Seen {
                verb: "unreadable".to_owned(),
                ..Seen::default()
            });
            return refused(stream, format!("bad request: {why}"));
        }
    }
    let call: Call = serde_json::from_value(value).unwrap();
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
        Request::UserCreate { lease, .. } => match shared.take(Seen {
            journaled: shared.journal_names(&lease),
            ..seen("user-create", &lease)
        }) {
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
            lease,
            argv,
            env,
            limits,
            deadline_ms,
            ..
        } => run(stream, shared, (lease, argv, env, limits), fds, deadline_ms),
        Request::Watch { lease } => watch(stream, shared, &lease),
        Request::Stop { lease, grace_ms } => stop(stream, shared, &lease, grace_ms),
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
                if let Some((path, read)) = shared.lock().read_at_delete.as_mut() {
                    *read = Some(fs::read(&*path).ok());
                }
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
fn run(
    stream: &UnixStream,
    shared: &Shared,
    ask: RunAsk,
    fds: Vec<OwnedFd>,
    deadline_ms: Option<u64>,
) {
    let (lease, argv, env, limits) = ask;
    let [stdin, stdout, stderr, cwd]: [OwnedFd; 4] = fds.try_into().expect("four descriptors");
    let mut held = Vec::new();
    let mut input = File::from(stdin.try_clone().unwrap());
    input.read_to_end(&mut held).unwrap();
    let cwd_meta = File::from(cwd.try_clone().unwrap()).metadata().unwrap();
    let cwd_is_dir = cwd_meta.is_dir();
    let one = Seen {
        verb: "run".to_owned(),
        lease: lease.clone(),
        argv: argv.clone(),
        env: env.clone(),
        limits,
        cwd_is_dir,
        cwd_id: Some((cwd_meta.dev(), cwd_meta.ino())),
        stdin_bytes: held.len(),
        millis: deadline_ms,
        ..Seen::default()
    };
    if let Some(reason) = shared.take(one) {
        return refused(stream, reason);
    }
    {
        let mut state = shared.lock();
        if let Some(journal) = state.break_at_run.take() {
            fs::remove_file(&journal).unwrap();
            fs::create_dir(&journal).unwrap();
        }
        let pid = 4242 + u32::try_from(state.pids.len()).unwrap();
        state.pids.insert(lease.clone(), pid);
        if let Some(after) = deadline_ms {
            let due = state.clock_ms + after;
            state.deadlines.insert(lease.clone(), due);
        }
    }
    let (real, (out, err)) = {
        let state = shared.lock();
        (state.real, state.prints.clone())
    };
    if real {
        return real_run(
            stream,
            (lease, argv, env, limits),
            [stdin, stdout, stderr, cwd],
        );
    }
    File::from(stdout).write_all(&out).unwrap();
    File::from(stderr).write_all(&err).unwrap();
    let pid = shared.lock().pids[&lease];
    reply(stream, &Reply::Started { pid });
    if let End::Exit(code, signal) = wait_for_end(shared, &lease) {
        // The agent that asked may be gone: its connection is then closed.
        let _ = proto::send(stream.as_fd(), &Reply::Exited { code, signal }, &[]);
    }
}

/// `watch`: the run of `lease` found again, answered as `run` is.
fn watch(stream: &UnixStream, shared: &Shared, lease: &str) {
    let seen = Seen {
        verb: "watch".to_owned(),
        lease: lease.to_owned(),
        ..Seen::default()
    };
    if let Some(reason) = shared.take(seen) {
        return refused(stream, reason);
    }
    let Some(pid) = shared.lock().pids.get(lease).copied() else {
        return refused(
            stream,
            format!("lease {lease} has no run this helper started"),
        );
    };
    reply(stream, &Reply::Started { pid });
    if let End::Exit(code, signal) = wait_for_end(shared, lease) {
        let _ = proto::send(stream.as_fd(), &Reply::Exited { code, signal }, &[]);
    }
}

/// `stop`: `SIGTERM`, then a kill once the fake's clock is `grace_ms` past the stop, unless
/// the run ended first.
fn stop(stream: &UnixStream, shared: &Shared, lease: &str, grace_ms: u64) {
    let seen = Seen {
        verb: "stop".to_owned(),
        lease: lease.to_owned(),
        millis: Some(grace_ms),
        ..Seen::default()
    };
    if let Some(reason) = shared.take(seen) {
        return refused(stream, reason);
    }
    let mut state = shared.lock();
    let until = state.clock_ms + grace_ms;
    state.termed.insert(lease.to_owned());
    state.signals.push(format!("term {lease}"));
    shared.changed.notify_all();
    loop {
        let ended = state.ends.contains_key(lease)
            || state.killed.contains(lease)
            || (state.termed.contains(lease) && !state.ignores_term);
        if ended {
            break;
        }
        if state.clock_ms >= until {
            state.signals.push(format!("kill {lease}"));
            state.killed.insert(lease.to_owned());
            shared.changed.notify_all();
            break;
        }
        state = shared
            .changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner);
    }
    drop(state);
    reply(stream, &Reply::Killed);
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
        if state.termed.contains(lease) && !state.ignores_term {
            return End::Exit(None, Some(15));
        }
        if state
            .deadlines
            .get(lease)
            .is_some_and(|due| state.clock_ms >= *due)
        {
            state.signals.push(format!("deadline {lease}"));
            state.killed.insert(lease.to_owned());
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

    let (lease, argv, env, limits) = ask;
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
        limits,
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

/// The helper's requests as a helper from before `watch`, `stop` and `run`'s deadline reads
/// them: the same shape less those, unknown fields refused.
mod old {
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Call {
        #[allow(dead_code, reason = "read only to check the shape")]
        nonce: String,
        #[allow(dead_code, reason = "read only to check the shape")]
        request: Request,
    }

    #[derive(Deserialize)]
    #[serde(tag = "verb", rename_all = "kebab-case", deny_unknown_fields)]
    #[allow(dead_code, reason = "read only to check the shape")]
    pub enum Request {
        UserCreate {
            lease: String,
            #[serde(default)]
            grant: Option<String>,
        },
        Run {
            lease: String,
            argv: Vec<String>,
            #[serde(default)]
            env: Vec<(String, String)>,
            #[serde(default)]
            session: Option<String>,
            #[serde(default)]
            limits: Option<serde_json::Value>,
        },
        KillUid {
            lease: String,
        },
        UserDelete {
            lease: String,
        },
        BootReset {
            #[serde(default)]
            held: Vec<String>,
        },
    }
}
