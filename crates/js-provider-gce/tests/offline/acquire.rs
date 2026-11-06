//! What an acquire does: the checks that refuse it with nothing sent, the launch guard,
//! and the insert with its labels and request id.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use js_elastic::{
    AcquireRequest, Acquired, Capacity, CapacityProvider, Held, HostName, HostStatus, Offer,
    Purchase, Reason, Refusal, RefusalCode, SchedulerId,
};
use js_provider_gce::{
    AcceleratorTypeInfo, AcceleratorTypeName, ApiError, AttachedAccelerator, ErrorReason, GceApi,
    GceConfig, GceProvider, HostMaintenance, InsertRequest, Instance, InstanceRef, InstanceStatus,
    LabelError, ListFilter, MachineTypeInfo, MachineTypeName, ManualClock, Method, Operation,
    OperationRef, OperationStatus, Page, Project, ProvisioningModel, QuotaMetric, Region,
    RequestId, Scheduling, SkuPrice, TerminationAction, Zone,
};
use tracing::Level;

use crate::rig::{
    ARM_IMAGE, ARM_TYPE, Account, CPU_TYPE, GPU_TYPE, HOLD, NETWORK, OTHER_ZONE, PROJECT,
    SCHEDULER, SUBNETWORK, X86_IMAGE, ZONE, ask, candidate, host, label, live, live_x86_only,
    logged, marks, name_of, network, offer, offer_as, ours, rig, rig_over, row, settings, stranger,
    trace_everything, zone,
};

/// The machine of an answer that must be `Accepted`.
pub fn accepted(answer: Acquired) -> Held {
    match answer {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

fn refused(code: RefusalCode, reason: Reason) -> Acquired {
    Acquired::Refused(Refusal { code, reason })
}

fn not_allowed() -> Acquired {
    refused(RefusalCode::Shape, Reason::NotAllowed)
}

/// How many times the project was asked for `method`.
fn calls(account: &Account, method: Method) -> usize {
    account
        .methods()
        .iter()
        .filter(|called| **called == method)
        .count()
}

/// An acquire of `acquire` for the offer `offer`.
fn asking(acquire: &str, offer: Offer) -> AcquireRequest {
    AcquireRequest {
        offer,
        ..ask(acquire)
    }
}

/// An instance of the acquire `acquire` that is gone from the cloud's point of view but
/// still listed: terminated, in the other zone, so its name does not block a new insert.
fn old(id_text: &str, acquire: &str) -> Instance {
    Instance {
        zone: zone(OTHER_ZONE),
        ..ours(id_text, InstanceStatus::Terminated, acquire)
    }
}

/// A change to what a project answers, for the answers the in-memory project never gives.
type Rewrite<T> = Box<dyn Fn(T) -> T + Send + Sync>;

/// What a [`Wrapped`] project changes in the answers it passes on.
struct Edit {
    insert: Rewrite<Operation>,
    read: Rewrite<Operation>,
    list: Rewrite<Page<Instance>>,
}

/// Changes nothing.
fn untouched() -> Edit {
    Edit {
        insert: Box::new(|operation| operation),
        read: Box::new(|operation| operation),
        list: Box::new(|page| page),
    }
}

/// Finishes an operation with the error codes `codes`.
fn failing(codes: &[&str]) -> Rewrite<Operation> {
    let codes: Vec<ErrorReason> = codes.iter().map(|code| ErrorReason::new(code)).collect();
    Box::new(move |operation| Operation {
        status: OperationStatus::Done,
        error_codes: codes.clone(),
        ..operation
    })
}

/// Takes the instance out of an operation's answer.
fn without_target() -> Rewrite<Operation> {
    Box::new(|operation| Operation {
        target_id: None,
        ..operation
    })
}

/// Makes the first list answer an empty page, as a list that lags the project does.
fn hide_once() -> Rewrite<Page<Instance>> {
    let hidden = AtomicBool::new(false);
    Box::new(move |page| match hidden.swap(true, Ordering::SeqCst) {
        false => Page {
            items: Vec::new(),
            next_page_token: None,
            ..page
        },
        true => page,
    })
}

/// The in-memory project with some of its answers changed.
struct Wrapped {
    inner: Arc<Account>,
    edit: Edit,
}

impl GceApi for Wrapped {
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
        self.inner
            .insert_instance(request)
            .map(|operation| (self.edit.insert)(operation))
    }

    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        self.inner.get_instance(instance)
    }

    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        self.inner.delete_instance(instance)
    }

    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        self.inner
            .list_instances(filter)
            .map(|page| (self.edit.list)(page))
    }

    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError> {
        self.inner
            .get_operation(operation)
            .map(|read| (self.edit.read)(read))
    }
}

