//! The Lambda view of the watcher over the in-memory account: what it keeps, and that it
//! only reads.

use std::sync::Arc;

use js_elastic::{AcquireId, HostName, HostStatus, SchedulerId};
use js_provider_lambdalabs::{
    ACQUIRE_TAG, ApiError, Architecture, Call, FakeLambda, Instance, InstanceId, InstanceTypeOffer,
    LambdaRead, Reader, Region, SCHEDULER_TAG, Tag,
};
use js_watch::{CloudView, LambdaView, StrayKind};

const SCHEDULER: &str = "sched-one";

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn instance(
    id: &str,
    name: Option<&str>,
    region: &str,
    status: HostStatus,
    tags: Vec<Tag>,
) -> Instance {
    Instance {
        id: InstanceId::new(id).unwrap(),
        name: name.map(str::to_owned),
        status,
        region: region.to_owned(),
        instance_type: "gpu_1x_sample".to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags,
    }
}

fn ours(acquire: &str, id: &str, region: &str, status: HostStatus) -> Instance {
    let name = HostName::derive(
        &SchedulerId::new(SCHEDULER).unwrap(),
        &AcquireId::new(acquire).unwrap(),
    );
    let tags = vec![tag(SCHEDULER_TAG, SCHEDULER), tag(ACQUIRE_TAG, acquire)];
    instance(id, Some(name.as_str()), region, status, tags)
}

fn account() -> Arc<FakeLambda> {
    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(vec![InstanceTypeOffer {
        name: "gpu_1x_sample".to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: 75,
        vcpus: 8,
        memory_gib: 64,
        storage_gib: 100,
        gpus: 1,
        architecture: Architecture::X86_64,
        regions: vec![Region {
            name: "region-a".to_owned(),
            description: String::new(),
        }],
    }]);
    fake.add_instance(ours("a", "run-a", "region-a", HostStatus::Active));
    fake.add_instance(ours("b", "run-b", "region-z", HostStatus::Booting));
    fake.add_instance(ours("c", "ending", "region-a", HostStatus::Terminating));
    fake.add_instance(instance(
        "half",
        Some("renamed"),
        "region-y",
        HostStatus::Active,
        vec![tag(SCHEDULER_TAG, SCHEDULER)],
    ));
    fake
}

fn view(fake: &Arc<FakeLambda>) -> LambdaView {
    let api: Arc<dyn LambdaRead> = fake.clone();
    LambdaView::new(Reader::new(api, SchedulerId::new(SCHEDULER).unwrap()))
}

fn down() -> ApiError {
    ApiError::Transport("down".to_owned())
}

/// Asserts that the view lists the scheduler's running and booting instances in every
/// region with their names, leaves out the one already terminating (it is going, and
/// listing it would be a false orphan at every scale-down), and names itself `lambda`. It
/// catches a view that reads one region and one that reports the machines the server has
/// just released.
#[test]
fn the_instances_are_the_running_ones_of_the_scheduler_in_every_region() {
    let fake = account();
    let view = view(&fake);
    assert_eq!(view.cloud(), "lambda");
    let seen = view.instances().unwrap();
    let ids: Vec<&str> = seen.iter().map(|each| each.id.as_str()).collect();
    assert_eq!(ids, ["run-a", "run-b"]);
    assert_eq!(seen[1].region, "region-z");
    assert!(seen[0].name.as_deref().unwrap().starts_with("js-"));
}

/// Asserts that the leftovers are the instances with some of the marks, with the region
/// they are in and the catalog's price; that a catalog that cannot be read only costs the
/// price; and that a list that cannot be read is an error. It catches a view that drops a
/// half-marked instance that still bills, and one that fails the sweep for a missing price.
#[test]
fn the_strays_are_the_half_marked_instances_priced_when_the_catalog_answers() {
    let fake = account();
    let view = view(&fake);
    let strays = view.strays().unwrap();
    assert_eq!(strays.len(), 1);
    assert_eq!(strays[0].kind, StrayKind::Instance);
    assert_eq!(
        (strays[0].id.as_str(), strays[0].region.as_str()),
        ("half", "region-y")
    );
    assert_eq!(strays[0].cents_per_hour, Some(75));

    fake.fail_next(Call::InstanceTypes, down());
    assert_eq!(view.strays().unwrap()[0].cents_per_hour, None);

    fake.fail_next(Call::ListInstances, down());
    assert!(view.strays().is_err());
    fake.fail_next(Call::ListInstances, down());
    assert!(view.instances().is_err());
    // Everything the view did was a read.
    assert_eq!(fake.count(Call::Launch) + fake.count(Call::Terminate), 0);
}
