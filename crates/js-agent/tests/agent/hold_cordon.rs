//! The cordon of a held host: when the keeper asks the lease server, what it says, and how it
//! connects. The server is a fake that records each request with the time of a
//! clock the test moves by hand; the state root and the hold file are real files in a
//! scratch directory. No thread runs and no time passes.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;

use js_agent::Login;
use js_agent::hold_cordon::{
    Connect, HOLD_LEASE_SECS, HOLD_RENEW, HOLD_RETRY, Keeper, Seams, connect_http,
};
use js_agent::lifecycle::hold::{self, HOLD_FILE};
use js_agent::lifecycle::{Clock, StateRoot};
use js_agent::own_host::{
    CordonView, Cordoned, HostReading, HttpLease, Lease, LeaseError, RequestLimit, Uncordoned,
};
use js_client::HostIdentity;

use crate::host_certs::self_signed;
use crate::jobs::private;
use crate::support::Scratch;

/// The host the keeper cordons.
const HOST: &str = "h1";

/// What the keeper says when a hold file in the right form begins.
const HELD: &str =
    "hold: this host is held (its hold file is there): it starts no work and keeps itself cordoned";

/// What the keeper says when the cordon is placed (the fake grants the whole hour).
const CORDONED: &str = "hold: cordoned host=h1 expires_in_ms=3600000";

/// What the keeper says when the hold file goes.
const GONE: &str = "hold: the hold file is gone: the cordon of this host is no longer renewed";

/// What the lease error `UnknownHost` says.
const UNKNOWN: &str = "unknown host (409 unknown-host): the lease server has not heard this host say hello since it started";

/// The line of a cordon request that failed for `why`.
fn failed(why: &str) -> String {
    format!("hold: cannot cordon host h1: {why}; asked again in 15s or at the next hello")
}

/// A clock moved by hand, in milliseconds.
#[derive(Clone)]
struct ManualClock(Rc<Cell<u64>>);

