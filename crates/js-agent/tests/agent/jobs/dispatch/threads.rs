//! The poller on its own thread, the session telling it each welcome, and the whole agent
//! polling through a connector of its own. The fake transport hands every poll to the test,
//! which answers it, so the poller runs as fast as the test and no faster; every wait on
//! another thread is for an event with a hang guard.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use js_agent::dispatch::{Cell, Dial, serve};
use js_agent::{
    Answer, Connector, Drain, Handle, Login, Session, Sleeper, Transport, Wake, Worker,
};
use js_agent_wire::{Command, DISPATCH_PATH, Dispatched, HeartbeatReply};
use js_client::{ClientError, HostIdentity};
use serde_json::{Value, json};

use super::{GUARD, acked, handed, quiet, spec, welcome_polling, welcomed};
use crate::jobs::drain::container_flags;
use crate::jobs::fake::FakeMachine;
use crate::jobs::start;
use crate::support::{
    Lowest, Reply, SECRET, Scratch, Script, ScriptConnector, beat_ok, flags, hello, limited,
    linux_host, lowest, refused, reply, run_on_polling, welcome,
};

/// The drain the sessions of these tests carry; none of them drains but one, by hand.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// What the poller's connector and its transports share with the test: every poll is handed
/// to the test, which answers it, so the poller runs as fast as the test and no faster.
struct Rig {
    polls: Mutex<Sender<Value>>,
    replies: Mutex<Receiver<Reply>>,
    connects: AtomicUsize,
}

/// The connector the poller makes its transports with.
struct RigConnector(Arc<Rig>);

struct RigTransport {
    rig: Arc<Rig>,
}

impl Connector for RigConnector {
    fn connect(
        &self,
        _server: &str,
        _login: &Login,
        _ca_cert: Option<&std::path::Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        self.0.connects.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(RigTransport {
            rig: self.0.clone(),
        }))
    }
}

impl Transport for RigTransport {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        assert_eq!(path, DISPATCH_PATH);
        let polls = self
            .rig
            .polls
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        polls
            .send(body.clone())
            .map_err(|_| "the test is gone".to_owned())?;
        drop(polls);
        let replies = self
            .rig
            .replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        replies
            .recv_timeout(GUARD)
            .map_err(|_| "no answer".to_owned())?
    }
}

/// The test's end of a [`Rig`]: the polls that arrive, and the answers it gives.
struct Driver {
    polls: Receiver<Value>,
    replies: Sender<Reply>,
}

impl Driver {
    /// The next poll.
    fn next(&self) -> Value {
        self.polls.recv_timeout(GUARD).expect("the poller polled")
    }

    fn answer(&self, reply: Reply) {
        self.replies.send(reply).expect("the poller waits");
    }

    /// Answers nothing more: a poll in flight, and every later one, fails at once. Returns
    /// the polls that arrive.
    fn finish(self) -> Receiver<Value> {
        self.polls
    }
}

fn rig() -> (RigConnector, Driver) {
    let (poll_tx, poll_rx) = channel();
    let (reply_tx, reply_rx) = channel();
    let rig = Rig {
        polls: Mutex::new(poll_tx),
        replies: Mutex::new(reply_rx),
        connects: AtomicUsize::new(0),
    };
    let driver = Driver {
        polls: poll_rx,
        replies: reply_tx,
    };
    (RigConnector(Arc::new(rig)), driver)
}

/// Closes a cell when dropped, so a failing test does not leave the poller waiting for ever.
struct Closing<'a>(&'a Cell);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Runs the poller of `worker` on a thread over `connector` while `body` runs, and closes
/// the cell after it.
fn serving<R>(
    cell: &Cell,
    worker: &Worker,
    connector: &RigConnector,
    body: impl FnOnce() -> R,
) -> R {
    let login = Login::HostCertificate(HostIdentity::new(
        "/nonexistent/host.crt",
        "/nonexistent/host.key",
    ));
    let dial = Dial {
        connector,
        server: "http://127.0.0.1:1",
        login: &login,
        ca_cert: None,
    };
    thread::scope(|scope| {
        scope.spawn(|| serve(cell, &dial, worker, Box::new(Lowest)));
        let _closing = Closing(cell);
        body()
    })
}

