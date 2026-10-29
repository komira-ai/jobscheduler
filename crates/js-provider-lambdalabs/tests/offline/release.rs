//! Release, list, the sweep and the choice of where to launch.

use js_elastic::{
    AcquireId, Claim, Demand, ElasticProvider, Held, HostStatus, Offer, ProviderError, Released,
};
use js_provider_lambdalabs::{
    ACQUIRE_TAG, ApiError, Architecture, Call, ErrorCode, SCHEDULER_TAG, TerminateEffect,
};
use js_types::Resources;

use crate::rig::{INTERVAL, Rig, SCHEDULER, ask, host, id, live, offer, rig, rig_with, row, tag};

/// A rig with one launched instance, `fake0001`.
fn launched() -> Rig {
    let rig = rig(live(5));
    rig.provider.acquire(&ask("a"));
    rig.clock.advance(INTERVAL);
    rig
}

fn api(status: u16, code: ErrorCode) -> ApiError {
    ApiError::Api {
        status,
        code,
        request_id: None,
    }
}

#[test]
fn releasing_ours_terminates_one_id_and_reports_what_the_read_back_showed() {
    let rig = launched();
    assert_eq!(
        rig.provider.release(&host("fake0001")),
        Released::Terminating(HostStatus::Terminating)
    );
    assert_eq!(rig.fake.count(Call::Terminate), 1);
    // One read before the terminate, one after.
    assert_eq!(rig.fake.count(Call::GetInstance), 2);
}

#[test]
fn gone_is_only_a_read_by_id_that_says_the_instance_does_not_exist() {
    let rig = launched();
    rig.fake.set_terminate_effect(TerminateEffect::Removed);
    assert_eq!(rig.provider.release(&host("fake0001")), Released::Gone);
    // And an instance that was never there: releasing a gone instance is a release.
    assert_eq!(rig.provider.release(&host("fake0009")), Released::Gone);
}

#[test]
fn a_404_is_trusted_only_after_a_list_has_worked_once() {
    let rig = launched();
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    // The key may not be able to see anything: a 404 proves nothing yet.
    let lists = rig.fake.count(Call::ListInstances);
    assert_eq!(rig.provider.release(&host("fake0009")), Released::Unknown);
    assert_eq!(rig.fake.count(Call::ListInstances), lists + 1);
    // The probe is tried again, and once it works it is not repeated.
    assert_eq!(rig.provider.release(&host("fake0009")), Released::Gone);
    assert_eq!(rig.provider.release(&host("fake0009")), Released::Gone);
    assert_eq!(rig.fake.count(Call::ListInstances), lists + 2);
}

#[test]
fn a_404_with_any_other_code_is_not_gone() {
    for code in [
        ErrorCode::NotFound,
        ErrorCode::Forbidden,
        ErrorCode::Unparsed,
    ] {
        let rig = launched();
        rig.fake
            .fail_next(Call::GetInstance, api(404, code.clone()));
        assert_eq!(
            rig.provider.release(&host("fake0001")),
            Released::Unknown,
            "{code:?}"
        );
        assert_eq!(rig.fake.count(Call::Terminate), 0);
    }
    // The right code with the wrong status is not the documented answer either.
    let rig = launched();
    rig.fake
        .fail_next(Call::GetInstance, api(400, ErrorCode::ObjectDoesNotExist));
    assert_eq!(rig.provider.release(&host("fake0001")), Released::Unknown);
}

#[test]
fn a_terminated_status_is_not_gone() {
    let rig = launched();
    rig.fake.set_status(&id("fake0001"), HostStatus::Terminated);
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    assert_eq!(
        rig.provider.release(&host("fake0001")),
        Released::Terminating(HostStatus::Terminated)
    );
}

#[test]
fn a_200_that_terminated_nothing_is_not_gone_and_not_terminating() {
    let rig = launched();
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    assert_eq!(rig.provider.release(&host("fake0001")), Released::Unknown);
    assert_eq!(rig.fake.count(Call::Terminate), 1);
    assert_eq!(rig.fake.instances()[0].status, HostStatus::Booting);
}

#[test]
fn a_terminate_whose_answer_was_lost_is_judged_by_the_read_back() {
    let rig = launched();
    rig.fake
        .lose_response(Call::Terminate, ApiError::Transport("reset".to_owned()));
    assert_eq!(
        rig.provider.release(&host("fake0001")),
        Released::Terminating(HostStatus::Terminating)
    );
    // A terminate that failed and left the instance running claims nothing.
    let rig = launched();
    rig.fake
        .fail_next(Call::Terminate, ApiError::Transport("reset".to_owned()));
    assert_eq!(rig.provider.release(&host("fake0001")), Released::Unknown);
    assert_eq!(rig.fake.instances()[0].status, HostStatus::Booting);
}

