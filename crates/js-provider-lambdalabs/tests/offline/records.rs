//! What the provider's own records answer, with no request: `capacity` and
//! `verify_instance` through the lifecycles that make records overlap or go stale (a lost
//! launch answer that lands, the extras of a launch, suspects, reads and releases that
//! come before or after a launch answer, a launch again under a released acquire id, a
//! restart), the capabilities with no catalog, and that neither waits for a request in
//! flight.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    Acquired, Capacity, CapacityProvider, Hello, HostName, HostRef, HostStatus, Observed, Released,
    SchedulerId,
};
use js_provider_lambdalabs::{
    ACQUIRE_TAG, ApiError, Call, FakeLambda, FirewallRuleset, Instance, InstanceId,
    InstanceTypeOffer, LambdaApi, LambdaProvider, LaunchRequest, ManualClock, OptionTable,
    POOL_TAG, SCHEDULER_TAG, SshKey, Tag,
};

use crate::rig::{Rig, SCHEDULER, TABLE, ask, host, id, live, rig, row, tag};

/// A provider with room for `machines` and no space between launches.
fn roomy(machines: usize) -> Rig {
    rig(live(machines).launch_interval(Duration::ZERO))
}

fn capacity(supplied: u32, can_add: u32) -> Capacity {
    Capacity {
        supplied,
        can_add: Some(can_add),
    }
}

fn accepted(answer: &Acquired) -> HostRef {
    match answer {
        Acquired::Accepted { held, .. } => held.id.clone(),
        other => panic!("expected an accepted launch, got {other:?}"),
    }
}

/// The name this scheduler derives for `acquire`.
fn name_of(acquire: &str) -> String {
    ask(acquire).name.to_string()
}

fn hello(name: &str, instance: Option<&str>) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: instance.map(str::to_owned),
    }
}

/// The marks this scheduler puts on the instance of `acquire`.
fn ours(acquire: &str) -> Vec<Tag> {
    vec![
        tag(SCHEDULER_TAG, SCHEDULER),
        tag(ACQUIRE_TAG, acquire),
        tag(POOL_TAG, "pool-a"),
    ]
}

/// Asserts that a launch whose answer was lost but which landed is counted once: while
/// unanswered it holds one place, and once a list shows its instance it is the instance
/// that holds it, not the instance and the launch. It catches a bound that counts an
/// unanswered launch beside the instance it made, which would refuse a launch the guard
/// allows.
#[test]
fn a_lost_launch_that_landed_is_counted_once_after_a_list() {
    let rig = roomy(3);
    rig.fake
        .lose_response(Call::Launch, ApiError::Transport("reset".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(rig.provider.capacity(), capacity(0, 2));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(1, 2));
}

/// Asserts that a launch that started two instances counts both and supplies only the
/// one it is answered with (the extra is released at once), and that a list showing both
/// changes neither count. It catches an extra counted twice, or supplied while it is
/// being terminated.
#[test]
fn the_extra_of_a_launch_is_counted_once_and_not_supplied() {
    let rig = roomy(4);
    rig.fake.launch_creates(2);
    let answer = rig.provider.acquire(&ask("a1"));
    assert!(matches!(&answer, Acquired::Accepted { extras, .. } if extras.len() == 1));
    assert_eq!(rig.provider.capacity(), capacity(1, 2));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(1, 2));
}

/// Asserts that a suspect (an instance with our name and none of our tags) holds a place
/// until a read by id finds it terminated, and another until a read finds it gone. It
/// catches suspects kept as a count that no read can lower.
#[test]
fn a_suspect_holds_a_place_until_a_read_finds_it_ended() {
    let rig = roomy(4);
    let own = HostName::prefix(&SchedulerId::new(SCHEDULER).unwrap());
    for text in ["suspect0001", "suspect0002"] {
        let name = format!("{own}0123456789ab");
        let suspect = row(text, Some(&name), HostStatus::Active, Vec::new());
        rig.fake.add_instance(suspect);
    }
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(0, 2));
    rig.fake
        .set_status(&id("suspect0001"), HostStatus::Terminated);
    assert_eq!(
        rig.provider.observe(&host("suspect0001")),
        Observed::Present(HostStatus::Terminated)
    );
    assert_eq!(rig.provider.capacity(), capacity(0, 3));
    rig.fake.remove_instance(&id("suspect0002"));
    assert_eq!(rig.provider.observe(&host("suspect0002")), Observed::Absent);
    assert_eq!(rig.provider.capacity(), capacity(0, 4));
}

