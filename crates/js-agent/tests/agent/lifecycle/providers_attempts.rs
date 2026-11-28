//! The attempts of a run (`docs/design/mount-providers.md`, section 4): every start that
//! names a provider is given an attempt of its own, larger than every earlier one, named in
//! each call of its sources, so a late release of an earlier start of the same job never
//! takes a later start's tree; and a release still owed for an earlier start is dropped when
//! the job is started here again.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::rc::Rc;

use js_agent::lifecycle::{Event, ProviderAnswer, ProviderCall, Providers, code};
use js_agent_wire::{HeldState, JobStartSpec};
use serde_json::json;

use super::fake_providers::{WAIT_MS, bare, rig, run_one, tree, with_layer};
use super::per_job::{job_start, listing};
use super::{Harness, names, reply_json};

/// A source as a provider that reads attempts holds it: the run, the attempt and the index.
type Held = (String, u64, u32);

#[derive(Default)]
struct Shelf {
    /// The trees made, by source.
    trees: BTreeSet<Held>,
    /// Every call sent, in order.
    sent: Vec<ProviderCall>,
    /// Whether a `Release` stays out (`Pending`) until [`Trees::land`].
    hold_releases: bool,
    /// The releases out, in the order sent.
    out: Vec<ProviderCall>,
    /// The releases that landed and are not yet handed over.
    landed: Vec<ProviderCall>,
}

/// A provider that reads attempts: each attempt of a run is a tree of its own, a `Release`
/// removes only its own attempt's, and a `Release` may be held out to land later, as one the
/// provider took long to answer does.
#[derive(Clone)]
struct Trees {
    shelf: Rc<RefCell<Shelf>>,
    /// The root the trees' directories are in.
    root: String,
}

impl Trees {
    fn new(h: &Harness) -> Self {
        Self {
            shelf: Rc::default(),
            root: tree(h, "ro", ""),
        }
    }

    /// From now on a `Release` stays out until it lands.
    fn hold_releases(&self) {
        self.shelf.borrow_mut().hold_releases = true;
    }

    /// Every release out lands now: each removes its own attempt's tree. The agent is handed
    /// the answer when it asks again.
    fn land(&self) {
        let mut shelf = self.shelf.borrow_mut();
        shelf.hold_releases = false;
        let out = std::mem::take(&mut shelf.out);
        for call in out {
            if let ProviderCall::Release {
                run_id,
                attempt,
                index,
            } = &call
            {
                shelf.trees.remove(&(run_id.clone(), *attempt, *index));
            }
            shelf.landed.push(call);
        }
    }

    fn trees(&self) -> Vec<Held> {
        self.shelf.borrow().trees.iter().cloned().collect()
    }

    /// The calls sent of the kind of `like`, in order.
    fn sent(&self, like: fn(&ProviderCall) -> bool) -> Vec<ProviderCall> {
        let shelf = self.shelf.borrow();
        shelf
            .sent
            .iter()
            .filter(|call| like(call))
            .cloned()
            .collect()
    }
}

impl Providers for Trees {
    fn has(&self, name: &str) -> bool {
        name == "tier"
    }

    fn names(&self) -> Vec<String> {
        vec!["tier".to_owned()]
    }

    fn ask(&mut self, _provider: &str, call: &ProviderCall) -> ProviderAnswer {
        let mut shelf = self.shelf.borrow_mut();
        if let Some(at) = shelf.landed.iter().position(|landed| landed == call) {
            shelf.landed.remove(at);
            return ProviderAnswer::Done(None);
        }
        if shelf.out.contains(call) {
            return ProviderAnswer::Pending;
        }
        shelf.sent.push(call.clone());
        match call {
            ProviderCall::Prepare {
                run_id,
                attempt,
                index,
                ..
            } => {
                shelf.trees.insert((run_id.clone(), *attempt, *index));
                ProviderAnswer::Done(None)
            }
            ProviderCall::Mount {
                run_id,
                attempt,
                index,
            } => ProviderAnswer::Done(Some(format!("{}{run_id}-a{attempt}-{index}", self.root))),
            ProviderCall::Release { .. } if shelf.hold_releases => {
                shelf.out.push(call.clone());
                ProviderAnswer::Pending
            }
            ProviderCall::Release {
                run_id,
                attempt,
                index,
            } => {
                shelf.trees.remove(&(run_id.clone(), *attempt, *index));
                ProviderAnswer::Done(None)
            }
            ProviderCall::Sweep { .. } => ProviderAnswer::Done(None),
        }
    }
}

fn is_prepare(call: &ProviderCall) -> bool {
    matches!(call, ProviderCall::Prepare { .. })
}

fn is_release(call: &ProviderCall) -> bool {
    matches!(call, ProviderCall::Release { .. })
}

/// The `Release` of source 0 of the attempt `attempt` of `run_id`.
fn release_of(run_id: &str, attempt: u64) -> ProviderCall {
    ProviderCall::Release {
        run_id: run_id.to_owned(),
        attempt,
        index: 0,
    }
}

/// Steps `h` until `job_id` is listed in `state`, a bounded number of times.
fn until(h: &mut Harness, job_id: &str, state: HeldState) -> Vec<Event> {
    let mut events = Vec::new();
    for _ in 0..8 {
        if h.life
            .held_jobs()
            .iter()
            .any(|job| job.job_id == job_id && job.state == state)
        {
            return events;
        }
        events.extend(h.life.step());
    }
    panic!("{job_id} is not {state:?}: {:?}", names(&events));
}

