//! What a list returns, and what it tells the provider's records.

use std::sync::{Arc, Mutex};

use js_elastic::{Acquired, Capacity, CapacityProvider, Held, HostStatus, ProviderError};
use js_provider_gce::{
    AcceleratorTypeInfo, ApiError, GceApi, GceProvider, InsertRequest, Instance, InstanceRef,
    InstanceStatus, ListFilter, MachineTypeInfo, ManualClock, Method, Operation, OperationRef,
    Page, Project, QuotaMetric, Region, SkuPrice, Zone,
};

use crate::rig::{
    Account, PROJECT, Rig, SCHEDULER, ZONE, ask, host, label, live, marks, name_of, ours, rig, row,
    settings, stranger, trace_everything,
};

fn accepted(answer: Acquired) -> Held {
    match answer {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

/// A project that writes down every list filter it is asked for and can answer the same
/// next-page token for ever. Every other call, and the list itself, go to the account.
struct Spy {
    inner: Arc<Account>,
    filters: Mutex<Vec<ListFilter>>,
    repeat: bool,
}

impl Spy {
    fn filters(&self) -> Vec<ListFilter> {
        self.filters.lock().unwrap().clone()
    }
}

impl GceApi for Spy {
    fn machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError> {
        self.inner.machine_types(project, zones)
    }

    fn accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
        self.inner.accelerator_types(project, zones)
    }

    fn prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError> {
        self.inner.prices(region)
    }

    fn region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError> {
        self.inner.region_quotas(project, region)
    }

    fn insert_instance(&self, request: &InsertRequest) -> Result<Operation, ApiError> {
        self.inner.insert_instance(request)
    }

    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        self.inner.get_instance(instance)
    }

    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        self.inner.delete_instance(instance)
    }

    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        self.filters.lock().unwrap().push(filter.clone());
        let page = self.inner.list_instances(filter)?;
        match self.repeat {
            true => Ok(Page {
                next_page_token: Some("again".to_owned()),
                ..page
            }),
            false => Ok(page),
        }
    }

    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError> {
        self.inner.get_operation(operation)
    }
}

/// A rig over a [`Spy`], which answers the same next-page token for ever when `repeat`.
fn rig_spying(max_machines: usize, repeat: bool) -> (Rig, Arc<Spy>) {
    trace_everything();
    let account = Arc::new(Account::new());
    let clock = Arc::new(ManualClock::new());
    let spy = Arc::new(Spy {
        inner: Arc::clone(&account),
        filters: Mutex::new(Vec::new()),
        repeat,
    });
    let provider = GceProvider::new(spy.clone(), clock.clone(), settings(), live(max_machines));
    let rig = Rig {
        account,
        clock,
        provider,
    };
    (rig, spy)
}

/// The filter a list of this scheduler's instances sends: the project, the scheduler label
/// and nothing else, from the page `page_token`.
fn scheduler_filter(page_token: Option<&str>) -> ListFilter {
    ListFilter {
        project: Project::new(PROJECT).unwrap(),
        labels: vec![label("js-scheduler", SCHEDULER)],
        page_token: page_token.map(str::to_owned),
    }
}

/// Asserts that a list holds the instances this scheduler launched that are not terminated,
/// each with its acquire id, name, machine type, zone and status, in the order the cloud
/// gave them; that instances of another scheduler, instances with no marks, instances with
/// only some of ours and terminated instances are not in it; and that the provider counts
/// what it holds (a terminated one not) and the suspect, which it does not list. It catches
/// a list that takes in another scheduler's machine or drops the suspect from the count.
#[test]
fn a_list_holds_the_machines_this_scheduler_launched() {
    let rig = rig(live(5));
    let first = accepted(rig.provider.acquire(&ask("a1")));
    rig.account
        .add_instance(ours(&stranger(1), InstanceStatus::Running, "a2"));
    rig.account
        .add_instance(ours(&stranger(2), InstanceStatus::Terminated, "a3"));
    rig.account.add_instance(row(
        &stranger(3),
        InstanceStatus::Running,
        &name_of("sched-two", "a4"),
        marks("sched-two", "a4"),
    ));
    rig.account.add_instance(row(
        &stranger(4),
        InstanceStatus::Running,
        "plain-vm",
        vec![label("js-scheduler", SCHEDULER)],
    ));
    let listed = rig.provider.list().unwrap();
    assert_eq!(
        listed,
        [
            Held {
                id: first.id.clone(),
                acquire: "a1".to_owned(),
                name: Some(name_of(SCHEDULER, "a1")),
                offer: "q9-standard-2".to_owned(),
                zone: Some(ZONE.to_owned()),
                status: HostStatus::Booting,
            },
            Held {
                id: host(&stranger(1)),
                acquire: "a2".to_owned(),
                name: Some(name_of(SCHEDULER, "a2")),
                offer: "q9-standard-2".to_owned(),
                zone: Some(ZONE.to_owned()),
                status: HostStatus::Active,
            },
        ]
    );
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 2,
            can_add: Some(2)
        }
    );
}