/// A provider over `account` whose answers are changed by `edit`.
fn wrapped(account: &Arc<Account>, edit: Edit, config: GceConfig) -> GceProvider {
    trace_everything();
    let api = Wrapped {
        inner: Arc::clone(account),
        edit,
    };
    GceProvider::new(
        Arc::new(api),
        Arc::new(ManualClock::new()),
        settings(),
        config,
    )
}

/// Asserts that two acquires with one id answer the same machine and the cloud inserts
/// once, with one request id derived from the scheduler and the acquire; and that when the
/// cloud's window for the request id has passed, a provider that remembers nothing finds
/// the instance again by its labels and inserts nothing. It catches a retry that inserts a
/// second instance.
#[test]
fn acquire_is_idempotent_on_the_acquire_id() {
    let first_rig = rig(live(2));
    let request = ask("a1");
    let first = accepted(first_rig.provider.acquire(&request));
    let second = accepted(first_rig.provider.acquire(&request));
    assert_eq!(first.id, second.id);
    assert_eq!(first.name, Some(request.name.to_string()));
    assert_eq!(second.name, first.name);
    assert_eq!(first_rig.account.inserted(), 1);
    let sent = first_rig.account.inserts();
    assert_eq!(sent.len(), 1);
    let scheduler = SchedulerId::new(SCHEDULER).unwrap();
    assert_eq!(
        sent[0].request_id,
        RequestId::derive(&request.acquire, &scheduler)
    );

    first_rig.account.forget_request_ids();
    let restarted = rig_over(Arc::clone(&first_rig.account), live(2));
    let third = accepted(restarted.provider.acquire(&request));
    assert_eq!(third.id, first.id);
    assert_eq!(third.acquire, "a1");
    assert_eq!(first_rig.account.inserted(), 1);
    assert_eq!(calls(&first_rig.account, Method::InsertInstance), 1);
}

/// Asserts that an insert the cloud refuses because the name is taken is answered with the
/// instance of that name, read by name, when it is ours and the list did not show it, with
/// nothing inserted, the instance counted, and the acquire not left unresolved. It catches
/// a name conflict taken for a failure.
#[test]
fn a_name_already_taken_by_our_instance_is_answered_with_it() {
    let account = Arc::new(Account::new());
    account.add_instance(ours(&stranger(1), InstanceStatus::Running, "a1"));
    let edit = Edit {
        list: hide_once(),
        ..untouched()
    };
    let provider = wrapped(&account, edit, live(2));
    let held = accepted(provider.acquire(&ask("a1")));
    assert_eq!(held.id, host(&stranger(1)));
    assert_eq!(held.acquire, "a1");
    assert_eq!(held.status, HostStatus::Active);
    assert_eq!(account.inserted(), 0);
    assert_eq!(
        account.methods(),
        [
            Method::ListInstances,
            Method::InsertInstance,
            Method::GetInstance
        ]
    );
    assert_eq!(
        provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(1)
        }
    );
}

/// Asserts that an insert the cloud refuses because the name is taken by an instance that
/// carries only some of our marks (here the scheduler's label and the name, not the
/// acquire's label) is refused as half-marked, with nothing inserted and the instance
/// counted toward the cap. It catches a suspect taken for the acquire's machine.
#[test]
fn a_name_taken_by_a_suspect_is_refused_as_half_marked() {
    let account = Arc::new(Account::new());
    account.add_instance(row(
        &stranger(2),
        InstanceStatus::Running,
        &name_of(SCHEDULER, "a1"),
        vec![label("js-scheduler", SCHEDULER)],
    ));
    let lab = rig_over(account, live(2));
    assert_eq!(
        lab.provider.acquire(&ask("a1")),
        refused(RefusalCode::Denied, Reason::HalfMarked { count: 1 })
    );
    assert_eq!(lab.account.inserted(), 0);
    assert_eq!(
        lab.account.methods(),
        [
            Method::ListInstances,
            Method::InsertInstance,
            Method::GetInstance
        ]
    );
    assert_eq!(lab.provider.capacity().can_add, Some(1));
}