#[test]
fn a_failed_read_back_claims_nothing() {
    let rig = launched();
    rig.fake.pass_next(Call::GetInstance);
    rig.fake
        .fail_next(Call::GetInstance, ApiError::Transport("down".to_owned()));
    assert_eq!(rig.provider.release(&host("fake0001")), Released::Unknown);
    // The terminate did go out; only the claim is withheld.
    assert_eq!(rig.fake.count(Call::Terminate), 1);
}

#[test]
fn a_preempted_instance_of_ours_is_terminated() {
    let rig = launched();
    rig.fake.set_status(&id("fake0001"), HostStatus::Preempted);
    assert_eq!(
        rig.provider.release(&host("fake0001")),
        Released::Terminating(HostStatus::Terminating)
    );
}

#[test]
fn what_is_not_entirely_ours_is_never_terminated() {
    let rig = launched();
    rig.fake.add_instance(row(
        "theirs",
        Some("box"),
        HostStatus::Active,
        vec![tag("team", "x")],
    ));
    rig.fake.add_instance(row(
        "half",
        Some("box"),
        HostStatus::Active,
        vec![tag(SCHEDULER_TAG, SCHEDULER), tag(ACQUIRE_TAG, "z")],
    ));
    assert_eq!(
        rig.provider.release(&host("theirs")),
        Released::Refused(Claim::Foreign)
    );
    assert_eq!(
        rig.provider.release(&host("half")),
        Released::Refused(Claim::Suspect)
    );
    assert_eq!(rig.fake.count(Call::Terminate), 0);
}

#[test]
fn a_failed_first_read_sends_no_terminate() {
    let rig = launched();
    rig.fake
        .fail_next(Call::GetInstance, ApiError::Transport("down".to_owned()));
    assert_eq!(rig.provider.release(&host("fake0001")), Released::Unknown);
    assert_eq!(rig.fake.count(Call::Terminate), 0);
}

#[test]
fn list_returns_ours_with_their_states_and_an_error_is_not_an_empty_list() {
    let rig = launched();
    rig.provider.acquire(&ask("b"));
    rig.fake
        .set_status(&id("fake0001"), HostStatus::Terminating);
    rig.fake
        .add_instance(row("theirs", Some("box"), HostStatus::Active, vec![]));
    rig.fake.add_instance(row(
        "half",
        Some("box"),
        HostStatus::Active,
        vec![tag(SCHEDULER_TAG, SCHEDULER)],
    ));
    let held: Vec<Held> = rig.provider.list().unwrap();
    let seen: Vec<(String, String, HostStatus)> = held
        .into_iter()
        .map(|h| (h.id.to_string(), h.acquire, h.status))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "fake0001".to_owned(),
                "a".to_owned(),
                HostStatus::Terminating
            ),
            ("fake0002".to_owned(), "b".to_owned(), HostStatus::Booting)
        ]
    );
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Incomplete);
    assert_eq!(
        rig.provider.list(),
        Err(ProviderError::new(ApiError::Incomplete.to_string()))
    );
}

#[test]
fn the_sweep_reports_orphans_suspects_and_the_ending_and_terminates_nothing() {
    let rig = launched();
    rig.provider.acquire(&ask("b"));
    rig.clock.advance(INTERVAL);
    rig.provider.acquire(&ask("c"));
    rig.clock.advance(INTERVAL);
    rig.provider.acquire(&ask("d"));
    rig.fake.set_status(&id("fake0003"), HostStatus::Preempted);
    rig.fake.set_status(&id("fake0004"), HostStatus::Terminated);
    rig.fake
        .add_instance(row("theirs", Some("box"), HostStatus::Active, vec![]));
    rig.fake.add_instance(row(
        "half",
        Some("box"),
        HostStatus::Active,
        vec![tag(SCHEDULER_TAG, SCHEDULER)],
    ));
    rig.fake.add_instance(row(
        "half-done",
        Some("box"),
        HostStatus::Terminated,
        vec![tag(SCHEDULER_TAG, SCHEDULER)],
    ));
    rig.fake
        .set_status(&id("fake0002"), HostStatus::Terminating);
    let booked = [AcquireId::new("a").unwrap()];
    let sweep = rig.provider.sweep(&booked).unwrap();
    let ids = |held: &[Held]| held.iter().map(|h| h.id.to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&sweep.orphans), Vec::<String>::new());
    assert_eq!(ids(&sweep.ending), ["fake0002", "fake0003"]);
    assert_eq!(ids(&sweep.suspects), ["half"]);
    // With nobody booked, the working instance is an orphan too.
    let sweep = rig.provider.sweep(&[]).unwrap();
    assert_eq!(ids(&sweep.orphans), ["fake0001"]);
    assert_eq!(rig.fake.count(Call::Terminate), 0);
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Incomplete);
    assert!(rig.provider.sweep(&[]).is_err());
}

