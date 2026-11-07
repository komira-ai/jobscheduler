//! Instances: the list, one instance, ownership marks, launch, terminate, firewall
//! rulesets and ssh keys.

use crate::*;

// ------------------------------------------------------------------- instances

fn instance(id_text: &str, status: HostStatus, region: &str, instance_type: &str) -> Instance {
    Instance {
        id: id(id_text),
        name: None,
        status,
        region: region.to_owned(),
        instance_type: instance_type.to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags: Vec::new(),
    }
}

/// The first instance of `instances.json`, `instance.json` and `instances_page_token.json`.
fn first_instance() -> Instance {
    Instance {
        name: Some("js-0123abcd-000000000001".to_owned()),
        ip: Some("198.51.100.7".to_owned()),
        private_ip: Some("192.0.2.7".to_owned()),
        hostname: Some("host-7".to_owned()),
        first_healthy: Some(at(2026, Month::October, 20, (12, 30, 45), 0)),
        tags: vec![tag("jobscheduler:pool", "fixture")],
        ..instance(FIRST, HostStatus::Active, "region-a", "gpu_1x_h100_sxm5")
    }
}

#[test]
fn the_instance_list_is_read_field_by_field_for_every_status() {
    let server = serving(LIST, "instances.json");
    let list = connect(&server).list_instances().unwrap();
    let expected = vec![
        Instance {
            first_healthy: Some(at(2026, Month::October, 20, (12, 30, 45), 500)),
            tags: vec![tag("jobscheduler:pool", "fixture"), tag("team", "x")],
            ..first_instance()
        },
        instance(
            "b1b2c3d4e5f60718293a4b5c6d7e8f91",
            HostStatus::Booting,
            "region-b",
            "gpu_1x_h100_sxm5",
        ),
        // `tags: null`.
        Instance {
            name: Some("renamed by a person".to_owned()),
            first_healthy: Some(at(2026, Month::October, 19, (8, 0, 0), 0)),
            ..instance(
                "c1b2c3d4e5f60718293a4b5c6d7e8f92",
                HostStatus::Terminated,
                "region-a",
                "gpu_8x_h100_sxm5",
            )
        },
        // A status the client does not know, and a timestamp that is not one.
        instance(
            "d1b2c3d4e5f60718293a4b5c6d7e8f93",
            HostStatus::Unknown("paused-for-review".to_owned()),
            "region-a",
            "gpu_1x_h100_sxm5",
        ),
        instance(
            "e1b2c3d4e5f60718293a4b5c6d7e8f94",
            HostStatus::Preempted,
            "region-a",
            "gpu_1x_h100_sxm5",
        ),
        instance(
            "f1b2c3d4e5f60718293a4b5c6d7e8f95",
            HostStatus::Unhealthy,
            "region-a",
            "gpu_1x_h100_sxm5",
        ),
        instance(
            "01b2c3d4e5f60718293a4b5c6d7e8f96",
            HostStatus::Terminating,
            "region-a",
            "gpu_1x_h100_sxm5",
        ),
    ];
    assert_eq!(list, expected);
}

#[test]
fn one_instance_is_read_whole_by_its_id() {
    let path = format!("GET /api/v1/instances/{FIRST}");
    let server = serving(&path, "instance.json");
    let found = connect(&server).get_instance(&id(FIRST)).unwrap();
    assert_eq!(found, first_instance());
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, format!("/api/v1/instances/{FIRST}"));
}

#[test]
fn an_empty_list_is_an_empty_list() {
    let server = serving(LIST, "instances_empty.json");
    assert_eq!(connect(&server).list_instances().unwrap(), []);
}

#[test]
fn a_list_with_a_page_token_is_incomplete_and_the_client_asks_for_no_second_page() {
    // The API documents no paging. An answer that carries a token anyway is not a whole
    // list: the client says so, and does not send the token back.
    let server = serving(LIST, "instances_page_token.json");
    server.on(LIST, Reply::json(200, &fixture("instances.json")));
    let err = connect(&server).list_instances().unwrap_err();
    assert_eq!(err, ApiError::Incomplete);
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, "/api/v1/instances");
    assert!(!seen[0].target.contains("EXAMPLE-PAGE-TOKEN-1"));
}

// ----------------------------------------------------------------- ownership marks