/// Asserts that when the name is taken and the instance of that name cannot be read, whether
/// the cloud says it does not exist or does not answer, the acquire is `Unknown` and holds
/// its room, and that the next try finds the instance in the list and answers it with
/// nothing inserted. It catches a name conflict whose instance cannot be read taken for a
/// refusal, which would free the room.
#[test]
fn a_name_taken_whose_instance_cannot_be_read_is_unknown() {
    for error in [
        ApiError::answered("notFound"),
        ApiError::Transport("reset".to_owned()),
    ] {
        let account = Arc::new(Account::new());
        account.add_instance(ours(&stranger(1), InstanceStatus::Running, "a1"));
        let edit = Edit {
            list: hide_once(),
            ..untouched()
        };
        let provider = wrapped(&account, edit, live(2));
        account.fail_next(Method::GetInstance, error);
        let (answer, warnings) = logged(Level::WARN, || provider.acquire(&ask("a1")));
        assert_eq!(answer, Acquired::Unknown);
        assert!(
            warnings.contains("the instance cannot be read"),
            "{warnings}"
        );
        assert_eq!(provider.capacity().can_add, Some(1));
        let held = accepted(provider.acquire(&ask("a1")));
        assert_eq!(held.id, host(&stranger(1)));
        assert_eq!(account.inserted(), 0);
    }
}

/// Asserts that the image of an insert is the one configured for the architecture of the
/// offer, `arm64` or `x86_64`, and that an offer of an architecture with no image is
/// refused as not allowed, with nothing sent and a warning that names the architecture and
/// not the payload.
#[test]
fn the_image_is_selected_by_offer_architecture() {
    let lab = rig(live(3));
    let arm = asking("a1", offer(&format!("{ARM_TYPE}/on-demand"), "arm64"));
    accepted(lab.provider.acquire(&arm));
    accepted(lab.provider.acquire(&ask("a2")));
    let images: Vec<String> = lab
        .account
        .inserts()
        .iter()
        .map(|insert| insert.image.to_string())
        .collect();
    assert_eq!(images, [ARM_IMAGE, X86_IMAGE]);

    let narrow = rig(live_x86_only(2));
    let (answer, warnings) = logged(Level::WARN, || narrow.provider.acquire(&arm));
    assert_eq!(answer, not_allowed());
    assert!(warnings.contains("no image is configured"), "{warnings}");
    assert!(warnings.contains("arm64"), "{warnings}");
    assert!(!warnings.contains("user-data"), "{warnings}");
    assert!(narrow.account.methods().is_empty());
    let other = asking("a3", offer(&format!("{CPU_TYPE}/on-demand"), "riscv64"));
    assert_eq!(narrow.provider.acquire(&other), not_allowed());
    accepted(narrow.provider.acquire(&ask("a4")));
}

/// Asserts that an offer the provider does not launch is refused as a shape error with
/// nothing sent: one with no zone, a zone that is not a zone name, or a zone the
/// configuration does not list; one that is not on demand or Spot (a capacity block, or no
/// purchase stated); and one whose id names no machine type; and that a Spot offer in a
/// configured zone is launched, as Spot, in that zone.
#[test]
fn an_offer_the_provider_does_not_launch_is_refused_as_shape() {
    let lab = rig(live(5));
    let offers = [
        Offer {
            zone: None,
            ..candidate()
        },
        Offer {
            zone: Some("nowhere".to_owned()),
            ..candidate()
        },
        Offer {
            zone: Some("region-two-a".to_owned()),
            ..candidate()
        },
        Offer {
            purchase: Some(Purchase::CapacityBlock),
            ..candidate()
        },
        Offer {
            purchase: None,
            ..candidate()
        },
        Offer {
            id: String::new(),
            ..candidate()
        },
        Offer {
            id: "/on-demand".to_owned(),
            ..candidate()
        },
    ];
    for (n, bad) in offers.into_iter().enumerate() {
        assert_eq!(
            lab.provider.acquire(&asking(&format!("a{n}"), bad)),
            not_allowed(),
            "offer {n}"
        );
    }
    assert!(lab.account.methods().is_empty());

    let spot = Offer {
        zone: Some(OTHER_ZONE.to_owned()),
        ..offer_as(&format!("{CPU_TYPE}/spot"), "x86_64", Purchase::Spot)
    };
    accepted(lab.provider.acquire(&asking("spot", spot)));
    let sent = lab.account.inserts();
    assert_eq!(
        sent[0].scheduling.provisioning_model,
        ProvisioningModel::Spot
    );
    assert_eq!(sent[0].zone.as_str(), OTHER_ZONE);
    assert_eq!(sent[0].machine_type, MachineTypeName(CPU_TYPE.to_owned()));
}