impl ManualClock {
    fn set(&self, ms: u64) {
        self.0.set(ms);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// What a request does instead of the ordinary answer.
enum Reply {
    /// Answers as the server does.
    Ok,
    /// Fails with this error.
    Fail(LeaseError),
    /// Answers, naming this host.
    Name(&'static str),
}

/// What the fake server knows.
struct Inner {
    clock: Rc<Cell<u64>>,
    cordoned: bool,
    script: VecDeque<Reply>,
    /// The clock and `lease_secs` of each cordon request, in order.
    asks: Vec<(u64, u64)>,
    uncordons: u32,
}

/// The lease server's own-host routes in memory.
#[derive(Clone)]
struct Server(Rc<RefCell<Inner>>);

impl Server {
    fn new(clock: &Rc<Cell<u64>>) -> Self {
        Self(Rc::new(RefCell::new(Inner {
            clock: Rc::clone(clock),
            cordoned: false,
            script: VecDeque::new(),
            asks: Vec::new(),
            uncordons: 0,
        })))
    }

    /// The next requests, one reply each.
    fn script(&self, replies: Vec<Reply>) {
        self.0.borrow_mut().script.extend(replies);
    }

    /// The cordon lapses.
    fn lapse(&self) {
        self.0.borrow_mut().cordoned = false;
    }

    fn asks(&self) -> Vec<(u64, u64)> {
        self.0.borrow().asks.clone()
    }

    /// The times of the cordon requests.
    fn times(&self) -> Vec<u64> {
        self.asks().iter().map(|(at, _)| *at).collect()
    }

    fn uncordons(&self) -> u32 {
        self.0.borrow().uncordons
    }

    fn reading(id: &str, cordoned: bool) -> HostReading {
        HostReading {
            id: id.to_owned(),
            state: "live".to_owned(),
            epoch: 1,
            session_started_ms: Some(1),
            leases: 0,
            lease: None,
            releasing: false,
            mark: None,
            cordon: cordoned.then_some(CordonView {
                expires_at_ms: 4_600_000,
                expires_in_ms: Some(HOLD_LEASE_SECS * 1000),
                holder: None,
            }),
            taint: None,
            agent_version: None,
            runtime: None,
        }
    }
}

impl Lease for Server {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        let mut inner = self.0.borrow_mut();
        let at = inner.clock.get();
        inner.asks.push((at, lease_secs));
        let id = match inner.script.pop_front().unwrap_or(Reply::Ok) {
            Reply::Ok => HOST,
            Reply::Fail(why) => return Err(why),
            Reply::Name(id) => id,
        };
        let already_cordoned = std::mem::replace(&mut inner.cordoned, true);
        Ok(Cordoned {
            already_cordoned,
            held_by_other: false,
            host: Self::reading(id, true),
        })
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        let mut inner = self.0.borrow_mut();
        inner.uncordons += 1;
        let lifted = std::mem::replace(&mut inner.cordoned, false);
        Ok(Uncordoned {
            lifted,
            held_by_other: false,
            host: Self::reading(HOST, false),
        })
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        Ok(Self::reading(HOST, self.0.borrow().cordoned))
    }
}

/// A state root, a fake server, a clock, and a connect that counts.
struct Rig {
    _scratch: Scratch,
    dir: PathBuf,
    root: StateRoot,
    owner: u32,
    clock: ManualClock,
    server: Server,
    /// How many times the keeper connected.
    connects: Rc<Cell<usize>>,
    /// Connecting fails while this is set.
    down: Rc<Cell<bool>>,
    connect: Box<Connect<'static>>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let dir = private(&scratch, "state");
        let root = StateRoot::open(&dir).unwrap();
        let owner = fs::metadata(&dir).unwrap().uid();
        let cell = Rc::new(Cell::new(0));
        let server = Server::new(&cell);
        let connects = Rc::new(Cell::new(0));
        let down = Rc::new(Cell::new(false));
        let connect: Box<Connect<'static>> = {
            let (server, connects, down) = (server.clone(), Rc::clone(&connects), Rc::clone(&down));
            Box::new(move || {
                connects.set(connects.get() + 1);
                if down.get() {
                    return Err("cannot connect".to_owned());
                }
                Ok(Box::new(server.clone()) as Box<dyn Lease>)
            })
        };
        Self {
            _scratch: scratch,
            dir,
            root,
            owner,
            clock: ManualClock(cell),
            server,
            connects,
            down,
            connect,
        }
    }

    fn keeper(&self) -> Keeper<'_> {
        Keeper::new(Seams {
            state: &self.root,
            owner: self.owner,
            host: HOST,
            connect: &*self.connect,
            clock: &self.clock,
        })
    }

    fn hold(&self) {
        hold::write(&self.root).unwrap();
    }

    fn unhold(&self) {
        fs::remove_file(self.dir.join(HOLD_FILE)).unwrap();
    }

    fn connected(&self) -> usize {
        self.connects.get()
    }
}

/// The ask is made as soon as a hello has been accepted, never before: with the host held and
/// time moving, steps that follow no hello connect to nothing and ask nothing and say only
/// that the hold began, once; the step of the first hello asks once, for the hour, and says
/// the cordon is placed with its remaining time, over one connection. It
/// catches a keeper that asks while the server cannot know the host, one that asks twice at a
/// hello, and one that says nothing of the hold.
#[test]
fn nothing_is_asked_before_the_first_hello_and_the_cordon_is_placed_at_it() {
    let rig = Rig::new("hold-cordon-first-hello");
    rig.hold();
    let mut keeper = rig.keeper();
    rig.clock.set(10);
    assert_eq!(keeper.step(false), vec![HELD.to_owned()]);
    for at in [1_000, 600_000, 7_200_000] {
        rig.clock.set(at);
        assert_eq!(keeper.step(false), Vec::<String>::new());
    }
    assert_eq!(rig.connected(), 0);
    assert!(rig.server.asks().is_empty());
    rig.clock.set(7_300_000);
    assert_eq!(keeper.step(true), vec![CORDONED.to_owned()]);
    assert_eq!(rig.server.asks(), vec![(7_300_000, 3600)]);
    assert_eq!(rig.connected(), 1);
}