/// Asserts that the poller polls only once a welcome says `dispatch_poll`; polls again at once
/// after every answer; uses the session of the welcome that replaced the one before (the poll
/// in flight when it arrives is answered, the next names the new epoch and session);
/// and stops for a welcome without the flag. Catches a poller that keeps the first welcome's
/// session after a second hello, one that polls before any welcome or after the flag is
/// withdrawn, and one that waits between answered polls.
#[test]
fn the_poller_follows_each_welcome_and_stops_when_the_flag_is_withdrawn() {
    let dir = Scratch::new("jobs-dispatch-serve");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let cell = Cell::new();
    let (connector, driver) = rig();

    let arrived = serving(&cell, &worker, &connector, || {
        let polling = |epoch: u64, session: &str, polling: bool| {
            cell.publish("h1", &welcomed(epoch, session, polling));
        };
        polling(1, "session-one", true);
        let first = driver.next();
        assert_eq!(first["epoch"], 1);
        assert_eq!(first["session"], "session-one");
        assert_eq!(first["host_id"], "h1");
        polling(2, "session-two", true);
        driver.answer(quiet());
        let second = driver.next();
        assert_eq!(second["epoch"], 2);
        assert_eq!(second["session"], "session-two");
        polling(3, "session-three", false);
        driver.answer(quiet());
        driver.finish()
    });

    assert!(arrived.try_recv().is_err(), "the flag was withdrawn");
    assert_eq!(connector.0.connects.load(Ordering::SeqCst), 2);
}

/// Asserts that a 409 from one instance does not end the polls of the session: the poller
/// backs off and polls again with the same route (epoch and session) and the acknowledgements
/// it holds, and a later 200 is taken as usual. Catches a poller that stops for the rest of
/// the session on a refusal while the heartbeats succeed, which leaves the host without the
/// starts the servers leave out of heartbeat replies.
#[test]
fn a_refused_poll_is_polled_again_with_the_same_session() {
    let dir = Scratch::new("jobs-dispatch-refused");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let cell = Cell::new();
    let (connector, driver) = rig();

    serving(&cell, &worker, &connector, || {
        cell.publish("h1", &welcomed(1, "one", true));
        driver.next();
        driver.answer(handed(vec![Dispatched::start(spec("j1"))]));
        let carrying = driver.next();
        assert_eq!(acked(&carrying), ["start:alloc-j1"]);
        driver.answer(refused(409, "dispatch-not-served"));
        let after = driver.next();
        assert_eq!(after["epoch"], 1);
        assert_eq!(after["session"], "one");
        assert_eq!(acked(&after), ["start:alloc-j1"], "kept across the refusal");
        driver.finish();
    });
}

/// Asserts that a failed poll pauses, that a welcome cuts the pause (the next poll names the
/// new session at once, though the server asked for a minute), and that the close cuts a pause
/// too, so the thread ends while the agent does. Catches a backoff that holds a new session
/// off for the length of the old one's wait, and a thread the end of the agent has to wait for.
#[test]
fn a_pause_ends_at_a_new_welcome_and_at_the_end_of_the_agent() {
    let dir = Scratch::new("jobs-dispatch-pause");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let cell = Cell::new();
    let (connector, driver) = rig();
    let (done, finished) = channel();

    let login = Login::HostCertificate(HostIdentity::new(
        "/nonexistent/host.crt",
        "/nonexistent/host.key",
    ));
    let dial = Dial {
        connector: &connector,
        server: "http://127.0.0.1:1",
        login: &login,
        ca_cert: None,
    };
    thread::scope(|scope| {
        scope.spawn(|| {
            serve(&cell, &dial, &worker, Box::new(Lowest));
            let _ = done.send(());
        });
        let _closing = Closing(&cell);
        cell.publish("h1", &welcomed(1, "one", true));
        driver.next();
        driver.answer(limited(429, 60));
        cell.publish("h1", &welcomed(2, "two", true));
        let next = driver.next();
        assert_eq!(next["epoch"], 2);
        driver.answer(limited(429, 60));
        cell.close();
        finished
            .recv_timeout(GUARD)
            .expect("the close ended the pause");
    });
}