/// Asserts that an offer id that names attached accelerators (`<type>+<accelerator>x<n>`)
/// is inserted with the machine type and the accelerators attached, and an id with none, or
/// one of a type that carries its accelerators, with none attached; and that a suffix that
/// is not an accelerator and a count (no `x`, a count of zero, a count that is not a number
/// or none) is refused as a shape error with nothing sent.
#[test]
fn attached_accelerators_come_from_the_offer_id() {
    let lab = rig(live(5));
    let t4 = offer_as(
        "n1-standard-8+nvidia-tesla-t4x2/spot",
        "x86_64",
        Purchase::Spot,
    );
    accepted(lab.provider.acquire(&asking("a1", t4)));
    accepted(lab.provider.acquire(&ask("a2")));
    let built_in = offer(&format!("{GPU_TYPE}/on-demand"), "x86_64");
    accepted(lab.provider.acquire(&asking("a3", built_in)));
    let sent = lab.account.inserts();
    assert_eq!(sent[0].machine_type.as_str(), "n1-standard-8");
    assert_eq!(
        sent[0].accelerators,
        Some(AttachedAccelerator {
            type_name: AcceleratorTypeName("nvidia-tesla-t4".to_owned()),
            count: 2
        })
    );
    assert_eq!(sent[1].accelerators, None);
    assert_eq!(sent[2].machine_type.as_str(), GPU_TYPE);
    assert_eq!(sent[2].accelerators, None);

    let before = lab.account.methods().len();
    for suffix in ["t4", "t4x0", "t4xtwo", "t4x", "t4x-1"] {
        let id = format!("n1-standard-8+nvidia-tesla-{suffix}/spot");
        let bad = offer_as(&id, "x86_64", Purchase::Spot);
        assert_eq!(
            lab.provider.acquire(&asking("a9", bad)),
            not_allowed(),
            "{id}"
        );
    }
    let no_type = offer_as("+nvidia-tesla-t4x1/spot", "x86_64", Purchase::Spot);
    assert_eq!(lab.provider.acquire(&asking("a9", no_type)), not_allowed());
    assert_eq!(lab.account.methods().len(), before);
}

/// Asserts that with an allow list of machine types an acquire of a type the list leaves out
/// is refused as a shape error with nothing sent, and one of a type it names is inserted.
#[test]
fn a_machine_type_outside_the_allow_list_is_refused() {
    let only = BTreeSet::from([MachineTypeName(ARM_TYPE.to_owned())]);
    let lab = rig(live(2).allowed(only));
    assert_eq!(lab.provider.acquire(&ask("a1")), not_allowed());
    assert!(lab.account.methods().is_empty());
    let arm = asking("a2", offer(&format!("{ARM_TYPE}/on-demand"), "arm64"));
    accepted(lab.provider.acquire(&arm));
}

/// Asserts that a host name that does not carry this scheduler's prefix is refused as a
/// shape error with nothing sent, because the instance could not be recognised as ours
/// afterwards.
#[test]
fn a_host_name_of_another_scheduler_is_refused() {
    let lab = rig(live(1));
    let request = ask("a1");
    let other = SchedulerId::new("sched-two").unwrap();
    let request = AcquireRequest {
        name: HostName::derive(&other, &request.acquire),
        ..request
    };
    assert_eq!(
        lab.provider.acquire(&request),
        refused(RefusalCode::Shape, Reason::Name)
    );
    assert!(lab.account.methods().is_empty());
}

/// Asserts that every instance is created with host maintenance `TERMINATE` and no
/// automatic restart; that an on-demand instance without a lifetime has no termination
/// action and no run duration; that a Spot instance is deleted when the cloud ends it; and
/// that a lifetime becomes a run duration in whole seconds rounded up (3600.4 s is 3601 s,
/// 3600 s stays) with the same deletion for an on-demand instance. It catches an instance
/// that restarts with a spent join token, and one that lingers terminated.
#[test]
fn every_instance_is_created_without_restart_and_a_spot_one_deletes_on_termination() {
    let lab = rig(live(6));
    let spot = offer_as(&format!("{CPU_TYPE}/spot"), "x86_64", Purchase::Spot);
    accepted(lab.provider.acquire(&ask("a1")));
    accepted(lab.provider.acquire(&asking("a2", spot.clone())));
    let rounded = AcquireRequest {
        lifetime: Some(Duration::from_millis(3_600_400)),
        ..ask("a3")
    };
    accepted(lab.provider.acquire(&rounded));
    let exact = AcquireRequest {
        lifetime: Some(Duration::from_secs(3600)),
        ..ask("a4")
    };
    accepted(lab.provider.acquire(&exact));
    let spot_for_a_while = AcquireRequest {
        lifetime: Some(Duration::from_secs(60)),
        ..asking("a5", spot)
    };
    accepted(lab.provider.acquire(&spot_for_a_while));
    let schedules: Vec<Scheduling> = lab
        .account
        .inserts()
        .into_iter()
        .map(|insert| insert.scheduling)
        .collect();
    let expected = |model, action, run: Option<u64>| Scheduling {
        provisioning_model: model,
        on_host_maintenance: HostMaintenance::Terminate,
        automatic_restart: false,
        termination_action: action,
        max_run_duration: run.map(Duration::from_secs),
    };
    let delete = Some(TerminationAction::Delete);
    assert_eq!(
        schedules,
        [
            expected(ProvisioningModel::Standard, None, None),
            expected(ProvisioningModel::Spot, delete, None),
            expected(ProvisioningModel::Standard, delete, Some(3601)),
            expected(ProvisioningModel::Standard, delete, Some(3600)),
            expected(ProvisioningModel::Spot, delete, Some(60)),
        ]
    );
}

