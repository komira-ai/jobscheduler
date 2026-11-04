//! The Lambda provider over the in-memory account against the provider contract's
//! conformance suite, and its own answers to the calls the suite leaves to each provider:
//! its name, what it offers, what it counts and that it sends no notices.

use std::collections::BTreeSet;
use std::time::Duration;

use js_elastic::conformance::{FOREIGN_ACQUIRE, OTHER_SCHEDULER, Rig as Conformance, run_all};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, Capacity, CapacityProvider, GpuClass, Hello,
    HostName, HostRef, HostStatus, Observed, ProviderCaps, RefusalCode, SchedulerId,
};
use js_provider_lambdalabs::{
    ACQUIRE_TAG, ApiError, Architecture, Call, ErrorCode, POOL_TAG, PROVIDER_ID, SCHEDULER_TAG,
};
use js_types::ProviderId;

use crate::rig::{Rig, SCHEDULER, ask, candidate, host, id, live, offer, rig, rig_with, row, tag};

/// The provider under the suite: a fresh account and provider for each case, with room
/// for as many machines as `max_machines` allows and no space between launches, whose
/// catalog holds an x86_64 type and an arm64 one.
struct LambdaRig(Rig);

impl LambdaRig {
    fn fresh(machines: usize) -> Rig {
        let fresh = rig(live(machines).launch_interval(Duration::ZERO));
        fresh.fake.set_offers(vec![
            offer("gpu_1x_sample", 75, 1, Architecture::X86_64, &["region-a"]),
            offer(
                "gpu_8x_example",
                1200,
                8,
                Architecture::Arm64,
                &["region-a"],
            ),
        ]);
        fresh
    }
}

/// The error the API answers a launch with for `code`.
fn refusal(code: RefusalCode) -> ApiError {
    let (status, code) = match code {
        RefusalCode::Capacity => (400, ErrorCode::InsufficientCapacity),
        RefusalCode::Quota => (400, ErrorCode::QuotaExceeded),
        RefusalCode::Shape => (400, ErrorCode::InvalidParameters),
        RefusalCode::Denied => (403, ErrorCode::Forbidden),
    };
    ApiError::Api {
        status,
        code,
        request_id: None,
    }
}

impl Conformance for LambdaRig {
    fn provider(&self) -> &dyn CapacityProvider {
        &self.0.provider
    }

    fn scheduler(&self) -> SchedulerId {
        SchedulerId::new(SCHEDULER).unwrap()
    }

    fn request(&self, acquire: &AcquireId) -> AcquireRequest {
        AcquireRequest {
            name: HostName::derive(&self.scheduler(), acquire),
            acquire: acquire.clone(),
            pool: "pool-a".to_owned(),
            lifetime: None,
            offer: candidate("region-a"),
            labels: Vec::new(),
            payload: BootPayload::new("user-data".to_owned()),
        }
    }

    fn make_room(&mut self, machines: u32) {
        self.0 = Self::fresh(usize::try_from(machines).unwrap());
    }

    fn refuse_next(&mut self, code: RefusalCode) {
        self.0.fake.fail_next(Call::Launch, refusal(code));
    }

    fn lose_next_answer_that_landed(&mut self) {
        let reset = ApiError::Transport("the connection was reset".to_owned());
        self.0.fake.lose_response(Call::Launch, reset);
    }

    fn lose_next_answer_that_did_not_land(&mut self) {
        let timeout = ApiError::Transport("the call timed out".to_owned());
        self.0.fake.fail_next_call(timeout);
    }

    fn lose_next_launch_that_did_not_land(&mut self) {
        let timeout = ApiError::Transport("the launch timed out".to_owned());
        self.0.fake.fail_next(Call::Launch, timeout);
    }