fn held(id_text: &str, acquire_id: &str, name: &str, status: HostStatus) -> Held {
    Held {
        id: HostRef(id_text.to_owned()),
        acquire: acquire_id.to_owned(),
        name: Some(name.to_owned()),
        offer: "gpu_1x_h100_sxm5".to_owned(),
        zone: Some("region-a".to_owned()),
        status,
        ends_unix_seconds: None,
    }
}

const OWNED_1: &str = "0a1b2c3d4e5f60718293a4b5c6d7e801";
const OWNED_2: &str = "0a1b2c3d4e5f60718293a4b5c6d7e802";
const OWNED_3: &str = "0a1b2c3d4e5f60718293a4b5c6d7e803";
const OWNED_4: &str = "0a1b2c3d4e5f60718293a4b5c6d7e804";
const RENAMED: &str = "0a1b2c3d4e5f60718293a4b5c6d7e805";

#[test]
fn the_names_in_the_ownership_fixture_are_the_ones_the_scheduler_derives() {
    let server = serving(LIST, "instances_by_owner.json");
    let list = connect(&server).list_instances().unwrap();
    for (row, text) in list.iter().zip(["acq-1", "acq-2", "acq-3", "acq-4"]) {
        let name = HostName::derive(&scheduler(), &acquire(text)).to_string();
        assert_eq!(row.name.as_deref(), Some(name.as_str()), "{text}");
    }
    // The fixture's other scheduler derives the same acquire ids under its own prefix.
    let other = HostName::derive(&SchedulerId::new("sched-two").unwrap(), &acquire("acq-1"));
    assert_eq!(list[5].name.as_deref(), Some(other.as_str()));
}

#[test]
fn the_provider_lists_only_the_instances_that_carry_this_schedulers_marks() {
    let server = serving(LIST, "instances_by_owner.json");
    let provider = provider(&server, &rows(&["gpu_1x_h100_sxm5"]), 4);
    let ours = provider.list().unwrap();
    let name = |text: &str| HostName::derive(&scheduler(), &acquire(text)).to_string();
    // The instance renamed by a person (tags whole, name gone), the other scheduler's
    // instance and the unmarked one are left out; the terminated one is still ours.
    assert_eq!(
        ours,
        [
            held(OWNED_1, "acq-1", &name("acq-1"), HostStatus::Active),
            held(OWNED_2, "acq-2", &name("acq-2"), HostStatus::Booting),
            held(OWNED_3, "acq-3", &name("acq-3"), HostStatus::Terminated),
            held(OWNED_4, "acq-4", &name("acq-4"), HostStatus::Preempted),
        ]
    );
}

#[test]
fn a_sweep_sorts_the_marked_instances_into_orphans_ending_and_suspects() {
    let server = serving(LIST, "instances_by_owner.json");
    let provider = provider(&server, &rows(&["gpu_1x_h100_sxm5"]), 4);
    let name = |text: &str| HostName::derive(&scheduler(), &acquire(text)).to_string();
    let swept = provider.sweep(&[acquire("acq-1")]).unwrap();
    assert_eq!(
        swept,
        Sweep {
            // Booked (acq-1) and terminated (acq-3) are in no list.
            orphans: vec![held(OWNED_2, "acq-2", &name("acq-2"), HostStatus::Booting)],
            ending: vec![held(
                OWNED_4,
                "acq-4",
                &name("acq-4"),
                HostStatus::Preempted
            )],
            suspects: vec![held(
                RENAMED,
                "acq-9",
                "renamed by a person",
                HostStatus::Active
            )],
        }
    );
}

#[test]
fn the_ownership_fixture_keeps_every_tag_and_every_address() {
    let server = serving(LIST, "instances_by_owner.json");
    let list = connect(&server).list_instances().unwrap();
    assert_eq!(list.len(), 7);
    assert_eq!(
        list[0].tags,
        [
            tag("jobscheduler:scheduler", "sched-one"),
            tag("jobscheduler:acquire", "acq-1"),
            tag("jobscheduler:pool", "pool-a"),
            tag("jobscheduler:team", "ml"),
        ]
    );
    assert_eq!(list[0].ip.as_deref(), Some("203.0.113.11"));
    assert_eq!(
        list[0].first_healthy,
        Some(at(2026, Month::October, 20, (12, 30, 45), 0))
    );
    // booting: no address, never healthy.
    assert_eq!((list[1].ip.as_deref(), list[1].first_healthy), (None, None));
    // another scheduler's marks, and an unmarked instance with an empty tag list.
    assert_eq!(list[5].tags[0], tag("jobscheduler:scheduler", "sched-two"));
    assert!(list[6].tags.is_empty());
    assert_eq!(list[6].name.as_deref(), Some("notebook"));
}

