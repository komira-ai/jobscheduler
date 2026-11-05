//! Probes (`Request::probe_host`): the registry shows the placer what it knows of the host
//! a probe names, books the probe there past the host's cordon only when the placer admits
//! it, never on another host, and answers the placer's refusal by its code.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{Heartbeat, Phase, Taint};
use js_provider::Candidate;
use js_provider::{Pool, RefusalCode, StartAck, StartSpec};
use js_registry::{MarkedTails, Placer, ProbeHost};
use js_types::{HostId, Request, ResourceClass};
use support::*;

/// A placer that answers every probe with `answer`, records what it was told of the
/// probe's host and which hosts each pick was shown, and books the first host shown.
struct Scripted {
    answer: Mutex<Result<(), RefusalCode>>,
    told: Mutex<Vec<Option<ProbeHost>>>,
    shown: Mutex<Vec<Vec<String>>>,
}

impl Scripted {
    fn answering(answer: Result<(), RefusalCode>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            told: Mutex::new(Vec::new()),
            shown: Mutex::new(Vec::new()),
        })
    }

    fn answer(&self, answer: Result<(), RefusalCode>) {
        *self.answer.lock().unwrap() = answer;
    }

    fn told(&self) -> Vec<Option<ProbeHost>> {
        std::mem::take(&mut *self.told.lock().unwrap())
    }

    fn shown(&self) -> Vec<Vec<String>> {
        std::mem::take(&mut *self.shown.lock().unwrap())
    }
}

impl Placer for Scripted {
    fn pick(&self, _: &StartSpec, open: &[Candidate], _: &MarkedTails) -> Option<usize> {
        let shown = open.iter().map(|c| c.host.to_string()).collect();
        self.shown.lock().unwrap().push(shown);
        Some(0)
    }

    fn admit_probe(
        &self,
        spec: &StartSpec,
        pin: &HostId,
        host: Option<&ProbeHost>,
    ) -> Result<(), RefusalCode> {
        assert_eq!(spec.request().probe_host.as_ref(), Some(pin));
        self.told.lock().unwrap().push(host.cloned());
        *self.answer.lock().unwrap()
    }
}

/// A placer that places as a registry with no placer does and says nothing of probes.
struct First;

impl Placer for First {
    fn pick(&self, _: &StartSpec, _: &[Candidate], _: &MarkedTails) -> Option<usize> {
        Some(0)
    }
}

/// Lease `sequence` of [`spec`], a probe pinned to `pin`.
fn probe_spec(sequence: u64, pin: &str) -> StartSpec {
    probe_of(sequence, pin, &class())
}

fn probe_of(sequence: u64, pin: &str, class: &ResourceClass) -> StartSpec {
    let request = Request {
        class: class.id().clone(),
        probe_host: Some(host(pin)),
        ..request()
    };
    StartSpec::new(lease(sequence), request, std::slice::from_ref(class)).unwrap()
}

/// What the registry knows of a live, serving, clean host of the pool that stays.
fn facts(cordoned: bool) -> ProbeHost {
    ProbeHost {
        shape: pool_shape(),
        cordoned,
        phase: Phase::Serving,
        taint: None,
        leaving: false,
    }
}

/// Asserts that a registry with no placer, or with a placer that judges no probe, places
/// none (`ProbeUnsupported`) and books nothing, though the probe's host is free and
/// cordoned. It catches a probe booked by a path that has no probe rule.
#[test]
fn without_a_placer_that_judges_probes_none_is_placed() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    fx.registry
        .cordon(&host("a"), Duration::from_secs(600))
        .unwrap();
    let refused = fx.provider.start(probe_spec(1, "a"));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeUnsupported));
    fx.registry.set_placer(Arc::new(First));
    let refused = fx.provider.start(probe_spec(2, "a"));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeUnsupported));
    assert!(fx.provider.booked().is_empty());
    // The placer's own picks are unchanged.
    fx.registry.uncordon(&host("a")).unwrap();
    assert_eq!(started_on(&fx.provider.start(spec(3))), Some("a"));
}