/// Asserts that a list asks for the instances of the configured project that carry the
/// scheduler label and no other label, and that a list over several pages sends that same
/// filter with each page's token in turn, starts with none, returns every instance of every
/// page that is not terminated, and stops at the page that names no next one. It catches a
/// filter that selects more or less than this scheduler's instances, and a walk that stops
/// at the first page.
#[test]
fn a_list_asks_for_the_scheduler_label_and_walks_every_page() {
    let (rig, spy) = rig_spying(9, false);
    for (n, status) in [
        (1, InstanceStatus::Running),
        (2, InstanceStatus::Running),
        (3, InstanceStatus::Terminated),
        (4, InstanceStatus::Staging),
        (5, InstanceStatus::Running),
    ] {
        rig.account
            .add_instance(ours(&stranger(n), status, &format!("a{n}")));
    }
    rig.account.paginate(2);
    let listed = rig.provider.list().unwrap();
    let ids: Vec<String> = listed.iter().map(|held| held.id.to_string()).collect();
    assert_eq!(ids, [1, 2, 4, 5].map(|n| host(&stranger(n)).to_string()));
    assert_eq!(
        spy.filters(),
        [
            scheduler_filter(None),
            scheduler_filter(Some("page-2")),
            scheduler_filter(Some("page-4")),
        ]
    );
}

/// Asserts that a list whose walk fails is an error and not a shorter list, with the words
/// of the failure, whichever way it fails (a page that cannot be had, a scope the cloud
/// could not reach, a call that fails outright, a next-page token the cloud answers again
/// for ever), and that none of those changes what the provider counts. It catches a list
/// that answers what it has read when a page is missing, and a walk that never ends.
#[test]
fn a_page_failure_in_list_is_an_error() {
    let rig = rig(live(5));
    for acquire in ["a1", "a2", "a3"] {
        accepted(rig.provider.acquire(&ask(acquire)));
    }
    rig.account.paginate(1).fail_page(2);
    assert_eq!(
        rig.provider.list(),
        Err(ProviderError::new("the answer is not complete"))
    );
    assert_eq!(rig.provider.list().unwrap().len(), 3);

    rig.account.answer_unreachable_once();
    assert_eq!(
        rig.provider.list(),
        Err(ProviderError::new("the answer is not complete"))
    );
    assert_eq!(rig.provider.list().unwrap().len(), 3);

    rig.account.fail_next(
        Method::ListInstances,
        ApiError::Transport("reset".to_owned()),
    );
    assert_eq!(
        rig.provider.list(),
        Err(ProviderError::new("cannot talk to the cloud: reset"))
    );
    assert_eq!(rig.provider.list().unwrap().len(), 3);
    assert_eq!(rig.provider.capacity().supplied, 3);

    let (repeating, _) = rig_spying(5, true);
    repeating
        .account
        .add_instance(ours(&stranger(1), InstanceStatus::Running, "a1"));
    assert_eq!(
        repeating.provider.list(),
        Err(ProviderError::new("the answer is not complete"))
    );
    assert_eq!(repeating.provider.capacity().supplied, 0);
}

/// Asserts that a list which does not show a machine this provider launched yet (a read
/// right after a write can lag it) does not drop it from the count, that a list which shows
/// it is believed from then on, and that a list that no longer shows it drops it. It catches
/// a count that falls when a read lags, which would let the cap be passed.
#[test]
fn a_list_that_lags_a_launch_does_not_drop_it() {
    let rig = rig(live(2));
    let held = accepted(rig.provider.acquire(&ask("a1")));
    let shown = rig.account.instances().remove(0);
    rig.account.remove_instance(&shown.id);
    assert!(rig.provider.list().unwrap().is_empty());
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(1)
        }
    );
    rig.account.add_instance(shown.clone());
    assert_eq!(rig.provider.list().unwrap().len(), 1);
    rig.account.remove_instance(&shown.id);
    assert!(rig.provider.list().unwrap().is_empty());
    assert_eq!(rig.provider.capacity().supplied, 0);
    assert_eq!(held.id, shown.host_ref());
}

/// Asserts that a list believes the instances it shows over what the provider held: a
/// machine listed terminated is not supplied or counted, and one of ours listed again as a
/// suspect (its name and acquire label gone) is counted once, as a suspect. It catches a
/// list that keeps a stale record over what the project says.
#[test]
fn a_list_replaces_what_the_provider_held() {
    let rig = rig(live(3));
    let held = accepted(rig.provider.acquire(&ask("a1")));
    let machine = rig.account.instances().remove(0);
    rig.account
        .set_status(&machine.id, InstanceStatus::Terminated);
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(3)
        }
    );

    let second = accepted(rig.provider.acquire(&ask("a2")));
    assert_eq!(rig.provider.capacity().supplied, 1);
    let twin = rig.account.instances().remove(1);
    rig.account.remove_instance(&twin.id);
    rig.account.add_instance(Instance {
        labels: vec![label("js-scheduler", SCHEDULER)],
        ..twin
    });
    assert!(rig.provider.list().unwrap().is_empty());
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(2)
        }
    );
    assert_ne!(held.id, second.id);
}

/// Asserts that a cloud which answers instances whatever labels were asked for does not
/// make a stranger's instance ours: the list holds only ours, and the strangers are not
/// counted. It catches a list that trusts the cloud's filter instead of the marks.
#[test]
fn a_list_ignores_instances_the_cloud_returns_that_are_not_ours() {
    let rig = rig(live(2));
    accepted(rig.provider.acquire(&ask("a1")));
    rig.account.ignore_label_filters();
    rig.account.add_instance(row(
        &stranger(1),
        InstanceStatus::Running,
        "plain-vm",
        Vec::new(),
    ));
    rig.account.add_instance(row(
        &stranger(2),
        InstanceStatus::Running,
        &name_of("sched-two", "a9"),
        marks("sched-two", "a9"),
    ));
    let listed = rig.provider.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].acquire, "a1");
    assert_eq!(rig.provider.capacity().can_add, Some(1));
    assert!(matches!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
}
