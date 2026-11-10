//! The server's pass over the queued jobs walks the Default platform's machine providers in
//! order (design section 5.6, pass 1): a host that joined with the pool's join token is of the
//! `static` provider, one whose hello named an instance of the pool's launching provider, and
//! a free host of the first provider in order takes a job before any other. Everything runs in
//! process on a manual clock; no listener is bound.

use std::time::Duration;

use js_agent_wire::{InstanceIdentity, Taint};
use js_jobs::JobId;
use js_types::HostId;

use super::engine_rig::{GIB, Rig, T0, hello_of, pool, rig_of};
use super::rank::STATIC;
use super::wiring_tests::{config_of, scratch};
use crate::config::Hosts;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// The hello of `name`, launched for the pool: it names an instance, under `provider`.
fn launched(name: &str, provider: &str) -> js_agent_wire::Hello {
    let mut hello = hello_of(name, 16_000, 32 * GIB);
    hello.instance = Some(InstanceIdentity {
        provider: provider.to_owned(),
        instance_id: format!("i-{name}"),
    });
    hello
}

/// The agent side of a server whose pool books 8 cores and 16 GiB of each host, from the
/// flags (no platform), with `static-1` joined with the pool's token and `cloud-1` launched
/// for the pool; `order` replaces the order of no configuration when given.
fn two_providers(name: &str, order: Option<Vec<js_types::ProviderId>>) -> Rig {
    let dir = scratch(name);
    let mut config = config_of(dir.path(), &pool(3, 8_000, ""), &[]);
    if let Hosts::Layered { agents, .. } = &mut config.hosts {
        agents.providers = order;
    }
    let clock = js_types::ManualClock::new(js_types::Timestamp::from_millis(T0));
    let mut r = rig_of(config, dir, clock);
    r.hello(&hello_of("static-1", 16_000, 32 * GIB));
    r.hello(&launched("cloud-1", "burst"));
    r
}

/// Places a job on `on` alone: the `others` are cordoned for the pass.
fn place_on(r: &Rig, id: &str, on: &str, others: &[&str]) {
    let registry = &r.agents.registry;
    for other in others {
        registry
            .cordon(&host(other), Duration::from_secs(600))
            .unwrap();
    }
    assert_eq!(r.place(id, 1_000, GIB).as_deref(), Some(on));
    for other in others {
        registry.uncordon(&host(other)).unwrap();
    }
}

/// The jobs still queued: what the scaler reads its demand from, so none means it launches
/// nothing for them.
fn queued(r: &Rig) -> Vec<JobId> {
    let jobs = r.agents.jobs.as_ref().unwrap();
    jobs.queued()
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect()
}

/// Asserts that with no platform configured (the flags) the order is the one the server
/// always had: of a free joined host and a free launched host, the joined one takes the job,
/// though the launched one holds a job and the score, which fills before it opens, prefers
/// it. It catches a default order that ranks the launched hosts first, and one that ranks
/// every host alike and lets the score decide.
#[test]
fn default_platform_without_config_keeps_static_then_registered() {
    let r = two_providers("walk_default", None);
    place_on(&r, "warm", "cloud-1", &["static-1"]);
    assert_eq!(r.place("job", 1_000, GIB).as_deref(), Some("static-1"));
}

/// Asserts that a free launched host takes a job when the joined host, of the provider
/// listed first, cannot take it now: full under the CPU slack, cordoned (as for a drain) or
/// tainted; and that the job is then not left queued, so the scaler has no demand to launch
/// a machine for. It catches a pass that leaves the job waiting for the first provider's
/// host while a later provider's sits idle.
#[test]
fn a_free_host_of_a_later_provider_is_used_before_any_launch() {
    let full = two_providers("walk_later_full", None);
    assert_eq!(full.place("fill", 8_000, GIB).as_deref(), Some("static-1"));
    assert_eq!(full.place("job", 1_000, GIB).as_deref(), Some("cloud-1"));
    assert!(queued(&full).is_empty());

    let cordoned = two_providers("walk_later_cordoned", None);
    let registry = &cordoned.agents.registry;
    registry
        .cordon(&host("static-1"), Duration::from_secs(600))
        .unwrap();
    assert_eq!(
        cordoned.place("job", 1_000, GIB).as_deref(),
        Some("cloud-1")
    );
    assert!(queued(&cordoned).is_empty());

    let tainted = two_providers("walk_later_tainted", None);
    let taint = Taint {
        reason: "disk-errors".to_owned(),
        run_id: None,
        check: None,
    };
    assert!(tainted.agents.registry.taint_host(&host("static-1"), taint));
    assert_eq!(tainted.place("job", 1_000, GIB).as_deref(), Some("cloud-1"));
    assert!(queued(&tainted).is_empty());
}

/// Asserts that a host's provider comes from its binding alone: `static-2`, launched though
/// named like a joined host, and holding a job the score would fill first, ranks after the
/// joined `static-1`, which takes the next job; `static-2`, whose instance names another
/// provider text than the pool, is offered a job all the same, as the pool's launching
/// provider; and in a platform that lists no launching provider a launched host is offered no
/// job, which waits, while it stays in the host table. It catches a provider inferred from a
/// host's name or its instance's text, and a default rank for a host the platform does not
/// list.
#[test]
fn hosts_rank_by_their_binding_alone() {
    let mut r = two_providers("walk_binding", None);
    r.hello(&launched("static-2", "lambdalabs"));
    place_on(&r, "warm", "static-2", &["static-1", "cloud-1"]);
    assert_eq!(r.place("job", 1_000, GIB).as_deref(), Some("static-1"));
    place_on(&r, "other", "static-2", &["static-1", "cloud-1"]);

    let only_static = two_providers("walk_unlisted", Some(vec![STATIC]));
    only_static
        .agents
        .registry
        .cordon(&host("static-1"), Duration::from_secs(600))
        .unwrap();
    assert_eq!(only_static.place("job", 1_000, GIB), None);
    let listed = only_static.agents.registry.hosts();
    assert!(listed.iter().any(|view| view.id == host("cloud-1")));
}

/// Asserts that a job placed on a later provider's host stays there, booked once, when a host
/// of the first provider frees: the next pass places nothing more. It catches a pass that
/// walks a placed job again (the queue holds only jobs not placed).
#[test]
fn a_placed_job_stays_on_its_host_when_an_earlier_provider_frees() {
    let r = two_providers("walk_placed_stays", None);
    place_on(&r, "job", "cloud-1", &["static-1"]);
    r.agents.place_jobs();
    let jobs = r.agents.jobs.as_ref().unwrap();
    let id = JobId::parse("job").unwrap();
    let allocation = jobs.get_allocation(&id).unwrap().unwrap();
    assert_eq!(allocation.host, host("cloud-1"));
    let live = jobs.live_allocations().unwrap();
    assert_eq!(live.iter().filter(|row| row.job == id).count(), 1);
    assert!(queued(&r).is_empty());
}