/// Applies a reply that acknowledges the result of `job_id`, and steps until it is let go.
fn let_go(h: &mut Harness, job_id: &str) {
    let reply = reply_json(&json!({ "job_acks": [{ "job_id": job_id, "ack": "ACCEPTED" }] }));
    h.life.apply(&reply);
    for _ in 0..8 {
        if h.life.held_jobs().is_empty() {
            return;
        }
        h.life.step();
    }
    panic!("{job_id} is still held");
}

/// Asserts the case of a job given to this agent again while the release of its last start
/// is still out: the first start's calls name attempt 1 and the second's attempt 2; the second
/// start's admission drops the release still owed for attempt 1 (one `provider-release-failed`
/// finding, word for word, and that release is never asked again); the release of attempt 1
/// that was out lands after attempt 2 is prepared, names attempt 1, and leaves attempt 2's
/// tree in place, which the second start keeps running on; and the second start's end
/// releases attempt 2, after which no tree is left and the run's record is gone. Catches a
/// late release that names no attempt (it would take the retry's tree at a provider that
/// reads attempts), a retry given the same attempt, and an owed release still asked after the
/// retry is prepared (at a provider that does not read attempts it takes the retry's tree).
#[test]
fn a_late_release_of_an_earlier_attempt_leaves_the_retrys_tree() {
    let mut h = bare("prov-attempts-late");
    let trees = Trees::new(&h);
    h.life.attach_providers(Box::new(trees.clone()), WAIT_MS);
    h.apply(vec![job_start(with_layer("j1"))]);
    until(&mut h, "j1", HeldState::Running);
    assert_eq!(trees.trees(), [("j1".to_owned(), 1, 0)]);

    trees.hold_releases();
    h.fake.exit("j1", 0);
    for _ in 0..6 {
        h.life.step();
    }
    assert_eq!(trees.sent(is_release), [release_of("j1", 1)]);
    let_go(&mut h, "j1");

    let again = JobStartSpec {
        allocation_id: "alloc-j1-again".to_owned(),
        ..with_layer("j1")
    };
    let admitted = h.apply(vec![job_start(again)]);
    let dropped: Vec<&str> = admitted
        .iter()
        .filter_map(|event| match event {
            Event::Finding(found) if found.code == code::PROVIDER_RELEASE_FAILED => {
                Some(found.message.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        dropped,
        [
            "attempt 1 of the run ended 0 s ago and is not released: tier source 0; no longer \
          asked, as attempt 2 of the run is prepared now, and left to the providers' own \
          collection"
        ]
    );
    until(&mut h, "j1", HeldState::Running);
    let prepared: Vec<u64> = trees
        .sent(is_prepare)
        .iter()
        .map(|call| call.key().2)
        .collect();
    assert_eq!(prepared, [1, 2]);

    // The release of attempt 1 that was out lands now, after attempt 2 is prepared.
    trees.land();
    assert_eq!(trees.trees(), [("j1".to_owned(), 2, 0)]);
    for _ in 0..6 {
        h.life.step();
    }
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    assert_eq!(
        trees.sent(is_release),
        [release_of("j1", 1)],
        "the dropped release is not asked again"
    );

    h.fake.exit("j1", 0);
    for _ in 0..6 {
        h.life.step();
    }
    assert_eq!(
        trees.sent(is_release),
        [release_of("j1", 1), release_of("j1", 2)]
    );
    assert!(trees.trees().is_empty());
    assert!(!h.dir.path("state/providers/j1.json").exists());
}

/// Asserts what a restart does with attempts: the record of a run names its attempt; the
/// releases of the runs not adopted name the attempt their records name, 0 for a record
/// written before attempts were counted; the sweep keeps the adopted run from its own attempt;
/// and the first start after the restart is given an attempt larger than every attempt a
/// record named. Catches a restart that counts attempts from 1 again (a start could reuse the
/// attempt of a run still held), a release of a recorded run under another attempt than its
/// own (the provider would not find its source), and a sweep that keeps a run whatever its
/// attempt.
#[test]
fn a_restart_counts_attempts_past_every_record() {
    let (mut h, providers) = rig("prov-attempts-restart", |_| {});
    run_one(&mut h, &providers, "j1");
    let dir = h.dir.path("state/providers");
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("j1.json")).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({ "attempt": 1, "sources": [{ "provider": "tier", "index": 0 }] })
    );
    let far = r#"{"attempt":9,"sources":[{"provider":"tier","index":0}]}"#;
    fs::write(dir.join("far.json"), far).unwrap();
    let old = r#"{"sources":[{"provider":"tier","index":1}]}"#;
    fs::write(dir.join("old.json"), old).unwrap();

    let adopted = h.restart().expect("the runtime lists its containers");
    assert_eq!(names(&adopted), ["adopted j1"]);
    h.life
        .attach_providers(Box::new(providers.clone()), WAIT_MS);
    assert_eq!(
        providers.attempts("sweep").last(),
        Some(&("provider sweep tier keep=[\"j1\"]".to_owned(), vec![1]))
    );
    assert_eq!(
        providers.attempts("release"),
        [
            ("provider release tier far#0".to_owned(), vec![9]),
            ("provider release tier old#1".to_owned(), vec![0]),
        ]
    );
    let path = tree(&h, "ro", "j2-0");
    providers.mounts_at("tier", "j2", 0, &path);
    h.apply(vec![job_start(with_layer("j2"))]);
    let prepared = providers.attempts("prepare");
    assert_eq!(
        prepared.last().map(|(_, attempts)| attempts.as_slice()),
        Some([10].as_slice())
    );
    assert_eq!(
        providers
            .attempts("mount")
            .last()
            .map(|(line, attempts)| (line.as_str(), attempts.as_slice())),
        Some(("provider mount tier j2#0", [10].as_slice()))
    );
}
