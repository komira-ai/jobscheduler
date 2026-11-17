//! The agent's client of its mount providers (`js_agent::provider::ProviderClient`) against
//! the fake provider of `js_testkit::mount_provider`, which serves the contract on a real Unix
//! socket in the test's scratch directory: what each kind of call is answered, how a call
//! stays pending and is handed over, what a refusal and a failure become, what the client
//! checks about a socket before it connects, the bound on a call, and what the provider sees.
//! No test waits for a clock: a test asks, waits for the answer to be in (`wait_answered`,
//! which is bounded only by a guard that ends a hung test), and asks again. A call that the
//! fake holds never answers, so a bound that ends it is not a race.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use js_agent::lifecycle::{ProviderAnswer, ProviderCall, Providers};
use js_agent::provider::{CallBounds, ProviderClient};
use js_agent_wire::MountMode;
use js_proto::provider::v1 as pb;
use js_testkit::mount_provider::{FakeProvider, Kind, Seen, mount_path};

use crate::support::Scratch;

/// How long a test waits for an answer before it fails: far above any answer, and no test
/// passes by it.
const GUARD: Duration = Duration::from_secs(120);

/// The host id the agents of these tests registered under.
const HOST: &str = "host-1";

/// The name the agent was given the fake's socket under.
const NAME: &str = "tier";

/// A fake provider and a client of it.
struct Rig {
    _scratch: Scratch,
    fake: FakeProvider,
    client: ProviderClient,
}

/// The effective user of this test process.
fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The sockets of one provider named [`NAME`].
fn sockets(path: PathBuf) -> Vec<(String, PathBuf)> {
    vec![(NAME.to_owned(), path)]
}

/// Starts a fake provider in a scratch directory whose name is `name` (kept short: a socket's
/// path is at most about 108 bytes, and `CARGO_TARGET_TMPDIR/js-agent/<name>-<pid>-<n>/mp.sock`
/// must fit), with its socket owner-only whatever the umask is.
fn start(name: &str) -> (Scratch, FakeProvider) {
    let scratch = Scratch::new(name);
    let fake = FakeProvider::start(&scratch.path("")).unwrap();
    fs::set_permissions(fake.socket(), fs::Permissions::from_mode(0o600)).unwrap();
    (scratch, fake)
}

/// A fake provider and a client of it for the host [`HOST`], with the contract's bounds.
fn rig(name: &str) -> Rig {
    rig_with(name, CallBounds::default())
}

/// [`rig`] with the bounds `bounds`.
fn rig_with(name: &str, bounds: CallBounds) -> Rig {
    let (scratch, fake) = start(name);
    let client =
        ProviderClient::with_bounds(HOST, euid(), &sockets(fake.socket().to_path_buf()), bounds)
            .unwrap();
    Rig {
        _scratch: scratch,
        fake,
        client,
    }
}

fn prepare(run_id: &str, index: u32, reference: &str, mode: MountMode) -> ProviderCall {
    ProviderCall::Prepare {
        run_id: run_id.to_owned(),
        index,
        reference: reference.to_owned(),
        mode,
    }
}

fn mount(run_id: &str, index: u32) -> ProviderCall {
    ProviderCall::Mount {
        run_id: run_id.to_owned(),
        index,
    }
}

fn release(run_id: &str, index: u32) -> ProviderCall {
    ProviderCall::Release {
        run_id: run_id.to_owned(),
        index,
    }
}

fn sweep(keep: &[&str]) -> ProviderCall {
    ProviderCall::Sweep {
        keep: keep.iter().map(|run_id| (*run_id).to_owned()).collect(),
    }
}

/// Asks `call` of `provider`, waits for its answer and asks again for it. The first ask sends
/// the call and answers `Pending`.
fn settle(client: &mut ProviderClient, provider: &str, call: &ProviderCall) -> ProviderAnswer {
    assert_eq!(
        client.ask(provider, call),
        ProviderAnswer::Pending,
        "a call sent for the first time is pending"
    );
    assert!(
        client.wait_answered(provider, call, GUARD),
        "no answer to {call:?} within the guard"
    );
    client.ask(provider, call)
}