/// What the first-launch runner and the pools ask for: x86_64, this many GPUs of the
/// rig's class.
fn request(gpus: u64) -> Demand {
    Demand {
        resources: Resources::new(4000, 16 << 30, gpus, Some("nvidia-sample".to_owned())).unwrap(),
        disk_bytes: None,
        os: None,
        arch: Some("x86_64".to_owned()),
        exec: None,
    }
}

fn names(rig: &Rig, wanted: &Demand) -> Vec<(String, String)> {
    rig.provider
        .launch_options(wanted)
        .unwrap()
        .into_iter()
        .map(|c| (c.id, c.zone.unwrap()))
        .collect()
}

#[test]
fn candidates_come_cheapest_first_then_fewest_gpus_then_name_then_region() {
    let rig = rig(live(5));
    let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    };
    assert_eq!(
        names(&rig, &request(1)),
        pairs(&[
            ("gpu_1x_sample", "region-a"),
            ("gpu_1x_sample", "region-b"),
            ("gpu_8x_example", "region-a")
        ])
    );
    // More GPUs than the small type has: only the big one.
    assert_eq!(
        names(&rig, &request(2)),
        pairs(&[("gpu_8x_example", "region-a")])
    );
    assert!(names(&rig, &request(9)).is_empty());
    // Equal price: fewer GPUs first; equal GPUs too: name.
    rig.fake.set_offers(vec![
        offer(
            "gpu_8x_example",
            100,
            8,
            Architecture::X86_64,
            &["region-a"],
        ),
        offer("gpu_1x_sample", 100, 1, Architecture::X86_64, &["region-a"]),
    ]);
    let order: Vec<String> = names(&rig, &request(1)).into_iter().map(|c| c.0).collect();
    assert_eq!(order, ["gpu_1x_sample", "gpu_8x_example"]);
}

const TIES: &str = r#"
[[option]]
instance_type = "gpu_1x_a"
gpu_class = "nvidia-sample"
gpus = 1

[[option]]
instance_type = "gpu_1x_b"
gpu_class = "nvidia-sample"
gpus = 1

[[option]]
instance_type = "gpu_1x_d"
gpu_class = "nvidia-sample"
gpus = 1

[[option]]
instance_type = "gpu_8x_c"
gpu_class = "nvidia-sample"
gpus = 8
"#;