/// The constants the lease and its renewals are asked with are the hour, a third of it, and
/// fifteen seconds. It catches a changed interval that no other test would name.
#[test]
fn the_lease_is_an_hour_renewed_every_third_and_retried_after_fifteen_seconds() {
    assert_eq!(HOLD_LEASE_SECS, 3600);
    assert_eq!(HOLD_RENEW.as_secs(), 1200);
    assert_eq!(HOLD_RETRY.as_secs(), 15);
}

/// A server that has not heard the host (unknown-host) is told of once; no request follows
/// before fifteen seconds without a hello; the next hello asks at once, the cordon is placed
/// and said, and the connection made first is the one used: one connect in all. It catches a
/// keeper that waits out the retry after a hello, one that repeats the failure line, and one
/// that drops its connection for an unknown host.
#[test]
fn an_unknown_host_is_retried_at_the_next_hello_on_the_same_connection() {
    let rig = Rig::new("hold-cordon-unknown-host");
    rig.hold();
    rig.server
        .script(vec![Reply::Fail(LeaseError::UnknownHost)]);
    let mut keeper = rig.keeper();
    assert_eq!(keeper.step(true), vec![HELD.to_owned(), failed(UNKNOWN)]);
    rig.clock.set(14_999);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(rig.server.times(), vec![0]);
    rig.clock.set(15_000);
    assert_eq!(keeper.step(true), vec![CORDONED.to_owned()]);
    assert_eq!(rig.server.times(), vec![0, 15_000]);
    assert_eq!(rig.connected(), 1);
}

/// Across three hours of steps a minute apart the cordon is asked for at once and then every
/// twenty minutes exactly, each time for the hour, so no gap reaches the hour and the cordon
/// never lapses; the renewals the server answers as already cordoned say nothing. It catches a
/// renewal that is late, early or missing, a lease of another length, and a line for each
/// renewal.
#[test]
fn the_cordon_is_renewed_every_twenty_minutes_and_says_nothing_of_it() {
    let rig = Rig::new("hold-cordon-renewal");
    rig.hold();
    let mut keeper = rig.keeper();
    let mut said = Vec::new();
    for minute in 0..=180u64 {
        rig.clock.set(minute * 60_000);
        said.extend(keeper.step(minute == 0));
    }
    assert_eq!(said, vec![HELD.to_owned(), CORDONED.to_owned()]);
    let times = rig.server.times();
    let expected: Vec<u64> = (0..=9).map(|n| n * 1_200_000).collect();
    assert_eq!(times, expected);
    assert!(rig.server.asks().iter().all(|(_, secs)| *secs == 3600));
    assert!(times.windows(2).all(|gap| gap[1] - gap[0] < 3_600_000));
    assert_eq!(rig.connected(), 1);
}

/// A renewal the server answers as not already cordoned (the cordon had lapsed and was placed
/// again) says the cordon is placed again, and not before the renewal is due. It catches a
/// keeper that says nothing of a lapse and one that asks early.
#[test]
fn a_cordon_placed_again_after_a_lapse_is_said_again() {
    let rig = Rig::new("hold-cordon-lapse");
    rig.hold();
    let mut keeper = rig.keeper();
    assert_eq!(
        keeper.step(true),
        vec![HELD.to_owned(), CORDONED.to_owned()]
    );
    rig.server.lapse();
    rig.clock.set(1_199_999);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    rig.clock.set(1_200_000);
    assert_eq!(keeper.step(false), vec![CORDONED.to_owned()]);
    assert_eq!(rig.server.times(), vec![0, 1_200_000]);
}