/// Asserts that `answer` is `Unavailable` and its reason contains `text`.
fn assert_unavailable(answer: &ProviderAnswer, text: &str) {
    match answer {
        ProviderAnswer::Unavailable(why) => assert!(
            why.contains(text),
            "the reason {why:?} does not contain {text:?}"
        ),
        other => panic!("expected Unavailable containing {text:?}, got {other:?}"),
    }
}

/// The source the provider sees for a run and an index of [`HOST`].
fn source(run: &str, index: u32) -> Option<pb::Source> {
    Some(pb::Source {
        agent: HOST.to_owned(),
        run: run.to_owned(),
        index,
    })
}

/// The key of a source in `FakeProvider::prepared`.
fn held_key(agent: &str, run: &str, index: u32) -> (String, String, u32) {
    (agent.to_owned(), run.to_owned(), index)
}

/// Asserts that a call of each kind reaches the provider and that its answer is what the
/// provider gave: `Done(None)` for a prepare, a release and a sweep, and the directory the
/// provider named for a mount, as the provider's own text. It catches a call kind that is
/// sent as another, and a mount whose path is dropped or altered on the way back.
#[test]
fn each_kind_of_call_is_answered_with_what_the_provider_gave() {
    let mut rig = rig("pc-kinds");
    assert_eq!(
        settle(
            &mut rig.client,
            NAME,
            &prepare("r1", 0, "k", MountMode::ReadOnly)
        ),
        ProviderAnswer::Done(None)
    );
    assert_eq!(
        settle(&mut rig.client, NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 0)))
    );
    assert_eq!(
        settle(&mut rig.client, NAME, &release("r1", 0)),
        ProviderAnswer::Done(None)
    );
    assert!(rig.fake.prepared().is_empty());
    assert_eq!(
        settle(&mut rig.client, NAME, &sweep(&[])),
        ProviderAnswer::Done(None)
    );
}

/// Asserts that a call is `Pending` for as long as the provider holds it and is not sent a
/// second time meanwhile, that its answer is handed over once when the provider lets it go,
/// and that asking again after that sends the call again (the provider then receives it a
/// second time and, the call being idempotent, answers the same). It catches a pending call
/// that is sent again on every poll, an answer that is handed over twice or lost, and an
/// answered call that is never sent again.
#[test]
fn a_held_call_stays_pending_then_its_answer_is_handed_over_once_then_it_is_sent_again() {
    let mut rig = rig("pc-pending");
    let call = prepare("r1", 0, "k", MountMode::ReadOnly);
    rig.fake.hold(Kind::Prepare);
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Pending);
    assert!(
        rig.fake.wait_parked(1, GUARD),
        "the provider did not receive the call"
    );
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Pending);
    rig.fake.unhold(Kind::Prepare);
    assert!(rig.client.wait_answered(NAME, &call, GUARD));
    assert_eq!(
        rig.fake.requests().len(),
        1,
        "the call was sent while it was out"
    );
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Done(None));
    assert_eq!(
        rig.client.ask(NAME, &call),
        ProviderAnswer::Pending,
        "the answer was handed over once; asking again sends the call again"
    );
    assert!(rig.client.wait_answered(NAME, &call, GUARD));
    assert_eq!(rig.fake.requests().len(), 2);
    assert_eq!(rig.client.ask(NAME, &call), ProviderAnswer::Done(None));
    assert_eq!(rig.fake.prepared().len(), 1, "a prepare is idempotent");
}

/// Asserts that two calls with different keys (two indexes of one run, and a prepare and a
/// mount of one source) are each sent and each answered, so one call's answer is never
/// handed to another's ask. It catches a table keyed by less than the kind, the run and the
/// index.
#[test]
fn calls_with_different_keys_are_answered_apart() {
    let mut rig = rig("pc-keys");
    let first = prepare("r1", 0, "a", MountMode::ReadOnly);
    let second = prepare("r1", 1, "b", MountMode::ReadOnly);
    assert_eq!(rig.client.ask(NAME, &first), ProviderAnswer::Pending);
    assert_eq!(rig.client.ask(NAME, &second), ProviderAnswer::Pending);
    assert!(rig.client.wait_answered(NAME, &first, GUARD));
    assert!(rig.client.wait_answered(NAME, &second, GUARD));
    assert_eq!(rig.client.ask(NAME, &first), ProviderAnswer::Done(None));
    assert_eq!(rig.client.ask(NAME, &second), ProviderAnswer::Done(None));
    assert_eq!(
        settle(&mut rig.client, NAME, &mount("r1", 1)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 1)))
    );
    assert_eq!(rig.fake.requests().len(), 3);
}