#[test]
fn the_choice_is_the_same_whatever_order_the_api_lists_in() {
    let rig = rig_with(live(5), TIES);
    let mut offers = vec![
        offer("gpu_1x_b", 80, 1, Architecture::X86_64, &["r2", "r1", "r1"]),
        offer("gpu_8x_c", 80, 8, Architecture::X86_64, &["r1"]),
        offer("gpu_1x_a", 80, 1, Architecture::X86_64, &["r1"]),
        offer("gpu_1x_d", 70, 1, Architecture::X86_64, &["r9"]),
    ];
    let want: Vec<(String, String)> = [
        ("gpu_1x_d", "r9"),
        ("gpu_1x_a", "r1"),
        ("gpu_1x_b", "r1"),
        ("gpu_1x_b", "r2"),
        ("gpu_8x_c", "r1"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
    .collect();
    for _ in 0..offers.len() {
        offers.rotate_left(1);
        rig.fake.set_offers(offers.clone());
        assert_eq!(names(&rig, &request(1)), want);
        offers.reverse();
        rig.fake.set_offers(offers.clone());
        assert_eq!(names(&rig, &request(1)), want);
    }
}

#[test]
fn a_type_that_does_not_fit_or_has_no_capacity_is_left_out() {
    let rig = rig(live(5));
    let mut arm = offer("gpu_1x_sample", 10, 1, Architecture::Arm64, &["region-a"]);
    // arm64 is not released for.
    rig.fake.set_offers(vec![arm.clone()]);
    assert!(names(&rig, &request(1)).is_empty());
    arm.architecture = Architecture::Other("riscv".to_owned());
    rig.fake.set_offers(vec![arm.clone()]);
    assert!(names(&rig, &request(1)).is_empty());
    // No region with capacity.
    arm.architecture = Architecture::X86_64;
    arm.regions.clear();
    rig.fake.set_offers(vec![arm.clone()]);
    assert!(names(&rig, &request(1)).is_empty());
    // The table row disagrees with the API about the GPU count.
    let mut odd = offer("gpu_1x_sample", 10, 2, Architecture::X86_64, &["region-a"]);
    rig.fake.set_offers(vec![odd.clone()]);
    assert!(names(&rig, &request(1)).is_empty());
    // A type not in the table.
    odd = offer(
        "gpu_9x_unlisted",
        10,
        1,
        Architecture::X86_64,
        &["region-a"],
    );
    rig.fake.set_offers(vec![odd]);
    assert!(names(&rig, &request(1)).is_empty());
}

#[test]
fn cpu_memory_and_class_must_fit() {
    let rig = rig(live(5));
    // gpu_1x_sample in the rig has 8 vcpus and 64 GiB.
    let mut wanted = request(1);
    let sized =
        |cpu, memory| Resources::new(cpu, memory, 1, Some("nvidia-sample".to_owned())).unwrap();
    wanted.resources = sized(8000, 64 << 30);
    assert!(!names(&rig, &wanted).is_empty());
    wanted.resources = sized(8001, 64 << 30);
    assert!(names(&rig, &wanted).iter().all(|c| c.0 != "gpu_1x_sample"));
    wanted.resources = sized(8000, (64 << 30) + 1);
    assert!(names(&rig, &wanted).iter().all(|c| c.0 != "gpu_1x_sample"));
    let other_class = Demand {
        resources: Resources::new(4000, 16 << 30, 1, Some("nvidia-other".to_owned())).unwrap(),
        ..request(1)
    };
    assert!(names(&rig, &other_class).is_empty());
}

#[test]
fn a_failed_read_of_the_types_is_an_error_and_not_an_empty_choice() {
    let rig = rig(live(5));
    rig.fake
        .fail_next(Call::InstanceTypes, ApiError::Transport("down".to_owned()));
    assert!(rig.provider.launch_options(&request(1)).is_err());
}

#[test]
fn the_catalog_has_one_offer_per_region_with_capacity_and_the_facts_of_the_type() {
    let rig = rig(live(5));
    let offers = rig.provider.catalog().unwrap();
    let places: Vec<(&str, &str)> = offers
        .iter()
        .map(|o| (o.id.as_str(), o.zone.as_deref().unwrap()))
        .collect();
    assert_eq!(
        places,
        [
            ("gpu_1x_sample", "region-a"),
            ("gpu_1x_sample", "region-b"),
            ("gpu_8x_example", "region-a")
        ]
    );
    // The rig's 8 vcpus, 64 GiB, 100 GiB of storage, one GPU and 75 cents per hour.
    assert_eq!(offers[0], crate::rig::candidate("region-a"));
    assert_eq!(offers[2].gpus, 8);
    assert_eq!(offers[2].price_cents_per_hour, Some(1200));
}

#[test]
fn the_catalog_carries_every_architecture_and_leaves_the_choice_to_the_demand() {
    let rig = rig(live(5));
    for (architecture, name) in [
        (Architecture::X86_64, "x86_64"),
        (Architecture::Arm64, "aarch64"),
        (Architecture::Other("riscv".to_owned()), "riscv"),
    ] {
        rig.fake.set_offers(vec![offer(
            "gpu_1x_sample",
            10,
            1,
            architecture,
            &["region-a"],
        )]);
        let offers: Vec<Offer> = rig.provider.catalog().unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].arch, name);
        assert_eq!(offers[0].os, "linux");
    }
}

#[test]
fn a_catalog_that_cannot_be_read_is_an_error_and_not_an_empty_catalog() {
    let rig = rig(live(5));
    let down = ApiError::Transport("down".to_owned());
    rig.fake.fail_next(Call::InstanceTypes, down.clone());
    assert_eq!(
        rig.provider.catalog(),
        Err(ProviderError::new(down.to_string()))
    );
}

#[test]
fn the_provider_prepares_the_host_and_names_its_payload_limit() {
    let rig = rig(live(5));
    let mut roomy = crate::rig::candidate("region-a");
    roomy.disk_bytes = 512 << 30;
    assert_eq!(rig.provider.host_prep(&roomy).len(), 8);
    assert_eq!(
        rig.provider.payload_limit(),
        js_provider_lambdalabs::PAYLOAD_LIMIT
    );
}

#[test]
fn a_reference_that_is_no_instance_id_is_foreign_and_asks_the_api_nothing() {
    let rig = launched();
    let calls = rig.fake.calls().len();
    for bad in ["", "a b", "a/b", "../x"] {
        assert_eq!(
            rig.provider.release(&js_elastic::HostRef(bad.to_owned())),
            Released::Refused(Claim::Foreign),
            "{bad:?}"
        );
    }
    assert_eq!(rig.fake.calls().len(), calls);
}

#[test]
fn the_provider_is_usable_through_the_trait_object() {
    let rig = launched();
    let dynamic: &dyn ElasticProvider = &rig.provider;
    let held = dynamic.list().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(
        dynamic.release(&held[0].id),
        Released::Terminating(HostStatus::Terminating)
    );
}
