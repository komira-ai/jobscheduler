//! Disk and execution mode on a job: the demand a lease request makes carries them, and
//! offers, hosts and machines on their way are judged by them.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use js_elastic::{Dimension, Offer};
use js_scaler::{StoredExec, Waiting};
use js_types::{ClassId, ExecMode, Request, RequestId, Requirements};

/// The waiting request `id` of a lease request that requires `pairs` of its host: the
/// demand is read from the request, as the registry reads it.
fn asking(id: &str, pairs: &[(&str, &str)]) -> Waiting {
    let asked = Request {
        class: ClassId::new("whole-node").unwrap(),
        resources: request("r").demand.resources,
        capabilities: pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    Waiting {
        id: RequestId::new(id).unwrap(),
        demand: Requirements::from_request(&asked),
        reserved: None,
    }
}

/// A change to a host or a stored offer, what the request requires, and how many
/// launches are left to make.
type Case<'a, T> = (fn(&mut T), &'a [(&'a str, &'a str)], usize);

fn miss(offers: Vec<Offer>, want: Waiting) -> (usize, Option<Dimension>) {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = offers;
    let key = want.id.clone();
    rig.input.waiting = vec![want];
    let launched = acquires(&rig.tick(at(0))).len();
    (launched, rig.state.no_offer.get(&key).copied())
}

/// Asserts that an offer with less disk than the request requires is refused and the miss
/// is named as disk, that an offer with exactly the disk required is launched, and that
/// a request that names no disk takes an offer of any size.
#[test]
fn an_offer_with_too_little_disk_is_refused_and_the_edge_is_accepted() {
    let disk = |gib: u64| Offer {
        disk_bytes: gib << 30,
        ..offer("a10", 60)
    };
    let need = [("disk-gib", "100")];
    let short = miss(vec![disk(99)], asking("r1", &need));
    assert_eq!(short, (0, Some(Dimension::DiskBytes)));
    let edge = miss(vec![disk(100)], asking("r1", &need));
    assert_eq!(edge, (1, None));
    let none = miss(vec![disk(1)], asking("r1", &[]));
    assert_eq!(none, (1, None));
}

/// Asserts that an offer whose hosts run the other mode is refused for a request that
/// requires a mode, with the miss named as the mode, and is launched for a request that
/// names no mode.
#[test]
fn an_offer_of_the_other_mode_is_refused_and_the_miss_is_named() {
    let sandbox = Offer {
        exec: ExecMode::SandboxExec,
        ..offer("mac", 60)
    };
    let container = miss(
        vec![sandbox.clone()],
        asking("r1", &[("exec", "container")]),
    );
    assert_eq!(container, (0, Some(Dimension::Exec)));
    let wants = miss(
        vec![sandbox.clone()],
        asking("r1", &[("exec", "sandbox-exec")]),
    );
    assert_eq!(wants, (1, None));
    assert_eq!(miss(vec![sandbox], asking("r1", &[])), (1, None));
}

/// Asserts that a free host covers a request only when its disk is at least what the
/// request requires (the exact edge covers) and its mode is the one required: a host that
/// does not leaves the request to a launch.
#[test]
fn a_host_covers_a_request_only_if_disk_and_mode_fit() {
    let cases: [Case<js_scaler::HostFacts>; 5] = [
        (
            |h| shape(h).disk_bytes = 99 << 30,
            &[("disk-gib", "100")],
            1,
        ),
        (
            |h| shape(h).disk_bytes = 100 << 30,
            &[("disk-gib", "100")],
            0,
        ),
        (
            |h| shape(h).exec = ExecMode::SandboxExec,
            &[("exec", "container")],
            1,
        ),
        (
            |h| shape(h).exec = ExecMode::SandboxExec,
            &[("exec", "sandbox-exec")],
            0,
        ),
        (|h| shape(h).exec = ExecMode::SandboxExec, &[], 0),
    ];
    for (n, (change, pairs, launches)) in cases.into_iter().enumerate() {
        let mut rig = Rig::new(settings("gpu"));
        let mut host = host("h1", "gpu");
        change(&mut host);
        rig.input.hosts = vec![host];
        rig.input.waiting = vec![asking("r1", pairs)];
        assert_eq!(acquires(&rig.tick(at(0))).len(), launches, "case {n}");
    }
}

/// Asserts that a machine on its way covers a request only if the disk and mode of its
/// stored offer fit: one with less disk, or the other mode, leaves the request to a
/// launch of its own, and one that fits covers it.
#[test]
fn a_machine_on_its_way_covers_a_request_only_if_disk_and_mode_fit() {
    let cases: [Case<js_scaler::StoredOffer>; 4] = [
        (|o| o.disk_bytes = 99 << 30, &[("disk-gib", "100")], 1),
        (|o| o.disk_bytes = 100 << 30, &[("disk-gib", "100")], 0),
        (
            |o| o.exec = StoredExec::SandboxExec,
            &[("exec", "container")],
            1,
        ),
        (
            |o| o.exec = StoredExec::SandboxExec,
            &[("exec", "sandbox-exec")],
            0,
        ),
    ];
    for (n, (change, pairs, launches)) in cases.into_iter().enumerate() {
        let mut rig = Rig::new(settings("gpu"));
        rig.input.waiting = vec![request("r0")];
        assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
        change(&mut rig.state.records.instances[0].offer);
        rig.input.waiting = vec![asking("r1", pairs)];
        assert_eq!(
            acquires(&rig.tick(at(5 * SECOND))).len(),
            launches,
            "case {n}"
        );
    }
}
