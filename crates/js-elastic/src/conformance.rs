//! The cases every [`CapacityProvider`] passes, one per rule of the contract, generic over
//! a [`Rig`] that the provider's crate writes: the rig puts the provider's cloud (a fake
//! of it) in a named state and hands over the provider under test.
//!
//! A provider that cannot launch (one whose machines register themselves) runs the
//! registered set instead, [`run_all_registered`] over a [`RegisteredRig`], which returns
//! how many of [`REGISTERED_CASES`] it ran. The cases of [`run_all`] do not apply to it:
//! they assume a [`Rig`] that scripts a cloud which launches, and a catalog with offers.
//!
//! A provider crate runs the suite from one test file: it enables this crate's
//! `conformance` feature in its `[dev-dependencies]`, implements [`Rig`] over its fake
//! cloud, and calls [`run_all`]. A new provider crate does the same; the cases do not
//! change. Each case is also a function of its own, so a crate can run one alone, and
//! this crate's own tests run a case against a provider that breaks its rule to show the
//! case fails on it.
//!
//! A case panics when the provider breaks the rule. Every assertion message starts with
//! the case's name, which is what a `should_panic(expected = ...)` test matches. No case
//! reads a clock or sleeps: a rig whose provider spaces its launches in time sets the space
//! to nothing. A case awaits the catalog with [`block_on`], which parks the calling thread
//! until the provider's future wakes it; a rig whose provider's catalog runs on an async
//! runtime enters that runtime before the cases run.

mod registered;

pub use registered::{
    REGISTERED_CASES, RegisteredCase, RegisteredRig, registered_acquire_is_refused_with_capacity,
    registered_capacity_counts_machines_and_adds_none,
    registered_catalog_is_empty_and_capabilities_follow_the_machines,
    registered_list_holds_every_machine_and_a_failure_is_an_error,
    registered_observe_is_absent_only_after_removal,
    registered_release_refuses_and_changes_nothing,
    registered_verify_instance_matches_only_a_bound_instance, run_all_registered,
};

pub use crate::wait::block_on;

use crate::answer::{Acquired, Claim, Held, HostStatus, Observed, RefusalCode, Released};
use crate::ids::{AcquireId, Hello, HostName, HostRef, SchedulerId};
use crate::provider::{AcquireRequest, CapacityProvider};

/// The scheduler whose machine [`Rig::foreign_machine_exists`] makes.
pub const OTHER_SCHEDULER: &str = "another-scheduler";

/// The acquire id the other scheduler's machine was launched for.
pub const FOREIGN_ACQUIRE: &str = "foreign-acquire";

/// A provider under test and the fake cloud under it, which a case puts in a named state.
///
/// "The cloud" is what the provider calls: an in-memory API for a provider over one, the
/// provider itself for a fake that is its own cloud.
pub trait Rig {
    /// The provider under test, as the rig holds it now.
    fn provider(&self) -> &dyn CapacityProvider;

    /// The scheduler the provider launches for.
    fn scheduler(&self) -> SchedulerId;

    /// An acquire of `acquire` for an offer of the provider's catalog, named as
    /// [`HostName::derive`] names it for [`Rig::scheduler`], that the provider launches
    /// when its cloud has room.
    fn request(&self, acquire: &AcquireId) -> AcquireRequest;

    /// Starts over: an empty cloud with room for `machines` launches and nothing scripted,
    /// and a provider that remembers nothing and, if it bounds the machines it holds (a
    /// `max_machines` or a quota), bounds them to `machines`. Every case begins here.
    fn make_room(&mut self, machines: u32);

    /// The cloud refuses the next launch with the error that means `code` in its own
    /// terms (no capacity, a quota, a request that does not fit, a request it denies), and
    /// launches nothing.
    fn refuse_next(&mut self, code: RefusalCode);

    /// The next launch takes effect, and its answer is lost (a timeout or a reset).
    fn lose_next_answer_that_landed(&mut self);

    /// The next call the provider sends to the cloud, of any kind, gets no answer (a
    /// timeout) and takes no effect.
    fn lose_next_answer_that_did_not_land(&mut self);

    /// The next launch request itself gets no answer (a timeout) and takes no effect; the
    /// calls the provider makes before it work.
    fn lose_next_launch_that_did_not_land(&mut self);