/// Asserts that the boot disk size is the offer's disk in whole GiB rounded up, and the
/// image's own when the offer asks for none: 0 bytes is none, 1 byte is 1 GiB, exactly
/// 10 GiB is 10 and one byte more is 11. It catches a size that rounds down.
#[test]
fn the_disk_size_is_the_offers_disk_in_whole_gib() {
    let lab = rig(live(6));
    let gib: u64 = 1 << 30;
    let sizes = [
        (0, None),
        (1, Some(1)),
        (gib, Some(1)),
        (10 * gib, Some(10)),
        (10 * gib + 1, Some(11)),
    ];
    for (n, (bytes, _)) in sizes.iter().enumerate() {
        let sized = Offer {
            disk_bytes: *bytes,
            ..candidate()
        };
        accepted(lab.provider.acquire(&asking(&format!("d{n}"), sized)));
    }
    let got: Vec<Option<u64>> = lab
        .account
        .inserts()
        .iter()
        .map(|insert| insert.disk_size_gb)
        .collect();
    let want: Vec<Option<u64>> = sizes.iter().map(|(_, size)| *size).collect();
    assert_eq!(got, want);
}

/// Asserts that the launched instance carries the scheduler, the acquire and then the
/// acquire's own labels (`js-label-` and the key) as labels, in the order given, is named
/// by the host name, of the offer's type in the offer's zone from the configured image, in
/// the project and on the network of the configuration, and that the answer names it as
/// booting, with the offer, the zone and the name.
#[test]
fn the_launched_instance_carries_the_scheduler_labels() {
    let lab = rig(live(2));
    let request = AcquireRequest {
        labels: vec![
            ("team".to_owned(), "infra".to_owned()),
            ("tier".to_owned(), "gpu".to_owned()),
        ],
        ..ask("a1")
    };
    let held = accepted(lab.provider.acquire(&request));
    let mut expected = marks(SCHEDULER, "a1");
    expected.push(label("js-label-team", "infra"));
    expected.push(label("js-label-tier", "gpu"));
    let launched = &lab.account.instances()[0];
    assert_eq!(launched.labels, expected);
    assert_eq!(launched.name, name_of(SCHEDULER, "a1"));
    assert_eq!(held.id, launched.host_ref());
    assert_eq!(held.acquire, "a1");
    assert_eq!(held.name, Some(request.name.to_string()));
    assert_eq!(held.offer, format!("{CPU_TYPE}/on-demand"));
    assert_eq!(held.zone.as_deref(), Some(ZONE));
    assert_eq!(held.status, HostStatus::Booting);
    let insert = &lab.account.inserts()[0];
    assert_eq!(insert.labels, expected);
    assert_eq!(insert.project.as_str(), PROJECT);
    assert_eq!(insert.zone.as_str(), ZONE);
    assert_eq!(insert.machine_type.as_str(), CPU_TYPE);
    assert_eq!(insert.image.as_str(), X86_IMAGE);
    assert_eq!(insert.network, network());
    assert_eq!(insert.network.network, NETWORK);
    assert_eq!(insert.network.subnetwork, SUBNETWORK);
}