/// Asserts that a connector that cannot make a transport is a failed poll that waits, not the
/// end of the poller: the poller tried, and ends only at the close, which cuts its wait.
/// Catches a poller that panics or exits when its connection cannot be made.
#[test]
fn a_transport_that_cannot_be_made_is_a_failed_poll() {
    struct Refusing(Mutex<Sender<()>>);
    impl Connector for Refusing {
        fn connect(
            &self,
            _server: &str,
            _login: &Login,
            _ca_cert: Option<&std::path::Path>,
        ) -> Result<Box<dyn Transport>, ClientError> {
            let _ = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(());
            Err(ClientError::Server("refused".to_owned()))
        }
    }
    let dir = Scratch::new("jobs-dispatch-refusing");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let cell = Cell::new();
    let (tried, connecting) = channel();
    let refusing = Refusing(Mutex::new(tried));
    let login = Login::HostCertificate(HostIdentity::new(
        "/nonexistent/host.crt",
        "/nonexistent/host.key",
    ));
    let dial = Dial {
        connector: &refusing,
        server: "http://127.0.0.1:1",
        login: &login,
        ca_cert: None,
    };
    let (done, finished) = channel();

    thread::scope(|scope| {
        scope.spawn(|| {
            serve(&cell, &dial, &worker, Box::new(Lowest));
            let _ = done.send(());
        });
        let _closing = Closing(&cell);
        cell.publish("h1", &welcomed(1, "one", true));
        connecting
            .recv_timeout(GUARD)
            .expect("the poller tried to connect");
        cell.close();
        finished.recv_timeout(GUARD).expect("the poller ended");
    });
}

/// Asserts that the poller's thread holds an answer its worker has no room for, makes no
/// transport and posts nothing while the queue stays full, and once the worker makes room
/// hands the start over and posts the next poll at once with its acknowledgement; and that
/// the poller's lines reach the cell. Catches a thread that polls on while it holds messages
/// (it would make a transport for every try), and one that waits out a bound after the room
/// appeared instead of the room itself.
#[test]
fn the_poller_thread_holds_what_the_worker_has_no_room_for_until_it_makes_room() {
    let dir = Scratch::new("jobs-dispatch-serve-held");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    super::held::fill(&worker, &machine);
    let cell = Cell::new();
    let (connector, driver) = rig();

    serving(&cell, &worker, &connector, || {
        cell.publish("h1", &welcomed(1, "one", true));
        driver.next();
        driver.answer(handed(vec![Dispatched::start(spec("j1"))]));
        assert_eq!(
            cell.wait_lines(GUARD),
            ["dispatch: the job worker has no room for 1 message(s) (undelivered answers: 1)"]
        );
        assert_eq!(connector.0.connects.load(Ordering::SeqCst), 1);
        machine.world.release();
        let carrying = driver.next();
        assert_eq!(acked(&carrying), ["start:alloc-j1"]);
        assert_eq!(connector.0.connects.load(Ordering::SeqCst), 2);
        driver.answer(quiet());
        assert_eq!(
            cell.wait_lines(GUARD),
            ["dispatch: the job worker took 1 held message(s) 0 ms after they arrived"]
        );
        driver.finish();
    });
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().jobs[0].job_id, "j1");
}

/// A session over a scripted transport and `worker`, telling `cell` its welcomes.
fn session_over<'a>(script: &'a mut Script, worker: &'a Worker, cell: &'a Cell) -> Session<'a> {
    Session::new(hello("h1"), script, lowest())
        .with_jobs(worker, DRAIN)
        .with_dispatch(cell)
}