/// Asserts that a probe the placer admits is booked on its cordoned host, the placer
/// shown that host alone, though another host is open; that the placer is told the
/// host's shape and cordon; and that an ordinary lease started after it still skips the
/// cordoned host. It catches the cordon read for the probe (it would never be placed),
/// the probe booked on another host, and the cordon opened to ordinary work.
#[test]
fn an_admitted_probe_is_booked_past_its_hosts_cordon_and_nowhere_else() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    let placer = Scripted::answering(Ok(()));
    fx.registry.set_placer(placer.clone());
    fx.registry
        .cordon(&host("b"), Duration::from_secs(600))
        .unwrap();
    let probed = fx.provider.start(probe_spec(1, "b"));
    assert_eq!(started_on(&probed), Some("b"));
    assert_eq!(placer.told(), [Some(facts(true))]);
    assert_eq!(placer.shown(), [vec!["b".to_owned()]]);
    let ordinary = fx.provider.start(spec(2));
    assert_eq!(started_on(&ordinary), Some("a"));
    assert_eq!(
        placer.told(),
        [],
        "an ordinary start is not judged as a probe"
    );
    // A repeated start of the probe reads its host back, whatever the placer says now.
    placer.answer(Err(RefusalCode::ProbeHostNotCordoned));
    assert_eq!(
        started_on(&fx.provider.start(probe_spec(1, "b"))),
        Some("b")
    );
}

/// Asserts that a probe the placer refuses is answered the placer's code and books
/// nothing, while the same host would take it if admitted; and that the placer is told
/// `None` for a host the registry does not list live (never joined, or silent past the
/// grace). It catches a refusal swallowed into `NoCapacity`, and facts given for a host
/// that is gone.
#[test]
fn a_refused_probe_is_answered_the_placers_code_and_books_nothing() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    let placer = Scripted::answering(Err(RefusalCode::ProbeHostNotCordoned));
    fx.registry.set_placer(placer.clone());
    let refused = fx.provider.start(probe_spec(1, "a"));
    assert_eq!(
        refused,
        StartAck::Refused(RefusalCode::ProbeHostNotCordoned)
    );
    assert_eq!(placer.told(), [Some(facts(false))]);
    assert_eq!(placer.shown(), Vec::<Vec<String>>::new());
    assert!(fx.provider.booked().is_empty());

    placer.answer(Err(RefusalCode::ProbeHostUnknown));
    let refused = fx.provider.start(probe_spec(2, "nowhere"));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeHostUnknown));
    fx.pass_grace();
    let refused = fx.provider.start(probe_spec(3, "a"));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeHostUnknown));
    assert_eq!(placer.told(), [None, None]);
}

/// Asserts that a probe admitted to a host with no room is `NoCapacity` and is not
/// booked on the free host beside it. It catches a probe that oversubscribes its host or
/// spills to another.
#[test]
fn an_admitted_probe_on_a_full_host_has_no_capacity() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    let placer = Scripted::answering(Ok(()));
    fx.registry.set_placer(placer.clone());
    // The class books the whole host: one lease fills `a`.
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    fx.registry
        .cordon(&host("a"), Duration::from_secs(600))
        .unwrap();
    let full = fx.provider.start(probe_spec(2, "a"));
    assert_eq!(full, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(fx.provider.booked().len(), 1, "b is not booked");
}

/// Asserts that an ended probe lease is answered `LeaseEnded`, not the placer's refusal.
/// It catches the probe answer put before the book's own.
#[test]
fn an_ended_probe_is_answered_lease_ended() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    let placer = Scripted::answering(Err(RefusalCode::ProbeHostNotCordoned));
    fx.registry.set_placer(placer);
    fx.provider.stop(&lease(1));
    let ended = fx.provider.start(probe_spec(1, "a"));
    assert_eq!(ended, StartAck::Refused(RefusalCode::LeaseEnded));
}