    fn fail_read_after_next_terminate(&mut self) {
        // The read before the terminate works; the read after it does not.
        self.0.fake.pass_next(Call::GetInstance);
        let timeout = ApiError::Transport("the read timed out".to_owned());
        self.0.fake.fail_next(Call::GetInstance, timeout);
    }

    fn foreign_machine_exists(&mut self) -> Vec<HostRef> {
        let other = SchedulerId::new(OTHER_SCHEDULER).unwrap();
        let acquire = AcquireId::new(FOREIGN_ACQUIRE).unwrap();
        let name = HostName::derive(&other, &acquire).to_string();
        let marks = vec![
            tag(SCHEDULER_TAG, OTHER_SCHEDULER),
            tag(ACQUIRE_TAG, FOREIGN_ACQUIRE),
            tag(POOL_TAG, "pool-a"),
        ];
        let theirs = row("foreign0001", Some(&name), HostStatus::Active, marks);
        self.0.fake.add_instance(theirs);
        let unmarked = row("unmarked0001", None, HostStatus::Active, Vec::new());
        self.0.fake.add_instance(unmarked);
        vec![host("foreign0001"), host("unmarked0001")]
    }

    fn fail_list(&mut self) {
        self.0.fake.fail_next(Call::ListInstances, refusal_500());
    }

    fn forget(&mut self, gone: &HostRef) {
        self.0.fake.remove_instance(&id(gone.as_str()));
    }

    fn throttle_reads(&mut self) {
        let throttled = ApiError::RateLimited {
            retry_after: None,
            request_id: None,
        };
        self.0.fake.fail_next(Call::GetInstance, throttled);
    }

    fn outwait_unresolved(&mut self) {
        self.0.clock.advance(Duration::from_secs(24 * 60 * 60));
    }

    fn launches(&self) -> usize {
        self.0.fake.launched()
    }

    fn terminates(&self) -> usize {
        self.0.fake.count(Call::Terminate)
    }
}

fn refusal_500() -> ApiError {
    ApiError::Api {
        status: 500,
        code: ErrorCode::InternalError,
        request_id: None,
    }
}

/// Runs every case of the suite against the provider over the in-memory account. Each
/// case names the defect it catches in the provider (`js_elastic::conformance`).
#[test]
fn the_lambda_provider_passes_the_conformance_suite() {
    run_all(&mut LambdaRig(LambdaRig::fresh(1)));
}

/// Asserts that the provider sends no notices: Lambda's reclaims show in a list. It
/// catches a stream that would never send anything being handed out as if it did.
#[test]
fn lambda_has_no_event_stream() {
    let rig = LambdaRig::fresh(1);
    assert!(rig.provider.events().is_none());
    assert_eq!(rig.provider.id(), PROVIDER_ID);
    assert_eq!(rig.provider.id().as_str(), "lambdalabs");
    assert_eq!(ProviderId::new("lambdalabs"), Ok(PROVIDER_ID));
    assert!(rig.provider.can_launch());
}

/// Asserts the capabilities: Linux, both documented architectures as the catalog spells
/// them, every GPU class of the option table and none other, a whole machine and host
/// ports. They are read from the table, so a table of two classes offers both. It catches
/// capabilities kept as a second list beside the table the catalog reads.
#[test]
fn the_capabilities_are_the_option_tables_classes_on_linux() {
    let table = r#"
[[option]]
instance_type = "gpu_1x_sample"
gpu_class = "nvidia-sample"
gpus = 1

[[option]]
instance_type = "gpu_8x_example"
gpu_class = "nvidia-example"
gpus = 8
"#;
    let rig = rig_with(live(1), table);
    let class = |text: &str| GpuClass::parse(text).unwrap();
    let expected = ProviderCaps {
        os: BTreeSet::from(["linux".to_owned()]),
        arch: BTreeSet::from(["arm64".to_owned(), "x86_64".to_owned()]),
        accelerators: BTreeSet::from([class("nvidia-example"), class("nvidia-sample")]),
        whole_host: true,
        host_ports: true,
    };
    assert_eq!(rig.provider.capabilities(), expected);
    let offers = rig.provider.catalog().unwrap();
    assert!(offers.iter().all(|offer| expected.covers(offer)));
}