#[test]
fn a_read_by_id_is_present_or_absent_as_the_documented_404_says() {
    let server = Server::plain();
    server.on(LIST, Reply::json(200, &fixture("instances_empty.json")));
    server.on(
        &one(FIRST),
        Reply::json(404, &fixture("error_object_does_not_exist.json")),
    );
    server.on(
        &one(FIRST),
        Reply::json(404, &fixture("error_not_found.json")),
    );
    server.on(&one(FIRST), Reply::json(200, &fixture("instance.json")));
    let provider = provider(&server, &rows(&["gpu_1x_h100_sxm5"]), 2);
    let host = HostRef(FIRST.to_owned());
    // The documented 404 counts after a list has worked; the generic one never does.
    assert_eq!(provider.observe(&host), Observed::Absent);
    assert_eq!(provider.observe(&host), Observed::Unknown);
    assert_eq!(
        provider.observe(&host),
        Observed::Present(HostStatus::Active)
    );
}

// ------------------------------------------------------------ launch and terminate

#[test]
fn a_launch_answer_gives_the_ids_it_lists() {
    let server = serving(LAUNCH, "launch.json");
    let ids = connect(&server).launch(&request()).unwrap();
    assert_eq!(ids, [id(LAUNCHED)]);
}

#[test]
fn a_terminate_answer_gives_the_ids_of_the_instances_it_lists() {
    let server = Server::plain();
    server.on(TERMINATE, Reply::json(200, &fixture("terminate.json")));
    server.on(TERMINATE, Reply::json(200, &fixture("terminate_none.json")));
    let api = connect(&server);
    // The terminated row carries a field of a newer API and every field of an instance;
    // only its id is read.
    assert_eq!(api.terminate(&id(LAUNCHED)).unwrap(), [id(LAUNCHED)]);
    assert_eq!(api.terminate(&id(LAUNCHED)).unwrap(), []);
}

#[test]
fn a_firewall_ruleset_answer_gives_id_name_region_and_the_number_of_rules() {
    let server = Server::plain();
    server.on(
        RULESETS,
        Reply::json(200, &fixture("firewall_rulesets.json")),
    );
    server.on(
        MAKE_RULESET,
        Reply::json(200, &fixture("firewall_ruleset.json")),
    );
    let api = connect(&server);
    let ruleset = |id: &str, name: &str, rules: usize| FirewallRuleset {
        id: id.to_owned(),
        name: name.to_owned(),
        region: "region-a".to_owned(),
        rules,
    };
    assert_eq!(
        api.list_firewall_rulesets().unwrap(),
        [
            ruleset("rs-noinbound-a", "js-sched-one-noinbound", 0),
            ruleset("rs-web", "web", 1),
        ]
    );
    assert_eq!(
        api.create_firewall_ruleset("region-a", "js-sched-one-noinbound")
            .unwrap(),
        ruleset("rs-new", "js-sched-one-noinbound", 0)
    );
}

#[test]
fn an_ssh_key_answer_gives_id_and_name_and_a_made_key_keeps_no_private_half() {
    let server = Server::plain();
    server.on(KEYS, Reply::json(200, &fixture("ssh_keys.json")));
    server.on(
        MAKE_KEY,
        Reply::json(200, &fixture("ssh_key_generated.json")),
    );
    let api = connect(&server);
    let key = |id: &str, name: &str| SshKey {
        id: id.to_owned(),
        name: name.to_owned(),
    };
    assert_eq!(
        api.list_ssh_keys().unwrap(),
        [key("k1", "alpha"), key("k2", "beta")]
    );
    let made = api.create_ssh_key("js-sched-one").unwrap();
    // The type has two fields, neither of them a key.
    assert_eq!(made, key("k3", "js-sched-one"));
    assert!(!format!("{made:?}").contains("PRIVATE"));
}