    /// The next terminate takes effect, and the read of the machine the provider makes
    /// after it gets no answer (a timeout).
    fn fail_read_after_next_terminate(&mut self);

    /// Two machines appear in the same account that this scheduler did not launch: one
    /// that [`OTHER_SCHEDULER`] launched for [`FOREIGN_ACQUIRE`] (named and marked as that
    /// scheduler marks its own), and one with no marks and no name. Returns their
    /// references, the other scheduler's first.
    fn foreign_machine_exists(&mut self) -> Vec<HostRef>;

    /// The cloud fails the next list.
    fn fail_list(&mut self);

    /// The cloud drops the machine `id`: a read of it by id answers that it does not
    /// exist.
    fn forget(&mut self, id: &HostRef);

    /// The cloud throttles the next read of one machine by id (a 429).
    fn throttle_reads(&mut self);

    /// Time passes beyond the provider's hold on calls that got no answer, so that an
    /// acquire held back for one is asked of the cloud again.
    fn outwait_unresolved(&mut self);

    /// How many machines the cloud has launched since [`Rig::make_room`].
    fn launches(&self) -> usize;

    /// How many terminate calls have reached the cloud since [`Rig::make_room`].
    fn terminates(&self) -> usize;
}

/// Runs every case against `rig`, in the order of the contract.
pub fn run_all<R: Rig>(rig: &mut R) {
    let rig: &mut dyn Rig = rig;
    acquire_is_idempotent_on_the_acquire_id(rig);
    accepted_means_launched_not_ready(rig);
    refused_codes_are_the_plain_constants(rig);
    unknown_never_launches_twice_and_never_frees(rig);
    unknown_when_the_answer_did_not_land_does_not_free_a_booking(rig);
    an_acquire_that_did_not_land_launches_once_when_retried(rig);
    release_terminates_then_reads_back(rig);
    release_of_a_foreign_reference_sends_nothing(rig);
    list_returns_only_machines_tagged_by_this_scheduler(rig);
    a_failed_list_is_an_error_not_an_empty_list(rig);
    observe_says_absent_only_after_an_explicit_not_found(rig);
    verify_instance_rejects_a_foreign_hello(rig);
    capacity_counts_what_it_supplied(rig);
    capabilities_cover_every_offer_the_catalog_returns(rig);
}

fn acquire_id(text: &str) -> AcquireId {
    AcquireId::new(text).expect("the suite's acquire ids are ids")
}

/// The machine of an answer that must be [`Acquired::Accepted`].
fn accepted(case: &str, answer: &Acquired) -> Held {
    let Acquired::Accepted { held, .. } = answer else {
        panic!("{case}: an acquire the cloud has room for answers {answer:?}, not Accepted");
    };
    held.clone()
}

/// The machine an answer names, when it is [`Acquired::Accepted`].
fn machine_of(answer: &Acquired) -> Option<HostRef> {
    let Acquired::Accepted { held, .. } = answer else {
        return None;
    };
    Some(held.id.clone())
}

/// The constant of an answer, when it is [`Acquired::Refused`].
fn refusal_of(answer: &Acquired) -> Option<RefusalCode> {
    let Acquired::Refused(refusal) = answer else {
        return None;
    };
    Some(refusal.code)
}

/// Launches the machine of `acquire`, which must be accepted.
fn launch(case: &str, rig: &dyn Rig, acquire: &str) -> Held {
    let answer = rig.provider().acquire(&rig.request(&acquire_id(acquire)));
    accepted(case, &answer)
}

/// A hello with the name `name` and no instance id.
fn hello(name: &str) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: None,
        identity: None,
    }
}

/// Two acquires with one id answer `Accepted` with the same machine and the cloud launches
/// once. Fails on a provider that launches on the retry, or derives a new name for it.
pub fn acquire_is_idempotent_on_the_acquire_id(rig: &mut dyn Rig) {
    const CASE: &str = "acquire_is_idempotent_on_the_acquire_id";
    rig.make_room(2);
    assert!(
        rig.provider().can_launch(),
        "{CASE}: a provider that launches says it can"
    );
    let request = rig.request(&acquire_id("idempotent"));
    let first = accepted(CASE, &rig.provider().acquire(&request));
    let second = accepted(CASE, &rig.provider().acquire(&request));
    assert_eq!(
        first.id, second.id,
        "{CASE}: a retry answers the machine of the first acquire"
    );
    let name = Some(request.name.to_string());
    assert_eq!(
        (&first.name, &second.name),
        (&name, &name),
        "{CASE}: both answers carry the name the request gave"
    );
    assert_eq!(rig.launches(), 1, "{CASE}: the cloud launched once");
}