/// Asserts what `capacity` counts against `max_machines`: a launched instance is supplied
/// and counted, from the launch's answer and with no call of its own; an instance with
/// some of our marks, not all, counts and is not supplied (a terminated one neither); an
/// instance a list shows terminated is neither supplied nor counted. It catches a bound
/// that leaves out what the launch guard counts, which would promise launches the guard
/// refuses, and a count read from the API on every call.
#[test]
fn capacity_counts_what_the_launch_guard_counts() {
    let rig = LambdaRig::fresh(4);
    assert!(matches!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
    let calls = rig.fake.calls().len();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(3)
        }
    );
    assert_eq!(rig.fake.calls().len(), calls, "capacity makes no call");
    let own = HostName::prefix(&SchedulerId::new(SCHEDULER).unwrap());
    let named = format!("{own}0123456789ab");
    for (text, status) in [
        ("suspect0001", HostStatus::Active),
        ("suspect0002", HostStatus::Terminated),
    ] {
        rig.fake
            .add_instance(row(text, Some(&named), status, Vec::new()));
    }
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(2)
        }
    );
    rig.fake.set_status(&id("fake0001"), HostStatus::Terminated);
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(3)
        }
    );
}

/// Asserts what a read by id tells the records: a terminated instance is present and
/// terminated, and no longer supplied; one the API says is gone is forgotten, so a hello
/// by its name is matched to nothing; a reference that is no id of the API is not read
/// and is `Unknown`. It catches a read that leaves the records stale, and one that reads
/// a reference the API cannot hold.
#[test]
fn a_read_by_id_updates_what_the_provider_holds() {
    let rig = LambdaRig::fresh(2);
    let request = ask("a1");
    assert!(matches!(
        rig.provider.acquire(&request),
        Acquired::Accepted { .. }
    ));
    let machine = host("fake0001");
    rig.fake.set_status(&id("fake0001"), HostStatus::Terminated);
    assert_eq!(
        rig.provider.observe(&machine),
        Observed::Present(HostStatus::Terminated)
    );
    assert_eq!(rig.provider.capacity().supplied, 0);
    let hello = Hello {
        name: request.name.to_string(),
        instance_id: Some("fake0001".to_owned()),
    };
    assert_eq!(rig.provider.verify_instance(&hello), Some(machine.clone()));
    rig.fake.remove_instance(&id("fake0001"));
    assert_eq!(rig.provider.observe(&machine), Observed::Absent);
    assert_eq!(rig.provider.verify_instance(&hello), None);
    let gets = rig.fake.count(Call::GetInstance);
    assert_eq!(
        rig.provider.observe(&HostRef("not an id".to_owned())),
        Observed::Unknown
    );
    assert_eq!(rig.fake.count(Call::GetInstance), gets);
}

/// Asserts that a hello is matched to an instance this scheduler launched that only a
/// list knows of (a provider that restarted): the list is a record as a launch is. It
/// catches a match that knows only the launches of this run.
#[test]
fn a_hello_is_matched_to_a_listed_instance() {
    let launched = LambdaRig::fresh(2);
    let request = ask("a1");
    assert!(matches!(
        launched.provider.acquire(&request),
        Acquired::Accepted { .. }
    ));
    let restarted = LambdaRig::fresh(2);
    for instance in launched.fake.instances() {
        restarted.fake.add_instance(instance);
    }
    let hello = Hello {
        name: request.name.to_string(),
        instance_id: None,
    };
    assert_eq!(restarted.provider.verify_instance(&hello), None);
    restarted.provider.list().unwrap();
    assert_eq!(
        restarted.provider.verify_instance(&hello),
        Some(host("fake0001"))
    );
}