/// A request that fails for another reason than an unknown host is said once while the same
/// text repeats, asked again fifteen seconds later and not before, on a new connection each
/// time; a failure of other text is said; the success after it says the cordon is placed. It
/// catches a repeated failure line, a retry at the renewal interval, a connection kept past a
/// failed request, and a keeper that is silent about a new failure.
#[test]
fn a_failed_request_is_said_once_while_it_repeats_and_asked_again_after_fifteen_seconds() {
    let rig = Rig::new("hold-cordon-failure");
    rig.hold();
    rig.server.script(vec![
        Reply::Fail(LeaseError::Unreachable("down".to_owned())),
        Reply::Fail(LeaseError::Unreachable("down".to_owned())),
        Reply::Fail(LeaseError::Refused("nope".to_owned())),
        Reply::Ok,
    ]);
    let mut keeper = rig.keeper();
    assert_eq!(keeper.step(true), vec![HELD.to_owned(), failed("down")]);
    rig.clock.set(14_999);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(rig.server.times(), vec![0]);
    assert_eq!(rig.connected(), 1);
    rig.clock.set(15_000);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(rig.connected(), 2);
    rig.clock.set(29_999);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    rig.clock.set(30_000);
    assert_eq!(keeper.step(false), vec![failed("refused: nope")]);
    assert_eq!(rig.connected(), 3);
    rig.clock.set(45_000);
    assert_eq!(keeper.step(false), vec![CORDONED.to_owned()]);
    assert_eq!(rig.connected(), 4);
    assert_eq!(rig.server.times(), vec![0, 15_000, 30_000, 45_000]);
    rig.clock.set(45_000 + 1_200_000);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(rig.connected(), 4);
}

/// A reply that names another host is a failure, said with the name, and the connection is
/// dropped: the next request connects again and, answering for this host, is placed. It
/// catches a keeper that takes the cordon of another host for its own and one that keeps a
/// connection that answers for the wrong host.
#[test]
fn a_reply_naming_another_host_is_a_failure_and_drops_the_connection() {
    let rig = Rig::new("hold-cordon-other-host");
    rig.hold();
    rig.server.script(vec![Reply::Name("other")]);
    let mut keeper = rig.keeper();
    assert_eq!(
        keeper.step(true),
        vec![HELD.to_owned(), failed("the reply names host \"other\"")]
    );
    assert_eq!(rig.connected(), 1);
    rig.clock.set(15_000);
    assert_eq!(keeper.step(false), vec![CORDONED.to_owned()]);
    assert_eq!(rig.connected(), 2);
}

/// A connect that fails is said with its own text and tried again after fifteen seconds and
/// not before, the failure said once while it repeats; once it works the cordon is placed. It
/// catches a keeper that gives up on a connect, one that retries at once, and one that repeats
/// the line.
#[test]
fn a_connect_that_fails_is_said_and_tried_again_after_fifteen_seconds() {
    let rig = Rig::new("hold-cordon-connect");
    rig.hold();
    rig.down.set(true);
    let mut keeper = rig.keeper();
    assert_eq!(
        keeper.step(true),
        vec![HELD.to_owned(), failed("cannot connect")]
    );
    rig.clock.set(14_999);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(rig.connected(), 1);
    rig.clock.set(15_000);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(rig.connected(), 2);
    assert!(rig.server.asks().is_empty());
    rig.down.set(false);
    rig.clock.set(30_000);
    assert_eq!(keeper.step(false), vec![CORDONED.to_owned()]);
    assert_eq!(rig.server.times(), vec![30_000]);
}

/// A host that is never held is never asked about: through hellos and hours the keeper makes
/// no connection, no request and says nothing. It catches a keeper that cordons every host.
#[test]
fn a_host_that_is_not_held_is_never_cordoned() {
    let rig = Rig::new("hold-cordon-never-held");
    let mut keeper = rig.keeper();
    for hour in 0..4u64 {
        rig.clock.set(hour * 3_600_000);
        assert_eq!(keeper.step(true), Vec::<String>::new());
        assert_eq!(keeper.step(false), Vec::<String>::new());
    }
    assert_eq!(rig.connected(), 0);
    assert!(rig.server.asks().is_empty());
    assert_eq!(rig.server.uncordons(), 0);
}