/// Asserts that a label which cannot be a label of the cloud (a key with an upper-case
/// letter, a value with one, a key given twice, more labels than the cloud takes) is
/// refused as a shape error that carries the words for why, with nothing sent. It catches a
/// label rewritten into something else, which could make two labels one.
#[test]
fn labels_that_cannot_be_labels_refuse_the_acquire() {
    let lab = rig(live(2));
    let many: Vec<(String, String)> = (0..63).map(|n| (format!("k{n}"), "v".to_owned())).collect();
    let cases = [
        (
            vec![("Team".to_owned(), "infra".to_owned())],
            LabelError::Key("Team".to_owned()),
        ),
        (
            vec![("team".to_owned(), "Infra".to_owned())],
            LabelError::Value("team".to_owned()),
        ),
        (
            vec![
                ("team".to_owned(), "a".to_owned()),
                ("team".to_owned(), "b".to_owned()),
            ],
            LabelError::Twice("team".to_owned()),
        ),
        (many, LabelError::TooMany(65)),
    ];
    for (labels, why) in cases {
        let request = AcquireRequest {
            labels,
            ..ask("a1")
        };
        assert_eq!(
            lab.provider.acquire(&request),
            refused(RefusalCode::Shape, Reason::Label(why.to_string()))
        );
    }
    assert!(lab.account.methods().is_empty());
}

/// Asserts that an insert that is throttled answers `Unknown`, sends no second insert
/// while the hold lasts (the retry is deferred by the time that is left of it), and counts
/// toward the cap; that a throttle after the instance landed is settled by the next try
/// finding it, so the cloud inserted once; and that a list that is throttled before the
/// insert is `Unknown` with no insert sent, and the retry with the same acquire id inserts
/// once. It catches a throttle read as a refusal, which would let a retry insert twice.
#[test]
fn throttle_is_unknown_and_never_a_second_launch() {
    let lab = rig(live(4));
    let throttle = ApiError::answered("rateLimitExceeded");
    lab.account
        .fail_next(Method::InsertInstance, throttle.clone());
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lab.account.inserted(), 0);
    assert_eq!(lab.provider.capacity().can_add, Some(3));
    assert_eq!(
        lab.provider.acquire(&ask("a1")),
        Acquired::Deferred { wait: HOLD }
    );
    lab.clock.advance(Duration::from_secs(100));
    assert_eq!(
        lab.provider.acquire(&ask("a1")),
        Acquired::Deferred {
            wait: HOLD - Duration::from_secs(100)
        }
    );
    assert_eq!(calls(&lab.account, Method::InsertInstance), 1);

    lab.account
        .lose_after_next(Method::InsertInstance, throttle.clone());
    assert_eq!(lab.provider.acquire(&ask("a2")), Acquired::Unknown);
    assert_eq!(lab.account.inserted(), 1);
    let landed = accepted(lab.provider.acquire(&ask("a2")));
    assert_eq!(landed.id, lab.account.instances()[0].host_ref());
    assert_eq!(lab.account.inserted(), 1);

    lab.account.fail_next(Method::ListInstances, throttle);
    let inserts = calls(&lab.account, Method::InsertInstance);
    assert_eq!(lab.provider.acquire(&ask("a3")), Acquired::Unknown);
    assert_eq!(calls(&lab.account, Method::InsertInstance), inserts);
    accepted(lab.provider.acquire(&ask("a3")));
    assert_eq!(lab.account.inserted(), 2);
}

/// Asserts that an insert whose operation cannot be read is `Unknown` and holds its room
/// though the instance did land, and that the retry answers that instance through the label
/// lookup and inserts nothing more. It catches an unreadable operation taken for a failure.
#[test]
fn an_operation_that_cannot_be_read_is_unknown() {
    let lab = rig(live(2));
    lab.account
        .fail_next(Method::GetOperation, ApiError::Timeout);
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lab.account.inserted(), 1);
    assert_eq!(lab.provider.capacity().can_add, Some(1));
    let held = accepted(lab.provider.acquire(&ask("a1")));
    assert_eq!(held.id, lab.account.instances()[0].host_ref());
    assert_eq!(lab.account.inserted(), 1);
    assert_eq!(calls(&lab.account, Method::InsertInstance), 1);
}

/// Asserts that an operation that finishes with an error is mapped by its code: no capacity
/// in the zone is refused as capacity and the quota as quota, each with the code as the
/// reason, no instance inserted and the room freed; and a code the table does not list is
/// `Unknown`, holds its room, and defers the retry. It catches an operation error taken for
/// success, or an unlisted one taken for a refusal.
#[test]
fn an_operation_error_is_mapped_by_its_code() {
    for (code, constant) in [
        ("ZONE_RESOURCE_POOL_EXHAUSTED", RefusalCode::Capacity),
        ("QUOTA_EXCEEDED", RefusalCode::Quota),
    ] {
        let lab = rig(live(1));
        lab.account.fail_next_operation(code);
        assert_eq!(
            lab.provider.acquire(&ask("a1")),
            refused(constant, Reason::Api(code.to_owned()))
        );
        assert_eq!(lab.account.inserted(), 0);
        assert_eq!(lab.provider.capacity().can_add, Some(1));
    }
    let lab = rig(live(1));
    lab.account.fail_next_operation("SOMETHING_NEW");
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lab.account.inserted(), 0);
    assert_eq!(lab.provider.capacity().can_add, Some(0));
    assert_eq!(
        lab.provider.acquire(&ask("a1")),
        Acquired::Deferred { wait: HOLD }
    );
}