/// Asserts that a session tells its cell every welcome: the route (host, epoch and session) of
/// one that says `dispatch_poll`, none for one that does not, and that a drain closes the cell.
/// Catches a session that never publishes, one that publishes the session of the welcome
/// before, and a drain that leaves the poller taking starts the worker will not act on.
#[test]
fn a_session_tells_its_cell_each_welcome_and_closes_it_at_a_drain() {
    let dir = Scratch::new("jobs-dispatch-session");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let cell = Cell::new();
    let (mut script, _) = Script::new(vec![
        welcome_polling(1),
        refused(409, "stale-epoch"),
        welcome(2),
        refused(409, "stale-epoch"),
        welcome_polling(3),
    ]);
    let mut session = session_over(&mut script, &worker, &cell);

    session.tick();
    let (generation, route) = cell.try_route().unwrap();
    assert_eq!(
        (generation, route.epoch, route.host_id.as_str()),
        (1, 1, "h1")
    );
    assert_eq!(route.session, SECRET);
    session.tick();
    session.tick();
    assert!(cell.try_route().is_none(), "a welcome without the flag");
    session.tick();
    session.tick();
    assert_eq!(cell.try_route().unwrap().1.epoch, 3);
    session.begin_drain();
    assert!(cell.wait_route().is_none(), "the drain closed the cell");
}

/// Asserts that a session with no cell says nothing to a poller and works as it did. Catches a
/// session that needs a cell.
#[test]
fn a_session_without_a_cell_says_nothing_to_a_poller() {
    let dir = Scratch::new("jobs-dispatch-session-bare");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    let (mut script, _) = Script::new(vec![welcome_polling(1)]);
    let mut session = Session::new(hello("h1"), &mut script, lowest()).with_jobs(&worker, DRAIN);
    session.tick();
    session.begin_drain();
}

/// A sleeper that waits for the test's go at its first wait and ends the agent there and at
/// every wait after it.
struct Parked {
    go: Receiver<()>,
    released: bool,
}

impl Sleeper for Parked {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        if !self.released {
            self.go.recv_timeout(GUARD).expect("the test sent go");
            self.released = true;
        }
        Wake::Stop
    }
}

/// A sleeper that lets the worker finish what it was handed before each wait, and ends the
/// agent at its `stop_at`th wait.
struct Settling {
    handles: Receiver<Handle>,
    handle: Option<Handle>,
    stop_at: usize,
    waits: usize,
}

impl Sleeper for Settling {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.handle = self.handle.take().or_else(|| self.handles.try_recv().ok());
        if let Some(handle) = &self.handle {
            assert!(handle.settle());
        }
        self.waits += 1;
        match self.waits >= self.stop_at {
            true => Wake::Stop,
            false => Wake::Elapsed,
        }
    }
}

