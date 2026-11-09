//! The job tables in the scaler's words: the request id of a job, what a share job and a
//! whole-host job ask of a host, the hosts in use, and when a reservation holds.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::{AcquireId, Offer, PriceUnit};
use js_jobs::{Booked, DeclaredPort, JobSpec, NetworkPolicy, NewJob};
use js_scaler::{HostState, Millis, Record, StoredOffer};
use js_types::JobNetwork as Reach;
use js_types::{ExecMode, IsaLevel, Model, Resources, X86Level, accelerator_of};

use super::*;
use crate::agent_api::job_rig::{placed, spec, tables};
use crate::job_store::ServerJobs;

const GIB: u64 = 1 << 30;

/// The job `id`, queued with `spec`, as the tables return it.
fn queued(jobs: &ServerJobs, id: &str, spec: JobSpec) -> JobRecord {
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap().record
}

/// The floor of a pool of a10 machines: eight cores, 32 GiB, one a10, run in containers.
fn floor() -> Demand {
    let resources = Resources::new(8000, 32 * GIB, 1, Some("nvidia-a10".to_owned())).unwrap();
    Demand {
        accelerator: AcceleratorNeed::booked(&resources),
        resources,
        disk_bytes: None,
        os: Some("linux".to_owned()),
        arch: None,
        isa_level: None,
        exec: Some(ExecMode::Container),
        network: Reach::None,
        host_ports: Vec::new(),
    }
}