/// Asserts that an insert whose answer is already done with error codes is mapped without
/// reading the operation: the first code the table refuses decides (here a quota after a
/// code it does not list), and codes it does not list at all are `Unknown`. It catches an
/// answer that is read by its first code only, or that is read a second time.
#[test]
fn an_insert_answered_done_with_errors_is_mapped_by_its_first_refusal() {
    let account = Arc::new(Account::new());
    let edit = Edit {
        insert: failing(&["SOMETHING_NEW", "QUOTA_EXCEEDED"]),
        ..untouched()
    };
    let provider = wrapped(&account, edit, live(1));
    assert_eq!(
        provider.acquire(&ask("a1")),
        refused(RefusalCode::Quota, Reason::Api("QUOTA_EXCEEDED".to_owned()))
    );
    assert_eq!(provider.capacity().can_add, Some(1));
    assert_eq!(calls(&account, Method::GetOperation), 0);

    let account = Arc::new(Account::new());
    let edit = Edit {
        insert: failing(&["SOMETHING_NEW"]),
        ..untouched()
    };
    let provider = wrapped(&account, edit, live(1));
    assert_eq!(provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(provider.capacity().can_add, Some(0));
    assert_eq!(calls(&account, Method::GetOperation), 0);
}

/// Asserts that an operation that names no instance, in the insert's answer and in the
/// read, is `Unknown` though the instance landed and that the retry finds it; and that when
/// only the read leaves the instance out, the one the insert's answer names is the answer.
/// It catches an operation with no target taken for a launch with no machine.
#[test]
fn an_operation_that_names_no_instance_is_unknown() {
    let account = Arc::new(Account::new());
    let edit = Edit {
        insert: without_target(),
        read: without_target(),
        ..untouched()
    };
    let provider = wrapped(&account, edit, live(1));
    assert_eq!(provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(account.inserted(), 1);
    assert_eq!(provider.capacity().can_add, Some(0));
    let held = accepted(provider.acquire(&ask("a1")));
    assert_eq!(held.id, account.instances()[0].host_ref());
    assert_eq!(account.inserted(), 1);

    let account = Arc::new(Account::new());
    let edit = Edit {
        read: without_target(),
        ..untouched()
    };
    let provider = wrapped(&account, edit, live(1));
    let held = accepted(provider.acquire(&ask("a1")));
    assert_eq!(held.id, account.instances()[0].host_ref());
}

/// Asserts that when the cap is reached the acquire is denied with the count and the cap,
/// and nothing is inserted; that an insert with no answer counts toward the cap while it is
/// unresolved; and that the cap is the number of machines, not of acquires.
#[test]
fn the_cap_counts_launched_and_unanswered_machines() {
    let lab = rig(live(1));
    accepted(lab.provider.acquire(&ask("a1")));
    let at_max = refused(
        RefusalCode::Denied,
        Reason::AtMaxMachines {
            held: 1,
            max_machines: 1,
        },
    );
    assert_eq!(lab.provider.acquire(&ask("a2")), at_max);
    assert_eq!(lab.account.inserted(), 1);

    let lost = rig(live(1));
    lost.account
        .fail_next(Method::InsertInstance, ApiError::Timeout);
    assert_eq!(lost.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lost.provider.acquire(&ask("a2")), at_max);
    assert_eq!(calls(&lost.account, Method::InsertInstance), 1);
}

/// Asserts that an insert whose answer was lost after it landed counts toward the cap once:
/// while it is unresolved and no list shows its machine, the insert is counted; once a list
/// shows that machine, the machine is counted and the unresolved insert of the same acquire
/// is not counted again. It catches a count that adds the unresolved insert beside the
/// machine it made, which would refuse an acquire the cap still has room for.
#[test]
fn an_unresolved_launch_and_the_machine_it_made_count_once() {
    let lab = rig(live(2));
    lab.account
        .lose_after_next(Method::InsertInstance, ApiError::Timeout);
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lab.account.inserted(), 1);
    assert_eq!(lab.provider.capacity().can_add, Some(1));
    let listed = lab.provider.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].acquire, "a1");
    let capacity = lab.provider.capacity();
    assert_eq!((capacity.supplied, capacity.can_add), (1, Some(1)));
    accepted(lab.provider.acquire(&ask("a2")));
    assert_eq!(lab.account.inserted(), 2);
}