/// A hold written while the agent runs is said and cordoned at the next step without a new
/// hello; its removal is said once and then nothing, the cordon is never lifted by the
/// keeper, and a hold written after that is said and cordoned again at once. It catches a
/// keeper that reads the hold file once, one that repeats the removal, one that lifts the
/// cordon, and one that treats a second hold as the first.
#[test]
fn a_hold_written_later_is_cordoned_and_its_removal_is_said_once() {
    let rig = Rig::new("hold-cordon-later");
    let mut keeper = rig.keeper();
    assert_eq!(keeper.step(true), Vec::<String>::new());
    rig.hold();
    rig.clock.set(5_000);
    assert_eq!(
        keeper.step(false),
        vec![HELD.to_owned(), CORDONED.to_owned()]
    );
    assert_eq!(rig.connected(), 1);
    rig.unhold();
    rig.clock.set(6_000);
    assert_eq!(keeper.step(false), vec![GONE.to_owned()]);
    rig.clock.set(2_000_000);
    assert_eq!(keeper.step(false), Vec::<String>::new());
    assert_eq!(keeper.step(true), Vec::<String>::new());
    assert_eq!(rig.server.times(), vec![5_000]);
    assert_eq!(rig.server.uncordons(), 0);
    rig.hold();
    rig.clock.set(2_001_000);
    assert_eq!(
        keeper.step(false),
        vec![HELD.to_owned(), CORDONED.to_owned()]
    );
    assert_eq!(rig.server.times(), vec![5_000, 2_001_000]);
    assert_eq!(rig.server.uncordons(), 0);
}

/// A hold file that is refused (group and others have permission on it) still holds the host:
/// the keeper cordons it, and its first line gives the refusal in full. It catches a keeper
/// that takes a refused file for an absent one, and one that hides why the file is refused.
#[test]
fn a_refused_hold_file_still_cordons_and_the_line_says_why() {
    let rig = Rig::new("hold-cordon-refused");
    rig.hold();
    fs::set_permissions(rig.dir.join(HOLD_FILE), fs::Permissions::from_mode(0o644)).unwrap();
    let mut keeper = rig.keeper();
    let line = concat!(
        "hold: this host is held: its hold file is refused (group or others have permission ",
        "on it (mode 0644)) and holds it until it is removed: it starts no work and keeps ",
        "itself cordoned"
    );
    assert_eq!(
        keeper.step(true),
        vec![line.to_owned(), CORDONED.to_owned()]
    );
    assert_eq!(rig.server.times(), vec![0]);
}

/// The server the connect tests name: a loopback address with port 0, which a connect never
/// reaches (no request is sent).
const NO_SERVER: &str = "https://127.0.0.1:0";

/// The daemon's connect fails with the client's own text, unchanged, when the certificate it
/// would present cannot be used: here files that are not there. It catches a connect that
/// hides why it failed, or that turns a certificate it cannot read into a connection.
#[test]
fn the_daemons_connect_says_what_the_client_could_not_use() {
    let scratch = Scratch::new("hold-cordon-connect-fails");
    let registered = Login::HostCertificate(HostIdentity::new(
        scratch.path("absent.crt"),
        scratch.path("absent.key"),
    ));
    let expected =
        match HttpLease::connect(NO_SERVER, &registered, None, "h1", RequestLimit::DEFAULT) {
            Ok(_) => panic!("a certificate that is not there connects"),
            Err(why) => why.to_string(),
        };
    match connect_http(NO_SERVER, &registered, None, "h1") {
        Ok(_) => panic!("the daemon's connect took a certificate that is not there"),
        Err(why) => assert_eq!(why, expected),
    }
}

/// The daemon's connect makes a connection when the host certificate can be used, and presents
/// the login the agent registered with as it is. Nothing is sent. It catches a connect that
/// refuses a usable certificate.
#[test]
fn the_daemons_connect_uses_the_certificate_the_agent_registered_with() {
    let scratch = Scratch::new("hold-cordon-connect-works");
    let registered = Login::HostCertificate(self_signed(&scratch, "h1"));
    assert!(connect_http(NO_SERVER, &registered, None, "h1").is_ok());
}