/// Asserts that the placer is told a host's taint, its phase and that it is being scaled
/// away, as the registry holds them. It catches facts read from the wrong record or left
/// at a default.
#[test]
fn the_placer_is_told_the_taint_phase_and_leaving_of_the_probes_host() {
    let mut fx = fixture(3);
    let a = fx.join("a", "t");
    let b = fx.join("b", "t");
    fx.join("c", "t");
    let placer = Scripted::answering(Err(RefusalCode::ProbeHostNotServing));
    fx.registry.set_placer(placer.clone());
    let taint = Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    };
    let tainted = Heartbeat {
        taint: Some(taint),
        ..fx.heartbeat("a", a)
    };
    fx.registry.heartbeat(&tainted, &cred("t")).unwrap();
    let draining = Heartbeat {
        phase: Phase::Draining,
        ..fx.heartbeat("b", b)
    };
    fx.registry.heartbeat(&draining, &cred("t")).unwrap();
    fx.registry.drain(&host("c")).unwrap();
    for (sequence, name) in [(1, "a"), (2, "b"), (3, "c")] {
        let refused = fx.provider.start(probe_spec(sequence, name));
        assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeHostNotServing));
    }
    let told = placer.told();
    let expected = [
        ProbeHost {
            taint: Some("residue".to_owned()),
            ..facts(false)
        },
        ProbeHost {
            phase: Phase::Draining,
            ..facts(false)
        },
        ProbeHost {
            leaving: true,
            ..facts(false)
        },
    ];
    assert_eq!(told, expected.map(Some));
}

/// Asserts that a probe of a whole-node class is not placed, and the placer is not asked:
/// a whole-node lease is prepared and held as the queue of whole-node requests decides;
/// and that an ended lease and the class gate still answer before that refusal. It
/// catches a whole-node probe sent down that path, and a probe answer put before the
/// book's own. (A whole-node probe is never booked, so it is never pending or held.)
#[test]
fn a_probe_of_a_whole_node_class_is_not_placed() {
    let whole = ResourceClass::whole_node();
    let config = js_registry::PoolConfig::new(
        js_types::ProviderId::new("builders").unwrap(),
        whole.clone(),
        pool_shape(),
        2,
    );
    let mut fx = fixture_of(config);
    fx.join("a", "t");
    // With no placer, as with one.
    let refused = fx.provider.start(probe_of(9, "a", &whole));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeUnsupported));
    let placer = Scripted::answering(Ok(()));
    fx.registry.set_placer(placer.clone());
    fx.registry
        .cordon(&host("a"), Duration::from_secs(600))
        .unwrap();
    let refused = fx.provider.start(probe_of(1, "a", &whole));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeUnsupported));
    assert_eq!(placer.told(), []);
    assert!(fx.provider.booked().is_empty());
    // An ended lease and the class gate answer first, as for any start.
    fx.provider.stop(&lease(2));
    let ended = fx.provider.start(probe_of(2, "a", &whole));
    assert_eq!(ended, StartAck::Refused(RefusalCode::LeaseEnded));
    let mut other = fx.registry.provider().with_classes(vec![class()]);
    let gated = other.start(probe_of(3, "a", &whole));
    assert_eq!(gated, StartAck::Refused(RefusalCode::UnsupportedClass));
    assert_eq!(placer.told(), []);
}

/// Asserts that the cordon the placer is told of is the one in force at the start: one
/// millisecond before the cordon lapses the host is cordoned, at the moment it lapses it
/// is not. It catches a lapsed cordon still reported (a probe would pass a cordon that is
/// over, onto a host open to ordinary work), and a cordon reported over early.
#[test]
fn the_placer_is_told_the_cordon_in_force_up_to_its_lapse() {
    let mut fx = fixture(2);
    let epoch = fx.join("a", "t");
    let placer = Scripted::answering(Err(RefusalCode::ProbeHostNotCordoned));
    fx.registry.set_placer(placer.clone());
    fx.registry
        .cordon(&host("a"), Duration::from_secs(10))
        .unwrap();
    fx.clock.advance(Duration::from_millis(9_999));
    fx.beat("a", epoch);
    fx.provider.start(probe_spec(1, "a"));
    fx.clock.advance(Duration::from_millis(1));
    fx.beat("a", epoch);
    fx.provider.start(probe_spec(2, "a"));
    assert_eq!(placer.told(), [Some(facts(true)), Some(facts(false))]);
}