/// Asserts that the launch guard's copy of a launch answer never brings back an older
/// state: an instance a read found gone stays gone, and one a release found terminating
/// stays terminating, through a later acquire whose list fails (the guard still holds
/// both answers then). A hello by the gone instance's name matches nothing. It catches a
/// copy that overwrites later reads with the launch-time `Booting`.
#[test]
fn a_launch_answer_never_brings_back_a_later_read_or_release() {
    let rig = roomy(4);
    let gone = accepted(&rig.provider.acquire(&ask("a1")));
    rig.fake.remove_instance(&id(gone.as_str()));
    assert_eq!(rig.provider.observe(&gone), Observed::Absent);
    // The guard still holds the gone instance's answer (no list has shown it) when this
    // acquire launches, and when the next one copies the guard again.
    let ending = accepted(&rig.provider.acquire(&ask("a2")));
    assert!(matches!(
        rig.provider.release(&ending),
        Released::Terminating(HostStatus::Terminating)
    ));
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a3")), Acquired::Unknown);
    assert_eq!(rig.provider.capacity(), capacity(0, 3));
    assert_eq!(
        rig.provider.verify_instance(&hello(&name_of("a1"), None)),
        None
    );
    assert_eq!(
        rig.provider.verify_instance(&hello(&name_of("a2"), None)),
        None
    );
}

/// Asserts that an instance the launch guard holds that a list does not show yet stays
/// held (a list can lag a launch), and that a terminating one a list no longer shows is
/// still not supplied. It catches a list that drops a launch it cannot see yet.
#[test]
fn a_list_keeps_a_launch_it_does_not_show_yet() {
    let rig = roomy(4);
    let unseen = accepted(&rig.provider.acquire(&ask("a1")));
    rig.fake.remove_instance(&id(unseen.as_str()));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(1, 3));
    assert_eq!(
        rig.provider.verify_instance(&hello(&name_of("a1"), None)),
        Some(unseen)
    );
    let ending = accepted(&rig.provider.acquire(&ask("a2")));
    assert!(matches!(
        rig.provider.release(&ending),
        Released::Terminating(HostStatus::Terminating)
    ));
    rig.fake.remove_instance(&id(ending.as_str()));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(1, 2));
    assert_eq!(
        rig.provider.verify_instance(&hello(&name_of("a2"), None)),
        None
    );
}

/// Asserts how a hello is matched when several instances share a name: after a release
/// and a launch again under the same acquire id, a hello by name alone is the running
/// instance, and one with an instance id is exactly that instance; with two running
/// instances of one name (a duplicate row of the account), a hello by name alone is
/// matched to neither. It catches a match that takes the first record of a name.
#[test]
fn a_hello_by_name_alone_needs_one_running_instance_of_that_name() {
    let rig = roomy(4);
    let first = accepted(&rig.provider.acquire(&ask("a1")));
    assert!(matches!(
        rig.provider.release(&first),
        Released::Terminating(_)
    ));
    let second = accepted(&rig.provider.acquire(&ask("a1")));
    assert_ne!(first, second);
    let name = name_of("a1");
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None)),
        Some(second.clone())
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(first.as_str()))),
        Some(first.clone())
    );
    let twin = row("twin0001", Some(&name), HostStatus::Active, ours("a1"));
    rig.fake.add_instance(twin);
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.verify_instance(&hello(&name, None)), None);
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some("twin0001"))),
        Some(host("twin0001"))
    );
}

/// Asserts that after a restart a list shows the instances of the account, and none of
/// these authenticates a hello: one with our name and no tags, one with our tags and no
/// name, and one with our tags whose name carries our prefix but is not one we derive.
/// It catches a match on the prefix alone, or on the tags alone.
#[test]
fn a_hello_is_not_matched_to_a_partly_marked_or_misnamed_instance() {
    let rig = roomy(4);
    let prefix = HostName::prefix(&SchedulerId::new(SCHEDULER).unwrap());
    let malformed = format!("{prefix}not-a-hash");
    let rows = [
        row(
            "nametag0001",
            Some(&name_of("a1")),
            HostStatus::Active,
            Vec::new(),
        ),
        row("tagonly0001", None, HostStatus::Active, ours("a2")),
        row(
            "badname0001",
            Some(&malformed),
            HostStatus::Active,
            ours("a3"),
        ),
    ];
    for instance in rows {
        rig.fake.add_instance(instance);
    }
    rig.provider.list().unwrap();
    for (name, instance) in [
        (name_of("a1"), "nametag0001"),
        (name_of("a2"), "tagonly0001"),
        (malformed.clone(), "badname0001"),
    ] {
        assert_eq!(rig.provider.verify_instance(&hello(&name, None)), None);
        let with_id = hello(&name, Some(instance));
        assert_eq!(rig.provider.verify_instance(&with_id), None, "{instance}");
    }
}