/// Right after `Accepted` the machine is booting, not up, and the first hello of its agent
/// is matched to it by name alone and by name and instance id. Fails on a provider whose
/// `Accepted` waits for readiness, or whose match needs a registered host.
pub fn accepted_means_launched_not_ready(rig: &mut dyn Rig) {
    const CASE: &str = "accepted_means_launched_not_ready";
    rig.make_room(1);
    let request = rig.request(&acquire_id("not-ready"));
    let held = accepted(CASE, &rig.provider().acquire(&request));
    assert_eq!(
        held.status,
        HostStatus::Booting,
        "{CASE}: an accepted machine is booting"
    );
    assert_eq!(
        rig.provider().observe(&held.id),
        Observed::Present(HostStatus::Booting),
        "{CASE}: a read of it by id finds it booting"
    );
    let name = request.name.to_string();
    assert_eq!(
        rig.provider().verify_instance(&hello(&name)),
        Some(held.id.clone()),
        "{CASE}: the first hello by its name is matched to it"
    );
    let with_id = Hello {
        instance_id: Some(held.id.to_string()),
        ..hello(&name)
    };
    assert_eq!(
        rig.provider().verify_instance(&with_id),
        Some(held.id.clone()),
        "{CASE}: the first hello by its name and instance id is matched to it"
    );
}

/// Each refusal of the cloud is answered `Refused` with exactly its constant, and nothing
/// is launched. Fails on a provider that reports one constant as another, such as a quota
/// as capacity (the caller falls through to another option on capacity, and passes over
/// the instance type on quota).
pub fn refused_codes_are_the_plain_constants(rig: &mut dyn Rig) {
    const CASE: &str = "refused_codes_are_the_plain_constants";
    let codes = [
        RefusalCode::Capacity,
        RefusalCode::Quota,
        RefusalCode::Shape,
        RefusalCode::Denied,
    ];
    for code in codes {
        rig.make_room(1);
        rig.refuse_next(code);
        let answer = rig.provider().acquire(&rig.request(&acquire_id("refused")));
        assert_eq!(
            refusal_of(&answer),
            Some(code),
            "{CASE}: a refusal for {code:?} answers {answer:?}"
        );
        assert_eq!(
            rig.launches(),
            0,
            "{CASE}: a {code:?} refusal launches nothing"
        );
        assert_eq!(
            rig.provider().capacity().supplied,
            0,
            "{CASE}: a {code:?} refusal supplies nothing"
        );
    }
}

/// An acquire whose launch landed and whose answer was lost answers `Unknown`; a retry
/// with the same id answers `Deferred` or `Accepted` with the machine that exists, and the
/// cloud launched once. A release that gets no answer is `Unknown` and leaves the machine
/// present and counted. Fails on a provider that retries an `Unknown` as a fresh launch,
/// or reads it as gone.
pub fn unknown_never_launches_twice_and_never_frees(rig: &mut dyn Rig) {
    const CASE: &str = "unknown_never_launches_twice_and_never_frees";
    rig.make_room(2);
    let request = rig.request(&acquire_id("lost-landed"));
    rig.lose_next_answer_that_landed();
    let lost = rig.provider().acquire(&request);
    assert_eq!(lost, Acquired::Unknown, "{CASE}: a lost answer is Unknown");
    let retry = rig.provider().acquire(&request);
    assert!(
        matches!(
            &retry,
            Acquired::Deferred { .. } | Acquired::Accepted { .. }
        ),
        "{CASE}: the retry of an Unknown answers {retry:?}"
    );
    assert_eq!(rig.launches(), 1, "{CASE}: the retry launched nothing");
    let listed = rig.provider().list().unwrap_or_default();
    let name = Some(request.name.to_string());
    let landed: Vec<&Held> = listed.iter().filter(|held| held.name == name).collect();
    assert_eq!(landed.len(), 1, "{CASE}: the landed machine is listed once");
    let existing = landed[0].id.clone();
    assert!(
        [None, Some(existing.clone())].contains(&machine_of(&retry)),
        "{CASE}: an accepted retry answers the machine that exists, not {retry:?}"
    );

    rig.lose_next_answer_that_did_not_land();
    assert_eq!(
        rig.provider().release(&existing),
        Released::Unknown,
        "{CASE}: a release that gets no answer is Unknown"
    );
    assert!(
        matches!(rig.provider().observe(&existing), Observed::Present(_)),
        "{CASE}: after an Unknown release the machine is present"
    );
    assert_eq!(
        rig.provider().capacity().supplied,
        1,
        "{CASE}: after an Unknown release the machine is still supplied"
    );
}