/// Proves, through the whole agent, that a welcome which says `dispatch_poll` makes the agent
/// poll on a connection of its own while its heartbeat loop waits: the poll names the
/// welcome's host, epoch and session; a start in its answer is started, a start handed out
/// again is started once, and both are acknowledged by the next poll; a stop in an answer
/// stops the job and is acknowledged as a stop; and the agent ends, with the thread of the
/// poller, at a stop request. Catches a poller never started by the agent, one that polls with
/// anything but the welcome's session, a start that reaches the worker twice as two jobs, and
/// a poller thread that outlives the agent's loop.
#[test]
fn the_whole_agent_polls_while_it_heartbeats_and_ends_with_the_poller() {
    let dir = Scratch::new("jobs-dispatch-agent");
    let owned = container_flags(&dir);
    let extra: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&extra);
    let (script, sent) = Script::new(vec![welcome_polling(1), beat_ok(), beat_ok(), beat_ok()]);
    let connector = ScriptConnector::of(script);
    let (polling, driver) = rig();
    let (go, parked) = channel();
    let mut sleeper = Parked {
        go: parked,
        released: false,
    };
    let (handles, handle_rx) = channel::<Handle>();
    let hook = move |handle: Handle| {
        let _ = handles.send(handle);
    };
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let jobs: crate::support::Jobs<'_> = (machine, Some(&hook));

    let (ran, seen) = thread::scope(|scope| {
        let driver = scope.spawn(move || {
            let handle = handle_rx.recv_timeout(GUARD).expect("the worker started");
            let mut bodies = Vec::new();
            let again = Dispatched::start(spec("j1"));
            let stop = Dispatched::stop("j1".to_owned(), "alloc-j1".to_owned());
            let mut seen_created = 0;
            for answer in [vec![again.clone()], vec![again], vec![stop], vec![]] {
                bodies.push(driver.next());
                assert!(handle.settle());
                seen_created = world.created().len();
                driver.answer(handed(answer));
            }
            bodies.push(driver.next());
            assert!(handle.settle());
            go.send(()).unwrap();
            driver.answer(quiet());
            (bodies, seen_created, world.log())
        });
        let ran = run_on_polling(
            &args,
            &linux_host(),
            (&connector, &polling),
            &mut sleeper,
            jobs,
        );
        (ran, driver.join().unwrap())
    });

    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    let (bodies, created, log) = seen;
    let host = sent.borrow()[0].1["host_id"].clone();
    let first = json!({ "host_id": host, "epoch": 1, "session": SECRET });
    assert_eq!(bodies[0], first);
    assert_eq!(acked(&bodies[1]), ["start:alloc-j1"]);
    assert_eq!(acked(&bodies[2]), ["start:alloc-j1"], "the redelivery");
    assert_eq!(acked(&bodies[3]), ["stop:alloc-j1"]);
    assert_eq!(acked(&bodies[4]), Vec::<String>::new());
    assert_eq!(created, 1, "{log:?}");
    assert!(log.contains(&"stop j1".to_owned()), "{log:?}");
    assert!(sent.borrow().iter().all(|(path, _)| path != DISPATCH_PATH));
}

/// Proves that an agent whose welcome does not say `dispatch_poll` (a server from before the
/// poll, or one that does not serve it to this host) never makes a connection for a poll and
/// starts the job a heartbeat's reply carries, as it did. Catches an agent that polls a server
/// that never offered, and one that stops taking starts from the heartbeat because it can
/// poll.
#[test]
fn a_welcome_without_dispatch_poll_leaves_the_starts_in_the_heartbeat_reply() {
    let dir = Scratch::new("jobs-dispatch-fallback");
    let owned = container_flags(&dir);
    let extra: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&extra);
    let starting = HeartbeatReply {
        commands: vec![Command::JobStart(Box::new(spec("j1")))],
        ..HeartbeatReply::default()
    };
    let replies = vec![
        welcome(1),
        reply(200, serde_json::to_value(starting).unwrap()),
        beat_ok(),
        beat_ok(),
    ];
    let (script, sent) = Script::new(replies);
    let connector = ScriptConnector::of(script);
    let (polling, driver) = rig();
    let (handles, handle_rx) = channel::<Handle>();
    let hook = move |handle: Handle| {
        let _ = handles.send(handle);
    };
    let mut sleeper = Settling {
        handles: handle_rx,
        handle: None,
        stop_at: 3,
        waits: 0,
    };
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let jobs: crate::support::Jobs<'_> = (machine, Some(&hook));

    let ran = run_on_polling(
        &args,
        &linux_host(),
        (&connector, &polling),
        &mut sleeper,
        jobs,
    );

    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    assert_eq!(polling.0.connects.load(Ordering::SeqCst), 0);
    assert!(driver.polls.try_recv().is_err());
    assert_eq!(world.created().len(), 1, "{:?}", world.log());
    assert!(sent.borrow().iter().all(|(path, _)| path != DISPATCH_PATH));
}