/// Asserts that a ref the provider refuses (`INVALID_ARGUMENT`) is answered `Refused` with the
/// provider's own reason, and that a source prepared with one ref and prepared again with
/// another (`FAILED_PRECONDITION`) is refused with that reason too. It catches a refusal
/// that is read as a try-again-later, and a reason that is replaced by a generic text.
#[test]
fn a_refusal_of_the_provider_is_refused_with_its_reason() {
    let mut rig = rig("pc-refuse");
    rig.fake.refuse("bad");
    assert_eq!(
        settle(
            &mut rig.client,
            NAME,
            &prepare("r1", 0, "bad", MountMode::ReadOnly)
        ),
        ProviderAnswer::Refused("the fake provider refuses the ref bad".to_owned())
    );
    assert!(rig.fake.prepared().is_empty());
    assert_eq!(
        settle(
            &mut rig.client,
            NAME,
            &prepare("r1", 0, "a", MountMode::ReadOnly)
        ),
        ProviderAnswer::Done(None)
    );
    assert_eq!(
        settle(
            &mut rig.client,
            NAME,
            &prepare("r1", 0, "b", MountMode::ReadOnly)
        ),
        ProviderAnswer::Refused("the source is prepared with the ref a".to_owned())
    );
}

/// Asserts that a mount of a source the provider never prepared is refused, with the
/// provider's reason (`FAILED_PRECONDITION`: a tree that will not be ready), and that a
/// release of a source the provider does not know succeeds. It catches a mount of an
/// unknown source answered as a directory, and a release that fails when there is nothing to
/// give up.
#[test]
fn a_mount_of_an_unprepared_source_is_refused_and_a_release_of_one_succeeds() {
    let mut rig = rig("pc-unknown");
    assert_eq!(
        settle(&mut rig.client, NAME, &mount("r1", 0)),
        ProviderAnswer::Refused("the source was not prepared".to_owned())
    );
    assert_eq!(
        settle(&mut rig.client, NAME, &release("r1", 0)),
        ProviderAnswer::Done(None)
    );
}

/// Asserts that a mount the provider answers `UNAVAILABLE` twice is `Unavailable` each time
/// with the provider's message, that asking again sends it again, and that it then gets the
/// directory; and that a release the provider fails with another code (`INTERNAL`) is
/// `Unavailable` too, and succeeds when asked again. It catches an `UNAVAILABLE` or an
/// unexpected code that ends the source for good, and a failed call that is not sent again.
#[test]
fn an_unavailable_provider_and_any_other_failure_are_unavailable_and_asked_again() {
    let mut rig = rig("pc-unavail");
    assert_eq!(
        settle(
            &mut rig.client,
            NAME,
            &prepare("r1", 0, "k", MountMode::ReadOnly)
        ),
        ProviderAnswer::Done(None)
    );
    rig.fake.fail_mount(2);
    for _ in 0..2 {
        assert_unavailable(
            &settle(&mut rig.client, NAME, &mount("r1", 0)),
            "the fake provider is not ready",
        );
    }
    assert_eq!(
        settle(&mut rig.client, NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path(HOST, "js.r1", 0)))
    );
    rig.fake.fail_release(1);
    assert_unavailable(
        &settle(&mut rig.client, NAME, &release("r1", 0)),
        "the fake provider failed to release",
    );
    assert_eq!(
        rig.fake.prepared().len(),
        1,
        "the failed release released nothing"
    );
    assert_eq!(
        settle(&mut rig.client, NAME, &release("r1", 0)),
        ProviderAnswer::Done(None)
    );
    assert!(rig.fake.prepared().is_empty());
}