/// A read by id of a booked machine that times out is `Unknown`, never `Absent`, the next
/// read finds the machine again, and it is still supplied. Fails on a provider that reads
/// a timeout as not-found.
pub fn unknown_when_the_answer_did_not_land_does_not_free_a_booking(rig: &mut dyn Rig) {
    const CASE: &str = "unknown_when_the_answer_did_not_land_does_not_free_a_booking";
    rig.make_room(2);
    let booked = launch(CASE, rig, "booked");
    rig.lose_next_answer_that_did_not_land();
    assert_eq!(
        rig.provider().observe(&booked.id),
        Observed::Unknown,
        "{CASE}: a read that times out is Unknown"
    );
    assert!(
        matches!(rig.provider().observe(&booked.id), Observed::Present(_)),
        "{CASE}: the next read finds the machine"
    );
    assert_eq!(
        rig.provider().capacity().supplied,
        1,
        "{CASE}: the booked machine is still supplied"
    );
}

/// An acquire whose call timed out without effect answers `Unknown` (or `Deferred`, for a
/// provider whose timed-out call came before the launch) and launches nothing; once the
/// provider's hold on it has passed, an acquire with the same id launches exactly once,
/// and a further one answers that machine. Fails on a provider that reads a timed-out
/// acquire as launched, or launches twice once it is retried.
pub fn an_acquire_that_did_not_land_launches_once_when_retried(rig: &mut dyn Rig) {
    const CASE: &str = "an_acquire_that_did_not_land_launches_once_when_retried";
    rig.make_room(1);
    let request = rig.request(&acquire_id("did-not-land"));
    rig.lose_next_launch_that_did_not_land();
    let answer = rig.provider().acquire(&request);
    assert_eq!(
        answer,
        Acquired::Unknown,
        "{CASE}: a launch that timed out is Unknown"
    );
    assert_eq!(
        rig.launches(),
        0,
        "{CASE}: the timed-out launch made nothing"
    );
    let held_back = rig.provider().capacity();
    assert_eq!(
        (held_back.supplied, held_back.can_add.unwrap_or(0)),
        (0, 0),
        "{CASE}: the unanswered launch keeps its room, {held_back:?}"
    );
    let early = rig.provider().acquire(&request);
    assert!(
        matches!(&early, Acquired::Deferred { .. }),
        "{CASE}: a retry within the hold is deferred, not {early:?}"
    );
    assert_eq!(
        rig.launches(),
        0,
        "{CASE}: the early retry launched nothing"
    );
    rig.outwait_unresolved();
    let launched = accepted(CASE, &rig.provider().acquire(&request));
    assert_eq!(
        rig.launches(),
        1,
        "{CASE}: the retry after the hold launched once"
    );
    let again = accepted(CASE, &rig.provider().acquire(&request));
    assert_eq!(
        again.id, launched.id,
        "{CASE}: a further acquire answers that machine"
    );
    assert_eq!(rig.launches(), 1, "{CASE}: and launches nothing");
}

/// A release terminates the machine and reports what a read after the terminate found:
/// when that read gets no answer the release is `Unknown`, whatever the terminate said;
/// when it works, a read by id afterwards agrees with what the release reported, and the
/// machine is terminating, terminated or absent, never still up. Fails on a provider that
/// reports from the terminate call alone.
pub fn release_terminates_then_reads_back(rig: &mut dyn Rig) {
    const CASE: &str = "release_terminates_then_reads_back";
    rig.make_room(2);
    let unread = launch(CASE, rig, "unread");
    rig.fail_read_after_next_terminate();
    let answer = rig.provider().release(&unread.id);
    assert_eq!(
        answer,
        Released::Unknown,
        "{CASE}: a release whose read after the terminate got no answer is Unknown"
    );
    let held = launch(CASE, rig, "released");
    let released = rig.provider().release(&held.id);
    let observed = rig.provider().observe(&held.id);
    let read_back = [
        (
            Released::Terminating(HostStatus::Terminating),
            Observed::Present(HostStatus::Terminating),
        ),
        (
            Released::Terminating(HostStatus::Terminated),
            Observed::Present(HostStatus::Terminated),
        ),
        (Released::Gone, Observed::Absent),
    ];
    let pair = (released.clone(), observed.clone());
    assert!(
        read_back.contains(&pair),
        "{CASE}: a release that answers {released:?} is followed by a read of {observed:?}; \
         it must be terminating, terminated or gone, and the read must agree"
    );
    assert_eq!(
        rig.terminates(),
        2,
        "{CASE}: each release sent one terminate"
    );
}