/// Asserts that a job id that is a request id is its own request id, and that one with a
/// capital letter (a job id, not a request id) is named by its digest: `job.` and 40 hex
/// digits, the same for the same id and different for another. It catches a job whose id has
/// a capital dropped from the demand, and a name that changes between steps, which would lose
/// the job's reservation.
#[test]
fn a_job_is_its_own_request_unless_its_id_is_not_one() {
    let plain = JobId::parse("job-1.a_b").unwrap();
    assert_eq!(request_id(&plain).unwrap().as_str(), "job-1.a_b");
    let capital = JobId::parse("Job-1").unwrap();
    let named = request_id(&capital).unwrap();
    assert_eq!(named.as_str().len(), "job.".len() + 40);
    assert!(named.as_str().starts_with("job."));
    assert!(named.as_str()[4..].chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(request_id(&capital), Some(named.clone()));
    let other = request_id(&JobId::parse("Job-2").unwrap()).unwrap();
    assert_ne!(other, named);
}

/// Asserts that a share job asks for exactly what admission asks at submit: its resources,
/// its disk, its accelerator from the booked GPUs narrowed by its `accelerator.*` keys, its
/// platform, ISA level, execution mode, network level and host ports (sorted, each once),
/// and nothing of the pool's floor; and that a job that names no disk asks for none. It
/// catches a demand that drops a constraint, which launches a machine the job cannot run on.
#[test]
fn a_share_job_asks_for_what_admission_asked() {
    let (_store, jobs) = tables();
    let resources = Resources::new(2000, 4 * GIB, 1, Some("nvidia-a10".to_owned()))
        .unwrap()
        .with_disk_bytes(20 * GIB);
    let mut asked = JobSpec {
        needs: Needs::Shape(resources.clone()),
        network: NetworkPolicy::Open,
        ports: vec![
            DeclaredPort {
                host: 9000,
                container: 80,
            },
            DeclaredPort {
                host: 8000,
                container: 81,
            },
            DeclaredPort {
                host: 9000,
                container: 82,
            },
        ],
        ..spec()
    };
    asked.constraints.os = Some("linux".to_owned());
    asked.constraints.arch = Some("arm64".to_owned());
    asked.constraints.exec = Some(ExecMode::SandboxExec);
    let caps = &mut asked.constraints.capabilities;
    caps.insert("isa_level".to_owned(), "x86-64-v3".to_owned());
    caps.insert("accelerator.memory-gib".to_owned(), "24".to_owned());
    let job = queued(&jobs, "share", asked.clone());
    let wanted = demand(&job, &floor());
    let accelerator =
        AcceleratorNeed::requested(&asked.constraints.capabilities, &resources).unwrap();
    assert!(
        accelerator
            .as_ref()
            .is_some_and(|need| need.memory_per_device.is_some())
    );
    let expected = Demand {
        resources,
        disk_bytes: Some(20 * GIB),
        os: Some("linux".to_owned()),
        arch: Some("arm64".to_owned()),
        isa_level: Some(IsaLevel::X86_64(X86Level::V3)),
        exec: Some(ExecMode::SandboxExec),
        accelerator,
        network: Reach::Open,
        host_ports: vec![8000, 9000],
    };
    assert_eq!(wanted, expected);

    let bare = queued(&jobs, "bare", spec());
    let wanted = demand(&bare, &floor());
    assert_eq!(wanted.disk_bytes, None);
    assert_eq!(wanted.accelerator, None);
    assert_eq!(wanted.isa_level, None);
    assert_eq!(
        wanted.resources,
        Resources::new(1500, GIB, 0, None).unwrap()
    );
    // The floor's platform and mode stand when the job names none.
    assert_eq!(wanted.os.as_deref(), Some("linux"));
    assert_eq!(wanted.exec, Some(ExecMode::Container));
}

/// Asserts that a stored share job whose `accelerator.*` keys do not read (an older build
/// stored them) asks for the GPUs it books, and that an `isa_level` that does not read asks
/// for none. It catches a job whose demand is dropped, or made empty, for a key the tables
/// kept.
#[test]
fn a_share_job_whose_keys_do_not_read_asks_for_what_it_books() {
    let (_store, jobs) = tables();
    let resources = Resources::new(1000, GIB, 2, Some("nvidia-a10".to_owned())).unwrap();
    let mut asked = JobSpec {
        needs: Needs::Shape(resources.clone()),
        ..spec()
    };
    let caps = &mut asked.constraints.capabilities;
    caps.insert("accelerator.count".to_owned(), "lots".to_owned());
    caps.insert("isa_level".to_owned(), "x86-64-v9".to_owned());
    let job = queued(&jobs, "odd", asked);
    let wanted = demand(&job, &floor());
    assert_eq!(wanted.accelerator, AcceleratorNeed::booked(&resources));
    assert_eq!(wanted.isa_level, None);
}

/// Asserts that a whole-host job asks for the pool's floor, its resources and accelerator,
/// with its own platform, execution mode, network level and ports over the floor's; and that
/// one that names nothing asks for the floor as it is. It catches a whole-host job read as a
/// job of no size, which a busy host or the smallest offer would be taken to serve.
#[test]
fn a_whole_host_job_asks_for_the_pools_floor() {
    let (_store, jobs) = tables();
    let mut asked = JobSpec {
        needs: Needs::WholeHost,
        network: NetworkPolicy::AllowList,
        ports: vec![DeclaredPort {
            host: 7000,
            container: 7000,
        }],
        ..spec()
    };
    asked.constraints.arch = Some("x86_64".to_owned());
    asked.constraints.exec = Some(ExecMode::SandboxExec);
    let job = queued(&jobs, "whole", asked);
    let expected = Demand {
        arch: Some("x86_64".to_owned()),
        exec: Some(ExecMode::SandboxExec),
        network: Reach::AllowList,
        host_ports: vec![7000],
        ..floor()
    };
    assert_eq!(demand(&job, &floor()), expected);
    let plain = JobSpec {
        needs: Needs::WholeHost,
        ..spec()
    };
    let plain = queued(&jobs, "plain", plain);
    assert_eq!(demand(&plain, &floor()), floor());
}

/// Asserts the accelerator a whole-host job asks for: its `accelerator.*` keys narrow the
/// floor's devices (a memory per device); keys of another model than the floor's ask for that
/// model alone, as admission read them, and not for the floor's; and keys that do not read ask
/// for the floor's. It catches a whole-host job's accelerator constraints dropped, which
/// launches a machine of the floor for a job that asks for other devices, again and again.
#[test]
fn a_whole_host_jobs_accelerator_keys_narrow_the_floor() {
    let (_store, jobs) = tables();
    let with = |id: &str, key: &str, value: &str| {
        let mut asked = JobSpec {
            needs: Needs::WholeHost,
            ..spec()
        };
        let caps = &mut asked.constraints.capabilities;
        caps.insert(key.to_owned(), value.to_owned());
        let caps = caps.clone();
        (
            demand(&queued(&jobs, id, asked), &floor()).accelerator,
            caps,
        )
    };
    let (narrowed, caps) = with("memory", "accelerator.memory-gib", "24");
    let floor_resources = floor().resources;
    assert_eq!(
        narrowed,
        AcceleratorNeed::requested(&caps, &floor_resources).unwrap()
    );
    assert!(narrowed.is_some_and(|need| need.memory_per_device.is_some()));

    let (other, caps) = with("other", "accelerator.model", "nvidia-h100");
    assert!(AcceleratorNeed::requested(&caps, &floor_resources).is_err());
    let alone = AcceleratorNeed::requested(&caps, &Resources::default()).unwrap();
    assert_eq!(other, alone);
    assert_ne!(other, floor().accelerator);

    let (unread, _) = with("unread", "accelerator.count", "lots");
    assert_eq!(unread, floor().accelerator);
}

/// Asserts that the hosts in use are those a live allocation is on, a whole-host one
/// included, each once, and none for no allocation. It catches a whole-host allocation not
/// counted, which leaves its host idle to the scaler.
#[test]
fn the_hosts_in_use_are_those_with_a_live_allocation() {
    let (_store, jobs) = tables();
    assert!(in_use(&[]).is_empty());
    let (_, one) = placed(&jobs, "a", "js-h1", spec(), share());
    let (_, two) = placed(&jobs, "b", "js-h1", spec(), share());
    let (_, whole) = placed(&jobs, "c", "js-h2", spec(), Booked::WholeHost);
    let used = in_use(&[one, two, whole]);
    let names: Vec<&str> = used.iter().map(HostId::as_str).collect();
    assert_eq!(names, ["js-h1", "js-h2"]);
}

fn share() -> Booked {
    Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: GIB,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// An offer of eight cores, 64 GiB and one a10, in the form a host's shape takes.
fn a10() -> Offer {
    Offer {
        id: String::new(),
        zone: None,
        cpu_millis: 8000,
        memory_bytes: 64 * GIB,
        disk_bytes: 0,
        accelerator: Some(accelerator_of(
            &Model::parse("nvidia-a10").unwrap(),
            1,
            None,
        )),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// The record of the machine `name` in `phase`.
fn record(name: &str, phase: Phase) -> Record {
    Record {
        pool: "gpu".to_owned(),
        acquire: Some(AcquireId::new(&format!("gpu-{name}")).unwrap()),
        name: name.to_owned(),
        offer: StoredOffer::from_offer(&a10()),
        price_cents_per_hour: 60,
        phase,
        since: Millis::new(0),
        launched_at: Millis::new(0),
        token_expires: None,
        cloud_id: None,
        retry_at: None,
        attempt: 0,
        gone_at: None,
        registered: false,
        condemned: false,
        clean_lists: 0,
        last_clean: None,
        seen_live: true,
        ends_at: None,
    }
}

/// The live, unmarked, idle host `name` that announced [`a10`].
fn host(name: &str) -> HostFacts {
    HostFacts {
        id: HostId::new(name).unwrap(),
        pool: "gpu".to_owned(),
        state: HostState::Live,
        shape: Some(a10()),
        mark: None,
        busy: false,
        lease_given_up: false,
        heard_since_hello: true,
    }
}

/// The queued job `id`, asking for one core and one a10.
fn waiting_job(id: &str) -> Queued {
    let job = JobId::parse(id).unwrap();
    Queued {
        request: request_id(&job).unwrap(),
        job,
        demand: Demand {
            resources: Resources::new(1000, GIB, 1, Some("nvidia-a10".to_owned())).unwrap(),
            ..floor()
        },
    }
}

/// Reservations with the job `job` set aside for the machine `name`, as a step that launched
/// that machine for it leaves them.
fn reserved(job: &str, name: &str) -> Reservations {
    let mut held = Reservations::default();
    let queued = [waiting_job(job)];
    let jobs: BTreeMap<RequestId, JobId> = queued
        .iter()
        .map(|each| (each.request.clone(), each.job.clone()))
        .collect();
    let launched = BTreeMap::from([(name.to_owned(), queued[0].request.clone())]);
    held.note(&launched, &jobs);
    held
}

/// The machine each job's request names, by request id.
fn named(held: &Reservations, queued: Vec<Queued>) -> Vec<Option<String>> {
    let waiting = held.waiting(queued);
    let names = waiting.iter().map(|each| each.reserved.as_ref());
    names
        .map(|name| name.map(|name| name.as_str().to_owned()))
        .collect()
}

/// Asserts that a launch the step names for a job sets that job aside for the machine, so
/// its request names it, while another job's request names nothing; that a launch named
/// for a request no job of the step has sets nothing aside; and that the request keeps its
/// id and demand. It catches a reservation given to the wrong job, which would launch again
/// for the right one.
#[test]
fn a_launch_sets_its_job_aside_for_the_machine() {
    let held = reserved("j1", "js-m1");
    let names = named(&held, vec![waiting_job("j1"), waiting_job("j2")]);
    assert_eq!(names, [Some("js-m1".to_owned()), None]);
    let waiting = held.waiting(vec![waiting_job("j1")]);
    assert_eq!(waiting[0].id.as_str(), "j1");
    assert_eq!(waiting[0].demand, waiting_job("j1").demand);

    let mut none = Reservations::default();
    let stranger = BTreeMap::from([("js-m2".to_owned(), RequestId::new("gone").unwrap())]);
    none.note(&stranger, &BTreeMap::new());
    assert_eq!(none, Reservations::default());
}

/// Asserts that a reservation holds while its machine is on its way, in each of the phases of
/// a machine on its way, and while its machine is registered with an idle host the registry
/// would let take a job and whose shape fits the job; and that one whose machine is a name no host id takes still
/// names nothing. It catches a reservation let go before the job is placed, which lets the
/// idle drain take the host or launches a second machine.
#[test]
fn a_reservation_holds_while_its_machine_is_coming_or_ready() {
    for phase in COMING {
        let mut held = reserved("j1", "js-m1");
        let records = Records {
            instances: vec![record("js-m1", phase)],
            ..Records::default()
        };
        held.settle(&[waiting_job("j1")], &records, (&[], &BTreeSet::new()));
        assert_eq!(held, reserved("j1", "js-m1"), "{phase:?}");
    }
    let mut held = reserved("j1", "js-m1");
    let records = Records {
        instances: vec![record("js-m1", Phase::Registered)],
        ..Records::default()
    };
    let open = BTreeSet::from([HostId::new("js-m1").unwrap()]);
    held.settle(&[waiting_job("j1")], &records, (&[host("js-m1")], &open));
    assert_eq!(held, reserved("j1", "js-m1"));

    let odd = reserved("j1", "Not A Host");
    assert_eq!(named(&odd, vec![waiting_job("j1")]), [None]);
}

/// Asserts each case that lets a reservation go: the job left the queue; the machine has no
/// record, or a record in a phase that is neither on its way nor registered (refused,
/// failed, draining, releasing, gone, orphan); the machine was condemned; and its registered
/// host is not in the registry, is not one the registry would let take a job (not heard
/// from, tainted, cordoned, marked: the engine never places on it), is busy, has announced
/// no shape, or has one the job does not fit. It catches a job held for a machine that will never take it,
/// which would then wait until its queue wait ran out instead of getting a machine.
#[test]
fn a_reservation_is_let_go_when_its_machine_cannot_take_the_job() {
    let at = |phase: Phase| Records {
        instances: vec![record("js-m1", phase)],
        ..Records::default()
    };
    let all_open = BTreeSet::from([HostId::new("js-m1").unwrap()]);
    let gone_if =
        |queued: &[Queued], records: &Records, hosts: &[HostFacts], open: &BTreeSet<HostId>| {
            let mut held = reserved("j1", "js-m1");
            held.settle(queued, records, (hosts, open));
            held == Reservations::default()
        };
    let gone = |queued: &[Queued], records: &Records, hosts: &[HostFacts]| {
        gone_if(queued, records, hosts, &all_open)
    };
    let job = [waiting_job("j1")];
    let ready = [host("js-m1")];
    assert!(gone(&[], &at(Phase::Booting), &ready));
    assert!(gone(&[waiting_job("j2")], &at(Phase::Booting), &ready));
    assert!(gone(&job, &Records::default(), &ready));
    for phase in [
        Phase::Refused,
        Phase::Failed,
        Phase::Draining,
        Phase::Releasing,
        Phase::Gone,
        Phase::Orphan,
    ] {
        assert!(gone(&job, &at(phase), &ready), "{phase:?}");
    }
    let mut condemned = at(Phase::Registered);
    condemned.instances[0].condemned = true;
    assert!(gone(&job, &condemned, &ready));
    let registered = at(Phase::Registered);
    assert!(gone(&job, &registered, &[]));
    assert!(gone_if(&job, &registered, &ready, &BTreeSet::new()));
    assert!(!gone(&job, &registered, &ready));
    type Break = fn(&mut HostFacts);
    let breakers: [Break; 4] = [
        |h| h.busy = true,
        |h| h.shape = None,
        |h| h.shape.as_mut().unwrap().cpu_millis = 500,
        |h| h.id = HostId::new("js-other").unwrap(),
    ];
    for (n, break_it) in breakers.into_iter().enumerate() {
        let mut broken = host("js-m1");
        break_it(&mut broken);
        assert!(gone(&job, &registered, &[broken]), "breaker {n}");
    }
}

/// Asserts what a step hands the placement pass ([`pass_two`]): each refusal for a request
/// that names a job, then each launch for one, charged to that job; a refusal recorded with
/// no request, and a refusal or launch for a request that names no waiting job,
/// are left out. It catches an outcome charged to the wrong job, one handed for a request
/// that names no waiting job, and launches handed before the refusals that came first.
#[test]
fn pass_two_charges_each_refusal_and_launch_to_its_job() {
    use js_sched::failover::Refused;
    let request = |text: &str| RequestId::new(text).unwrap();
    let refused = |request: Option<RequestId>| Refused {
        provider: js_types::ProviderId::new("cloud").unwrap(),
        offer: "big".to_owned(),
        zone: None,
        code: js_elastic::RefusalCode::Capacity,
        passed_over: None,
        request,
    };
    let mut next = State::default();
    next.refusals = vec![
        refused(None),
        refused(Some(request("lease-1"))),
        refused(Some(request("r-job"))),
    ];
    next.launched = BTreeMap::from([
        ("m-lease".to_owned(), request("lease-1")),
        ("m-job".to_owned(), request("r-job")),
    ]);
    let job = JobId::parse("job-1").unwrap();
    let jobs = BTreeMap::from([(request("r-job"), job.clone())]);
    let handed = pass_two(&next, &jobs);
    assert_eq!(
        handed,
        vec![
            (
                job.clone(),
                Outcome::Refused(refused(Some(request("r-job"))))
            ),
            (
                job,
                Outcome::Launched {
                    machine: "m-job".to_owned()
                }
            ),
        ]
    );
}