/// Asserts that an instance of the acquire that carries some of this scheduler's marks and
/// not all (here the right labels and another name) denies the acquire as half-marked with
/// nothing inserted; and that the provider counts such an instance toward the cap for every
/// other acquire.
#[test]
fn a_half_marked_instance_blocks_its_acquire_and_counts_toward_the_cap() {
    let lab = rig(live(1));
    lab.account.add_instance(row(
        &stranger(8),
        InstanceStatus::Running,
        "renamed",
        marks(SCHEDULER, "a1"),
    ));
    assert_eq!(
        lab.provider.acquire(&ask("a1")),
        refused(RefusalCode::Denied, Reason::HalfMarked { count: 1 })
    );
    assert_eq!(lab.account.inserted(), 0);
    assert_eq!(lab.provider.capacity().can_add, Some(0));
    assert_eq!(
        lab.provider.acquire(&ask("a2")),
        refused(
            RefusalCode::Denied,
            Reason::AtMaxMachines {
                held: 1,
                max_machines: 1
            }
        )
    );
}

/// Asserts that when the instances of the acquire cannot be read, the acquire answers
/// `Unknown` and no insert is sent, and that the acquire is not left unresolved: nothing
/// was sent that could have landed.
#[test]
fn a_failed_read_before_the_insert_is_unknown_and_sends_no_insert() {
    let lab = rig(live(1));
    let reset = ApiError::Transport("reset".to_owned());
    lab.account.fail_next(Method::ListInstances, reset);
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(lab.account.methods(), [Method::ListInstances]);
    assert_eq!(lab.provider.capacity().can_add, Some(1));
    accepted(lab.provider.acquire(&ask("a1")));
}

/// Asserts that a terminated instance of the acquire is not its machine: the acquire
/// inserts a new one.
#[test]
fn a_terminated_instance_of_the_acquire_is_not_its_machine() {
    let lab = rig(live(2));
    let earlier = stranger(7);
    lab.account.add_instance(old(&earlier, "a1"));
    let held = accepted(lab.provider.acquire(&ask("a1")));
    assert_ne!(held.id, host(&earlier));
    assert_eq!(lab.account.inserted(), 1);
}

/// Asserts that the lookup of the acquire's instances walks every page: with the list in
/// pages of one, two terminated instances and a running one of the acquire, the running one
/// is the answer and nothing is inserted; that a page that fails, a page that names a scope
/// it could not reach and a page that answers the token it was given are each `Unknown`
/// with no insert sent, and a retry then goes on. It catches a lookup that reads the first
/// page only, and a lookup that gives up on a partial list as if it were whole.
#[test]
fn the_lookup_walks_every_page_and_a_page_that_fails_is_unknown() {
    let lab = rig(live(3));
    lab.account.paginate(1);
    lab.account.add_instance(old(&stranger(1), "a1"));
    lab.account.add_instance(old(&stranger(2), "a1"));
    lab.account
        .add_instance(ours(&stranger(3), InstanceStatus::Running, "a1"));
    let held = accepted(lab.provider.acquire(&ask("a1")));
    assert_eq!(held.id, host(&stranger(3)));
    assert_eq!(calls(&lab.account, Method::ListInstances), 3);
    assert_eq!(lab.account.inserted(), 0);

    let failing_page = rig(live(3));
    failing_page.account.paginate(1);
    failing_page.account.add_instance(old(&stranger(1), "a1"));
    failing_page.account.add_instance(old(&stranger(2), "a1"));
    failing_page.account.fail_page(2);
    assert_eq!(failing_page.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(
        failing_page.account.methods(),
        [Method::ListInstances, Method::ListInstances]
    );
    accepted(failing_page.provider.acquire(&ask("a1")));
    assert_eq!(failing_page.account.inserted(), 1);

    let unreachable = rig(live(3));
    unreachable.account.answer_unreachable_once();
    assert_eq!(unreachable.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(unreachable.account.methods(), [Method::ListInstances]);
    accepted(unreachable.provider.acquire(&ask("a1")));

    let account = Arc::new(Account::new());
    let edit = Edit {
        list: Box::new(|page| Page {
            next_page_token: Some("stuck".to_owned()),
            ..page
        }),
        ..untouched()
    };
    let provider = wrapped(&account, edit, live(3));
    assert_eq!(provider.acquire(&ask("a1")), Acquired::Unknown);
    assert_eq!(
        account.methods(),
        [Method::ListInstances, Method::ListInstances]
    );
}