/// Asserts the counts of statuses a list can show: a preempted instance is not supplied
/// and still counts; one in a status this crate does not know is supplied and counts.
/// A fresh provider that releases or reads an instance it has not listed yet answers what
/// the API said and adds nothing it does not know. It catches an unknown status read as
/// ended, and a release that makes up a record.
#[test]
fn capacity_counts_preempted_and_unknown_statuses_and_reads_before_a_list() {
    let rig = roomy(4);
    for (text, status) in [
        ("preempted0001", HostStatus::Preempted),
        ("unknown0001", HostStatus::Unknown("rebooting".to_owned())),
        ("running0001", HostStatus::Active),
    ] {
        let acquire = format!("{text}-acquire");
        let instance = row(text, Some(&name_of(&acquire)), status, ours(&acquire));
        rig.fake.add_instance(instance);
    }
    assert!(matches!(
        rig.provider.release(&host("running0001")),
        Released::Terminating(_)
    ));
    assert!(matches!(
        rig.provider.observe(&host("unknown0001")),
        Observed::Present(_)
    ));
    assert_eq!(rig.provider.capacity(), capacity(0, 4));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(1, 1));
}

/// Asserts that the capabilities need no catalog: with no type in any region, and with
/// the types call failing, they are the same and make no call. It catches capabilities
/// read from what is available now.
#[test]
fn the_capabilities_hold_when_nothing_is_available_and_the_catalog_fails() {
    let rig = roomy(1);
    let caps = rig.provider.capabilities();
    let mut types = rig.fake.calls().len();
    let none = crate::rig::offer(
        "gpu_1x_sample",
        75,
        1,
        js_provider_lambdalabs::Architecture::X86_64,
        &[],
    );
    rig.fake.set_offers(vec![none]);
    assert!(rig.provider.catalog().unwrap().is_empty());
    types += 1;
    assert_eq!(rig.provider.capabilities(), caps);
    rig.fake
        .fail_next(Call::InstanceTypes, ApiError::Transport("down".to_owned()));
    assert!(rig.provider.catalog().is_err());
    types += 1;
    assert_eq!(rig.provider.capabilities(), caps);
    assert_eq!(rig.fake.calls().len(), types, "capabilities make no call");
}

/// Asserts the order of the in-memory account's failures: the failure of the next call
/// of any kind comes first, then the one queued for the kind, then the call works.
#[test]
fn the_next_call_failure_comes_before_the_one_queued_for_its_kind() {
    let fake = FakeLambda::new();
    fake.fail_next(Call::ListSshKeys, ApiError::Transport("queued".to_owned()));
    fake.fail_next_call(ApiError::Transport("next".to_owned()));
    let errors: Vec<String> = (0..2)
        .map(|_| fake.list_ssh_keys().unwrap_err().to_string())
        .collect();
    assert_eq!(
        errors,
        [
            "cannot talk to the API: next",
            "cannot talk to the API: queued"
        ]
    );
    assert!(fake.list_ssh_keys().is_ok());
}

/// An API that holds the next `list_instances` until told to go on, and says when it
/// holds it.
struct Held {
    inner: Arc<FakeLambda>,
    entered: Mutex<Sender<()>>,
    go: Mutex<Receiver<()>>,
}

