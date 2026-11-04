//! The fake cloud against the provider contract's conformance suite, and the fake's own
//! answers to the calls the suite leaves to each provider: its name, what it offers, the
//! bound it knows and its reclaim notices.

mod common;

use std::collections::BTreeSet;

use common::{PRICE, START, cloud, launch, offer};
use js_elastic::conformance::{FOREIGN_ACQUIRE, OTHER_SCHEDULER, Rig, run_all};
use js_elastic::{
    AcquireId, AcquireRequest, BootPayload, Capacity, CapacityProvider, GpuClass, HostName,
    HostRef, Observed, ProviderCaps, ProviderEvent, RefusalCode, SchedulerId,
};
use js_elastic_fake::{Billing, Call, ElasticFake, FakeClock, PROVIDER_ID, Step};
use js_types::ProviderId;

/// The fake under the suite: a fresh cloud for each case, whose quota is the room and
/// whose catalog holds an x86_64 offer and an aarch64 one.
struct FakeRig {
    fake: ElasticFake,
}

impl Rig for FakeRig {
    fn provider(&self) -> &dyn CapacityProvider {
        &self.fake
    }

    fn scheduler(&self) -> SchedulerId {
        SchedulerId::new("sched").unwrap()
    }

    fn request(&self, acquire: &AcquireId) -> AcquireRequest {
        AcquireRequest {
            name: HostName::derive(&self.scheduler(), acquire),
            acquire: acquire.clone(),
            pool: "gpu".to_owned(),
            offer: offer(Some(PRICE)),
            lifetime: None,
            labels: Vec::new(),
            payload: BootPayload::new("user-data".to_owned()),
        }
    }

    fn make_room(&mut self, machines: u32) {
        let mut arm = offer(Some(PRICE));
        (arm.id, arm.arch) = ("a10-arm".to_owned(), "aarch64".to_owned());
        let offers = vec![offer(Some(PRICE)), arm];
        self.fake = ElasticFake::new(FakeClock::new(START), Billing::Lambda, offers);
        let mut knobs = self.fake.knobs();
        knobs.quota = Some(usize::try_from(machines).unwrap());
        self.fake.set_knobs(knobs);
    }

    fn refuse_next(&mut self, code: RefusalCode) {
        self.fake.script(Call::Acquire, &[Step::Refuse(code)]);
    }

    fn lose_next_answer_that_landed(&mut self) {
        self.fake
            .script(Call::Acquire, &[Step::Lost { landed: true }]);
    }

    fn lose_next_answer_that_did_not_land(&mut self) {
        self.fake.script_next(Step::Lost { landed: false });
    }

    fn lose_next_launch_that_did_not_land(&mut self) {
        self.fake
            .script(Call::Acquire, &[Step::Lost { landed: false }]);
    }

    fn fail_read_after_next_terminate(&mut self) {
        // The fake's release is one call: one that lands and gets no answer is this.
        self.fake
            .script(Call::Release, &[Step::Lost { landed: true }]);
    }

    fn foreign_machine_exists(&mut self) -> Vec<HostRef> {
        let other = SchedulerId::new(OTHER_SCHEDULER).unwrap();
        let acquire = AcquireId::new(FOREIGN_ACQUIRE).unwrap();
        let name = HostName::derive(&other, &acquire).to_string();
        vec![
            self.fake.add_foreign(Some(&name), offer(Some(PRICE))),
            self.fake.add_foreign(None, offer(Some(PRICE))),
        ]
    }

    fn fail_list(&mut self) {
        self.fake
            .script(Call::List, &[Step::Lost { landed: false }]);
    }

    fn forget(&mut self, id: &HostRef) {
        self.fake.drop_machine(id);
    }

    fn throttle_reads(&mut self) {
        self.fake.script(Call::IsGone, &[Step::Throttled]);
    }

    fn outwait_unresolved(&mut self) {
        self.fake.clock().advance(self.fake.knobs().unknown_hold);
    }

    fn launches(&self) -> usize {
        self.fake.launched()
    }

    fn terminates(&self) -> usize {
        self.fake.released().len()
    }
}

/// Runs every case of the suite against the fake. Each case names the defect it catches
/// in the provider (`js_elastic::conformance`).
#[test]
fn the_fake_passes_the_conformance_suite() {
    run_all(&mut FakeRig {
        fake: cloud(Billing::Lambda),
    });
}