/// A release of a reference that is not one of this provider's machines (text that is no
/// id of it, another scheduler's machine, a machine with no marks) answers
/// `Refused(Foreign)`, and no terminate reaches the cloud. Fails on a provider that sends a
/// foreign id to the cloud.
pub fn release_of_a_foreign_reference_sends_nothing(rig: &mut dyn Rig) {
    const CASE: &str = "release_of_a_foreign_reference_sends_nothing";
    rig.make_room(1);
    launch(CASE, rig, "ours");
    let mut foreign = rig.foreign_machine_exists();
    foreign.push(HostRef("not an id: of any machine".to_owned()));
    for id in &foreign {
        assert_eq!(
            rig.provider().release(id),
            Released::Refused(Claim::Foreign),
            "{CASE}: the release of {id}"
        );
    }
    assert_eq!(
        rig.terminates(),
        0,
        "{CASE}: no terminate reached the cloud"
    );
}

/// With another scheduler's machine and a machine with no marks in the account, a list
/// returns only the machines this scheduler launched, each carrying its prefix. Fails on a
/// provider that lists by account, not by its marks.
pub fn list_returns_only_machines_tagged_by_this_scheduler(rig: &mut dyn Rig) {
    const CASE: &str = "list_returns_only_machines_tagged_by_this_scheduler";
    rig.make_room(2);
    let ours = launch(CASE, rig, "listed");
    rig.foreign_machine_exists();
    let listed = rig.provider().list().unwrap_or_default();
    let ids: Vec<&HostRef> = listed.iter().map(|held| &held.id).collect();
    assert_eq!(ids, [&ours.id], "{CASE}: the list holds only our machine");
    let prefix = HostName::prefix(&rig.scheduler());
    let marked = listed.iter().all(|held| {
        held.name
            .as_deref()
            .is_some_and(|name| name.starts_with(&prefix))
    });
    assert!(
        marked,
        "{CASE}: every listed machine carries the prefix {prefix}"
    );
}

/// A list that fails is an error while a machine exists, never an empty list. Fails on a
/// provider that turns a failure into `Ok(vec![])`.
pub fn a_failed_list_is_an_error_not_an_empty_list(rig: &mut dyn Rig) {
    const CASE: &str = "a_failed_list_is_an_error_not_an_empty_list";
    rig.make_room(1);
    launch(CASE, rig, "listed");
    rig.fail_list();
    let listed = rig.provider().list();
    assert!(listed.is_err(), "{CASE}: a failed list answers {listed:?}");
}

/// A read by id answers `Absent` only after the cloud said the machine does not exist: a
/// throttled read is `Unknown`, an existing machine is `Present`, and only the absent one
/// is gone. Fails on a provider that maps any non-answer to `Absent`.
pub fn observe_says_absent_only_after_an_explicit_not_found(rig: &mut dyn Rig) {
    const CASE: &str = "observe_says_absent_only_after_an_explicit_not_found";
    rig.make_room(2);
    let kept = launch(CASE, rig, "kept");
    let dropped = launch(CASE, rig, "dropped");
    assert!(
        matches!(rig.provider().observe(&kept.id), Observed::Present(_)),
        "{CASE}: an existing machine is present"
    );
    rig.forget(&dropped.id);
    assert_eq!(
        rig.provider().observe(&dropped.id),
        Observed::Absent,
        "{CASE}: a machine the cloud says does not exist is absent"
    );
    assert!(
        rig.provider().is_gone(&dropped.id),
        "{CASE}: an absent machine is gone"
    );
    rig.throttle_reads();
    assert_eq!(
        rig.provider().observe(&kept.id),
        Observed::Unknown,
        "{CASE}: a throttled read is Unknown"
    );
    assert!(
        !rig.provider().is_gone(&kept.id),
        "{CASE}: a present machine is not gone"
    );
}