impl LambdaApi for Held {
    fn instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError> {
        self.inner.instance_types()
    }

    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        self.entered.lock().unwrap().send(()).unwrap();
        self.go.lock().unwrap().recv().unwrap();
        self.inner.list_instances()
    }

    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError> {
        self.inner.get_instance(id)
    }

    fn launch(&self, request: &LaunchRequest) -> Result<Vec<InstanceId>, ApiError> {
        self.inner.launch(request)
    }

    fn terminate(&self, id: &InstanceId) -> Result<Vec<InstanceId>, ApiError> {
        self.inner.terminate(id)
    }

    fn list_firewall_rulesets(&self) -> Result<Vec<FirewallRuleset>, ApiError> {
        self.inner.list_firewall_rulesets()
    }

    fn create_firewall_ruleset(
        &self,
        region: &str,
        name: &str,
    ) -> Result<FirewallRuleset, ApiError> {
        self.inner.create_firewall_ruleset(region, name)
    }

    fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        self.inner.list_ssh_keys()
    }

    fn create_ssh_key(&self, name: &str) -> Result<SshKey, ApiError> {
        self.inner.create_ssh_key(name)
    }
}

/// Asserts that `capacity` and `verify_instance` answer while an acquire is held inside
/// its list request (with the launch lock taken), make no request of their own, and that
/// the acquire's records land once the request goes on. The order is set by explicit
/// events, not by time. It catches the local calls taking the launch lock, which would
/// make them wait for the API.
#[test]
fn capacity_and_hello_matching_answer_while_a_request_is_in_flight() {
    let template = roomy(2);
    let (entered_tx, entered) = mpsc::channel();
    let (go, go_rx) = mpsc::channel();
    let api = Arc::new(Held {
        inner: template.fake.clone(),
        entered: Mutex::new(entered_tx),
        go: Mutex::new(go_rx),
    });
    let provider = Arc::new(LambdaProvider::new(
        api,
        Arc::new(ManualClock::new()),
        OptionTable::parse(TABLE).unwrap(),
        live(2).launch_interval(Duration::ZERO),
    ));
    let acquiring = {
        let provider = provider.clone();
        std::thread::spawn(move || provider.acquire(&ask("a1")))
    };
    entered.recv().unwrap();
    let calls = template.fake.calls().len();
    assert_eq!(provider.capacity(), capacity(0, 2));
    assert_eq!(provider.verify_instance(&hello(&name_of("a1"), None)), None);
    assert_eq!(template.fake.calls().len(), calls, "no request was made");
    go.send(()).unwrap();
    let answer = acquiring.join().unwrap();
    let launched = accepted(&answer);
    assert_eq!(provider.capacity(), capacity(1, 1));
    assert_eq!(
        provider.verify_instance(&hello(&name_of("a1"), None)),
        Some(launched)
    );
}

/// Asserts that what the launch guard's own probe of an unlisted instance reads reaches
/// the records: once the hold has run out, an instance the probe finds gone, and one it
/// finds terminated (its tags gone, so no list shows it as ours), are no longer supplied
/// and match no hello. It catches records that keep supplying an instance only the guard
/// knows is gone.
#[test]
fn what_the_guards_probe_reads_reaches_the_records() {
    let rig = roomy(4);
    let past_the_hold = Duration::from_secs(11 * 60);
    let gone = accepted(&rig.provider.acquire(&ask("a1")));
    rig.fake.remove_instance(&id(gone.as_str()));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity(), capacity(1, 3));
    rig.clock.advance(past_the_hold);
    accepted(&rig.provider.acquire(&ask("a2")));
    assert_eq!(rig.provider.capacity(), capacity(1, 3));
    assert_eq!(
        rig.provider.verify_instance(&hello(&name_of("a1"), None)),
        None
    );

    let ended = accepted(&rig.provider.acquire(&ask("a3")));
    rig.fake.update_instance(&id(ended.as_str()), &|instance| {
        instance.tags.clear();
        instance.status = HostStatus::Terminated;
    });
    rig.clock.advance(past_the_hold);
    accepted(&rig.provider.acquire(&ask("a4")));
    assert_eq!(rig.provider.capacity(), capacity(2, 2));
    assert_eq!(
        rig.provider.verify_instance(&hello(&name_of("a3"), None)),
        None
    );
}

/// Asserts that a launch with no answer under the acquire id of an instance released
/// before it holds its place: the terminating instance is not the one the lost launch may
/// have made. It catches a lost launch matched to an earlier instance of its acquire id.
#[test]
fn a_lost_replacement_launch_keeps_its_place() {
    let rig = roomy(2);
    let first = accepted(&rig.provider.acquire(&ask("a1")));
    assert!(matches!(
        rig.provider.release(&first),
        Released::Terminating(_)
    ));
    rig.fake
        .fail_next(Call::Launch, ApiError::Transport("timed out".to_owned()));
    assert_eq!(rig.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(rig.provider.capacity(), capacity(0, 0));
}