/// Asserts that a stream taken from the fake sees `Reclaimed` for a machine it preempts,
/// once, and nothing for a second preemption of it, for a machine a release already ended
/// or for an id it does not hold; the stream ends when the cloud is dropped. It catches a
/// preemption that sends no notice, and one that sends a notice for a machine that was not
/// reclaimed.
#[test]
fn fake_events_report_a_reclaim() {
    let cloud = cloud(Billing::Lambda);
    let reclaimed = launch(&cloud, "a1");
    let released = launch(&cloud, "a2");
    let events = cloud.events().unwrap();
    drop(cloud.events());
    cloud.preempt(&reclaimed);
    cloud.preempt(&reclaimed);
    cloud.release(&released);
    cloud.preempt(&released);
    cloud.preempt(&HostRef("nobody".to_owned()));
    assert_eq!(cloud.count(Call::Events), 2);
    drop(cloud);
    let notices: Vec<ProviderEvent> = events.collect();
    assert_eq!(notices, [ProviderEvent::Reclaimed(reclaimed)]);
}

/// Asserts the fake's own answers: its id is `fake`, it can launch, its capabilities are
/// the strings of its offers with a whole machine and host ports, and with no quota it
/// supplies what runs and knows no bound. It catches capabilities that are not read from
/// the offers, and a bound made up where the fake has none.
#[test]
fn the_fake_names_itself_and_offers_what_its_catalog_lists() {
    let cloud = cloud(Billing::Lambda);
    assert_eq!(cloud.id(), PROVIDER_ID);
    assert_eq!(cloud.id().as_str(), "fake");
    assert_eq!(ProviderId::new(cloud.id().as_str()), Ok(PROVIDER_ID));
    assert!(cloud.can_launch());
    let expected = ProviderCaps {
        os: BTreeSet::from(["linux".to_owned()]),
        arch: BTreeSet::from(["x86_64".to_owned()]),
        accelerators: BTreeSet::from([GpuClass::parse("nvidia-a10").unwrap()]),
        whole_host: true,
        host_ports: true,
    };
    assert_eq!(cloud.capabilities(), expected);
    launch(&cloud, "a1");
    assert_eq!(
        cloud.capacity(),
        Capacity {
            supplied: 1,
            can_add: None
        }
    );
    let counted = [
        Call::Id,
        Call::CanLaunch,
        Call::Capabilities,
        Call::Capacity,
    ];
    assert!(counted.iter().all(|call| cloud.count(*call) > 0));
}

/// Asserts what a machine the account holds beside the caller's does: it counts toward
/// the quota (a launch is refused once it fills it, and nothing more can be added) but is
/// not supplied, and a read by id finds it. It catches a quota that counts only the
/// caller's machines, which a real account does not.
#[test]
fn a_foreign_machine_counts_toward_the_quota_and_is_not_supplied() {
    let cloud = cloud(Billing::Lambda);
    let mut knobs = cloud.knobs();
    knobs.quota = Some(1);
    cloud.set_knobs(knobs);
    let foreign = cloud.add_foreign(None, offer(None));
    assert_eq!(
        cloud.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(0)
        }
    );
    assert!(matches!(cloud.observe(&foreign), Observed::Present(_)));
    assert!(
        cloud.truth().is_empty(),
        "the truth is the caller's machines"
    );
    assert_eq!(cloud.running(), [foreign]);
}

/// Asserts that dropping a machine ends it and makes a read by id absent, and that
/// dropping an id the cloud does not hold changes nothing. It catches a drop that leaves
/// the machine billing.
#[test]
fn a_dropped_machine_is_absent_and_an_unknown_id_changes_nothing() {
    let cloud = cloud(Billing::Lambda);
    let id = launch(&cloud, "a1");
    cloud.drop_machine(&HostRef("nobody".to_owned()));
    assert_eq!(cloud.running(), std::slice::from_ref(&id));
    cloud.drop_machine(&id);
    assert!(cloud.running().is_empty());
    assert_eq!(cloud.observe(&id), Observed::Absent);
    assert!(cloud.released().is_empty(), "a drop is not a release");
}