/// A hello is matched only to a machine this provider launched: a name another scheduler
/// derives, a name this scheduler derives for an acquire never launched, the other
/// scheduler's machine that exists, and our machine's name with another instance id are
/// all `None`. Fails on a provider that matches by prefix alone.
pub fn verify_instance_rejects_a_foreign_hello(rig: &mut dyn Rig) {
    const CASE: &str = "verify_instance_rejects_a_foreign_hello";
    rig.make_room(1);
    let request = rig.request(&acquire_id("verified"));
    let ours = accepted(CASE, &rig.provider().acquire(&request));
    let foreign = rig.foreign_machine_exists();
    let other = SchedulerId::new(OTHER_SCHEDULER).expect("the other scheduler's id is an id");
    let theirs = HostName::derive(&other, &acquire_id(FOREIGN_ACQUIRE)).to_string();
    let elsewhere = HostName::derive(&other, &acquire_id("verified")).to_string();
    let never = HostName::derive(&rig.scheduler(), &acquire_id("never-launched")).to_string();
    let wrong_id = Hello {
        instance_id: foreign.first().map(ToString::to_string),
        ..hello(&request.name.to_string())
    };
    let their_own = Hello {
        instance_id: foreign.first().map(ToString::to_string),
        ..hello(&theirs)
    };
    for refused in [
        hello(&theirs),
        hello(&elsewhere),
        hello(&never),
        wrong_id,
        their_own,
    ] {
        assert_eq!(
            rig.provider().verify_instance(&refused),
            None,
            "{CASE}: {refused:?} is not a machine of this provider"
        );
    }
    assert_eq!(
        rig.provider()
            .verify_instance(&hello(&request.name.to_string())),
        Some(ours.id),
        "{CASE}: our machine's hello is matched"
    );
}

/// After two accepted acquires the provider supplies two, and with its room used up it
/// can add none (or knows no bound); a release takes one off, and the count is the
/// provider's own: a list that would fail does not change it. Fails on a provider that
/// reads the count from the cloud on every call, or does not update it on acquire and
/// release.
pub fn capacity_counts_what_it_supplied(rig: &mut dyn Rig) {
    const CASE: &str = "capacity_counts_what_it_supplied";
    rig.make_room(2);
    let empty = rig.provider().capacity();
    assert_eq!(
        empty.supplied, 0,
        "{CASE}: an empty provider supplies nothing"
    );
    assert!(
        matches!(empty.can_add, Some(2) | None),
        "{CASE}: with room for two it can add {:?}",
        empty.can_add
    );
    let first = launch(CASE, rig, "first");
    launch(CASE, rig, "second");
    let full = rig.provider().capacity();
    assert_eq!(full.supplied, 2, "{CASE}: two accepted acquires supply two");
    assert!(
        matches!(full.can_add, Some(0) | None),
        "{CASE}: with its room used up it can add {:?}",
        full.can_add
    );
    assert!(
        matches!(
            rig.provider().release(&first.id),
            Released::Terminating(_) | Released::Gone
        ),
        "{CASE}: the first machine is released"
    );
    assert_eq!(
        rig.provider().capacity().supplied,
        1,
        "{CASE}: a released machine is no longer supplied"
    );
    rig.fail_list();
    assert_eq!(
        rig.provider().capacity().supplied,
        1,
        "{CASE}: the count is the provider's own, whatever the cloud would answer"
    );
}

/// Every offer of the catalog is inside the capabilities: its operating system,
/// architecture and GPU class. It holds for a catalog whose architectures are ones the
/// provider's cloud documents; a rig's catalog holds more than one architecture. Fails on a provider whose capability table and catalog are
/// two lists that drift.
pub fn capabilities_cover_every_offer_the_catalog_returns(rig: &mut dyn Rig) {
    const CASE: &str = "capabilities_cover_every_offer_the_catalog_returns";
    rig.make_room(1);
    let caps = rig.provider().capabilities();
    let offers = block_on(rig.provider().catalog()).unwrap_or_default();
    assert!(!offers.is_empty(), "{CASE}: the rig's catalog has offers");
    for offer in &offers {
        assert!(caps.covers(offer), "{CASE}: {offer:?} is outside {caps:?}");
    }
}