/// Asserts that the client says which providers it was given (`has`, and `names` in
/// alphabetical order), and that a call to a provider it was not given is `Unavailable` at
/// once, naming the provider, and sends nothing. It catches a call to an unknown name that
/// is sent to some other provider or that panics.
#[test]
fn a_provider_the_agent_was_not_given_is_unavailable_and_nothing_is_sent() {
    let (_scratch, fake) = start("pc-names");
    let socket = fake.socket().to_path_buf();
    let mut client = ProviderClient::new(
        HOST,
        euid(),
        &[
            ("zeta".to_owned(), socket.clone()),
            ("alpha".to_owned(), socket),
        ],
    )
    .unwrap();
    assert!(client.has("alpha") && client.has("zeta"));
    assert!(!client.has("nope"));
    assert_eq!(client.names(), ["alpha", "zeta"]);
    assert_eq!(
        client.ask("nope", &sweep(&[])),
        ProviderAnswer::Unavailable("this agent was given no mount provider named nope".to_owned())
    );
    assert!(fake.requests().is_empty());
}

/// Asserts that a provider whose socket does not exist yet is `Unavailable` (naming the
/// path), and that the same client reaches the provider on a later ask once it is there,
/// since the channel connects again for each ask that finds it unconnected. It catches a
/// client that gives a provider up for good when it is not there at first, as it is when the
/// agent starts before the provider does.
#[test]
fn a_missing_socket_is_unavailable_and_the_provider_is_reached_once_it_is_there() {
    let scratch = Scratch::new("pc-late");
    let socket = scratch.path("mp.sock");
    let mut client = ProviderClient::new(HOST, euid(), &sockets(socket.clone())).unwrap();
    let call = sweep(&[]);
    assert_unavailable(&settle(&mut client, NAME, &call), "mp.sock");
    let fake = FakeProvider::start(&scratch.path("")).unwrap();
    fs::set_permissions(fake.socket(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(socket, fake.socket());
    assert_eq!(settle(&mut client, NAME, &call), ProviderAnswer::Done(None));
}

/// Asserts that a path that is a file and not a socket is `Unavailable` with that reason. It
/// catches a client that connects to, or fails obscurely on, whatever is at the path.
#[test]
fn a_path_that_is_not_a_socket_is_unavailable() {
    let scratch = Scratch::new("pc-file");
    let path = scratch.write("mp.sock", "", 0o600);
    let mut client = ProviderClient::new(HOST, euid(), &sockets(path)).unwrap();
    assert_unavailable(&settle(&mut client, NAME, &sweep(&[])), "is not a socket");
}

/// Asserts that a socket a group or others can write is `Unavailable` with that reason, that
/// the group bit alone is enough, and that the same client reaches the provider when the
/// socket is owner-only again. It catches a socket that anyone could have put in the
/// provider's place being trusted with the refs of jobs.
#[test]
fn a_socket_others_can_write_is_unavailable_until_it_is_not() {
    let mut rig = rig("pc-mode");
    let call = sweep(&[]);
    for mode in [0o666, 0o620] {
        fs::set_permissions(rig.fake.socket(), fs::Permissions::from_mode(mode)).unwrap();
        assert_unavailable(
            &settle(&mut rig.client, NAME, &call),
            "can be written by its group or by others",
        );
    }
    fs::set_permissions(rig.fake.socket(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        settle(&mut rig.client, NAME, &call),
        ProviderAnswer::Done(None)
    );
}

/// Asserts that a socket owned by a user that is neither the agent's nor root is
/// `Unavailable` with that reason. The socket is the test's own, so the agent is told it runs
/// as another user (when the test runs as root it first gives the socket to another user, as
/// root may). It catches an owner check that is missing or that compares the wrong ids.
#[test]
fn a_socket_owned_by_another_user_is_unavailable() {
    let (_scratch, fake) = start("pc-owner");
    // Root may give the socket away; any other user cannot, and keeps owning it.
    let _gave = std::os::unix::fs::chown(fake.socket(), Some(65534), None);
    let owner = fs::metadata(fake.socket()).unwrap().uid();
    let mut client =
        ProviderClient::new(HOST, owner + 1, &sockets(fake.socket().to_path_buf())).unwrap();
    assert_unavailable(
        &settle(&mut client, NAME, &sweep(&[])),
        &format!("belongs to user {owner}"),
    );
    assert!(fake.requests().is_empty());
}

/// Asserts that a socket file nobody listens on (a provider that died and left its file) is
/// `Unavailable`. It catches a failed connect that is taken for an answer or that ends the
/// channel.
#[test]
fn a_socket_nobody_listens_on_is_unavailable() {
    let scratch = Scratch::new("pc-stale");
    let path = scratch.path("mp.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut client = ProviderClient::new(HOST, euid(), &sockets(path)).unwrap();
    assert!(matches!(
        settle(&mut client, NAME, &sweep(&[])),
        ProviderAnswer::Unavailable(_)
    ));
}

/// Asserts that a call of each kind that the provider holds ends `Unavailable` at its own
/// bound ("no answer within <bound>"): the bound of the kind under test is small and the
/// others are the contract's, and the provider never answers, so the bound is what ends the
/// call. It catches a kind that is not bounded, a bound read from another kind, and a call
/// that stays pending for ever behind a provider that does not answer.
#[test]
fn a_call_the_provider_holds_is_unavailable_at_its_bound() {
    let small = Duration::from_millis(20);
    let ends = format!("no answer within {small:?}");
    let defaults = CallBounds::default();
    let cases = [
        (
            Kind::Prepare,
            prepare("r1", 0, "k", MountMode::ReadOnly),
            CallBounds {
                prepare: small,
                ..defaults
            },
        ),
        (
            Kind::Mount,
            mount("r1", 0),
            CallBounds {
                mount: small,
                ..defaults
            },
        ),
        (
            Kind::Release,
            release("r1", 0),
            CallBounds {
                release: small,
                ..defaults
            },
        ),
        (
            Kind::Sweep,
            sweep(&[]),
            CallBounds {
                sweep: small,
                ..defaults
            },
        ),
    ];
    for (kind, call, bounds) in cases {
        let mut rig = rig_with("pc-bound", bounds);
        rig.fake.hold(kind);
        assert_eq!(
            settle(&mut rig.client, NAME, &call),
            ProviderAnswer::Unavailable(ends.clone())
        );
    }
}

/// Asserts that the bounds a client is given by default are the contract's: 10 s for a
/// prepare, 30 s for a mount, 5 s for a release and 30 s for a sweep. It catches a bound
/// that drifts from the design.
#[test]
fn the_default_bounds_are_the_contracts() {
    assert_eq!(
        CallBounds::default(),
        CallBounds {
            prepare: Duration::from_secs(10),
            mount: Duration::from_secs(30),
            release: Duration::from_secs(5),
            sweep: Duration::from_secs(30),
        }
    );
}

/// Asserts that waiting for a call that was never asked gives up at the guard and says so,
/// since nothing can answer it. It catches a wait that reports an answer that is not there.
#[test]
fn waiting_for_a_call_that_was_never_asked_gives_up_at_the_guard() {
    let rig = rig("pc-wait");
    assert!(
        !rig.client
            .wait_answered(NAME, &sweep(&[]), Duration::from_millis(10))
    );
}

/// Asserts that a client can be handed to the lifecycle's worker thread. It catches a field
/// that is not `Send`.
#[test]
fn the_client_can_move_to_another_thread() {
    fn is_send<T: Send>() {}
    is_send::<ProviderClient>();
}

/// Asserts what the provider received for each call: the agent's host id as `Source.agent`,
/// `PrepareRequest.host` and `SweepRequest.agent`, `js.` before every run id (in the keep list
/// of a sweep too), the index, the ref untouched, and each mode of the wire as the protocol's
/// mode. It catches an id sent without its prefix, a ref altered on the way, a mode mapped to
/// another, and a call that names another source than the one asked.
#[test]
fn the_provider_sees_the_agents_id_the_prefixed_run_the_index_the_ref_and_the_mode() {
    let mut rig = rig("pc-saw");
    let modes = [
        (MountMode::ReadOnly, pb::MountMode::ReadOnly),
        (MountMode::ReadWrite, pb::MountMode::ReadWrite),
        (MountMode::CopyOnWrite, pb::MountMode::CopyOnWrite),
    ];
    let mut expected = Vec::new();
    for (index, (mode, protocol)) in (2..).zip(modes) {
        let reference = format!("ref/{index}");
        assert_eq!(
            settle(
                &mut rig.client,
                NAME,
                &prepare("run-a", index, &reference, mode)
            ),
            ProviderAnswer::Done(None)
        );
        expected.push(Seen::Prepare(pb::PrepareRequest {
            source: source("js.run-a", index),
            r#ref: reference,
            mode: protocol as i32,
            host: HOST.to_owned(),
        }));
    }
    settle(&mut rig.client, NAME, &mount("run-a", 3));
    expected.push(Seen::Mount(pb::MountRequest {
        source: source("js.run-a", 3),
    }));
    settle(&mut rig.client, NAME, &release("run-a", 4));
    expected.push(Seen::Release(pb::ReleaseRequest {
        source: source("js.run-a", 4),
    }));
    settle(&mut rig.client, NAME, &sweep(&["run-b", "run-c"]));
    expected.push(Seen::Sweep(pb::SweepRequest {
        agent: HOST.to_owned(),
        keep: vec!["js.run-b".to_owned(), "js.run-c".to_owned()],
    }));
    assert_eq!(rig.fake.requests(), expected);
}

/// Asserts that a sweep releases the sources of the agent that sweeps whose run it does not
/// keep, and only those: a source of a kept run stays, and so does a source of a run that
/// another agent also named. It catches a sweep that releases a kept run or a run that is
/// not the agent's own.
#[test]
fn a_sweep_releases_the_sources_the_agent_does_not_keep() {
    let mut rig = rig("pc-sweep");
    for (run, index) in [("r1", 0), ("r1", 1), ("r2", 0), ("r3", 0)] {
        assert_eq!(
            settle(
                &mut rig.client,
                NAME,
                &prepare(run, index, "k", MountMode::ReadOnly)
            ),
            ProviderAnswer::Done(None)
        );
    }
    assert_eq!(
        settle(&mut rig.client, NAME, &sweep(&["r1"])),
        ProviderAnswer::Done(None)
    );
    let kept: BTreeMap<_, _> = [
        (held_key(HOST, "js.r1", 0), "k".to_owned()),
        (held_key(HOST, "js.r1", 1), "k".to_owned()),
    ]
    .into();
    assert_eq!(rig.fake.prepared(), kept);
}

/// Asserts that when two agents (two host ids) share one provider and one of them sweeps
/// without keeping anything, its own sources are released and the other agent's remain, so an
/// agent that restarts never gives up the sources of another agent on its host. It catches a
/// sweep that does not name its agent, or a provider contract in which one agent's sweep
/// reaches another's sources.
#[test]
fn a_sweep_leaves_another_agents_sources_alone() {
    let (_scratch, fake) = start("pc-agents");
    let sockets = sockets(fake.socket().to_path_buf());
    let mut first = ProviderClient::new("host-a", euid(), &sockets).unwrap();
    let mut second = ProviderClient::new("host-b", euid(), &sockets).unwrap();
    let call = prepare("r1", 0, "k", MountMode::ReadOnly);
    assert_eq!(settle(&mut first, NAME, &call), ProviderAnswer::Done(None));
    assert_eq!(settle(&mut second, NAME, &call), ProviderAnswer::Done(None));
    assert_eq!(fake.prepared().len(), 2);
    assert_eq!(
        settle(&mut first, NAME, &sweep(&[])),
        ProviderAnswer::Done(None)
    );
    let remaining: BTreeMap<_, _> = [(held_key("host-b", "js.r1", 0), "k".to_owned())].into();
    assert_eq!(fake.prepared(), remaining);
    assert_eq!(
        settle(&mut second, NAME, &mount("r1", 0)),
        ProviderAnswer::Done(Some(mount_path("host-b", "js.r1", 0)))
    );
}
