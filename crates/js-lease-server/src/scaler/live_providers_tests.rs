//! The real effects of a pool with two providers: the lists read as one, a release and a
//! read by id sent to the provider that listed or launched the machine, a launch sent to the
//! provider its plan names, and the later provider's offers read from its cached catalog.

use std::sync::{Arc, Mutex};

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CapacityProvider, CatalogFuture, Claim, Extra,
    Held, HostRef, HostStatus, Instant, Observed, Offer, ProviderError, Released, Secret,
};
use js_scaler::{AcquirePlan, Millis};
use js_types::ProviderId;

use super::tests::{live_with, name, offer};
use crate::catalogs::Catalogs;
use crate::scaler::Effects;

/// A provider `id` that holds the machines `owns`, whose list fails when `broken`, and that
/// logs each call it is asked by the machine it names.
struct Owner {
    id: &'static str,
    owns: Vec<&'static str>,
    broken: bool,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Owner {
    fn new(id: &'static str, owns: &[&'static str], calls: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            id,
            owns: owns.to_vec(),
            broken: false,
            calls: Arc::clone(calls),
        }
    }

    fn note(&self, call: &str, id: &str) {
        let line = format!("{}:{call}:{id}", self.id);
        self.calls.lock().unwrap().push(line);
    }

    fn held(&self, id: &str) -> Held {
        Held {
            id: HostRef(id.to_owned()),
            acquire: "a1".to_owned(),
            name: None,
            offer: offer().id,
            zone: offer().zone,
            status: HostStatus::Active,
            ends_unix_seconds: None,
        }
    }
}

impl CapacityProvider for Owner {
    fn id(&self) -> ProviderId {
        ProviderId::known(self.id)
    }

    fn can_launch(&self) -> bool {
        true
    }

    fn capabilities(&self) -> js_elastic::ProviderCaps {
        js_elastic::ProviderCaps::default()
    }

    fn catalog(&self) -> CatalogFuture<'_> {
        Box::pin(std::future::ready(Ok(vec![offer()])))
    }

    fn capacity(&self) -> js_elastic::Capacity {
        js_elastic::Capacity {
            supplied: 0,
            can_add: None,
        }
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        1 << 20
    }

    /// Launches the machine `i-<id>`, and reports `i-<id>-extra`, a second machine the
    /// launch made, as left running.
    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        let id = format!("i-{}", self.id);
        self.note("acquire", &id);
        let extra = Extra {
            id: HostRef(format!("{id}-extra")),
            released: Released::Unknown,
        };
        Acquired::Accepted {
            held: self.held(&id),
            extras: vec![extra],
        }
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        match self.broken {
            true => Err(ProviderError::new("the list failed")),
            false => Ok(self.owns.iter().map(|id| self.held(id)).collect()),
        }
    }

    fn release(&self, id: &HostRef) -> Released {
        self.note("release", &id.0);
        match self.owns.contains(&id.0.as_str()) {
            true => Released::Terminating(HostStatus::Terminating),
            false => Released::Refused(Claim::Foreign),
        }
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        vec![AcquireId::new(&format!("u-{}", self.id)).unwrap()]
    }

    fn observe(&self, id: &HostRef) -> Observed {
        self.note("observe", &id.0);
        match self.owns.contains(&id.0.as_str()) {
            true => Observed::Present(HostStatus::Active),
            false => Observed::Absent,
        }
    }

    fn verify_instance(&self, _hello: &js_elastic::Hello) -> Option<HostRef> {
        None
    }
}

/// The calls logged since the last read, which it clears.
fn taken(calls: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    std::mem::take(&mut *calls.lock().unwrap())
}

/// Asserts how the machines of two providers, `a` holding `i-a` and `b` holding `i-b`, are
/// told apart: before any list, nothing is known to be a later provider's; a release of `i-b` asks `a`, which refuses it as another's,
/// and then `b`, which ends it; a read by id of `i-b` is gone only if both say so, so not
/// gone; a machine neither holds is gone, and its release is refused as another's after
/// both are asked. Once the lists have been read, which give both providers' machines in
/// order and name `i-b` as `b`'s (and `i-a`, the first provider's, not at all), a release and a read by id of `i-b` go to `b` alone, and the unresolved acquires
/// are both providers'. Catches a release of a later provider's machine sent to the first
/// provider, which refuses it and leaves it billed, a machine read gone because one
/// provider does not hold it, and a list that drops a provider's machines.
#[test]
fn a_machine_goes_to_the_provider_that_holds_it() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let a = Owner::new("a", &["i-a"], &calls);
    let b = Owner::new("b", &["i-b"], &calls);
    let (live, _dir) = live_with("two-owners", vec![Arc::new(a), Arc::new(b)], Arc::default());
    let b_machine = HostRef("i-b".to_owned());
    let terminating = Released::Terminating(HostStatus::Terminating);
    assert_eq!(live.release(&b_machine), terminating);
    assert_eq!(taken(&calls), ["a:release:i-b", "b:release:i-b"]);
    assert!(!live.is_gone(&b_machine));
    let nobody = HostRef("i-nobody".to_owned());
    assert!(live.is_gone(&nobody));
    assert_eq!(live.release(&nobody), Released::Refused(Claim::Foreign));
    taken(&calls);

    assert!(live.listed_by().is_empty());
    let listed = live.list().unwrap();
    let ids: Vec<&str> = listed.iter().map(|each| each.id.0.as_str()).collect();
    assert_eq!(ids, ["i-a", "i-b"]);
    let by = live.listed_by();
    assert_eq!(
        by.into_iter().collect::<Vec<_>>(),
        [("i-b".to_owned(), ProviderId::known("b"))]
    );
    assert_eq!(live.release(&b_machine), terminating);
    assert!(!live.is_gone(&b_machine));
    assert_eq!(taken(&calls), ["b:release:i-b", "b:observe:i-b"]);
    let unresolved = live.unresolved();
    let names: Vec<&str> = unresolved.iter().map(AcquireId::as_str).collect();
    assert_eq!(names, ["u-a", "u-b"]);
}

/// Asserts that when the later provider's list fails, the whole list fails, never the first
/// provider's alone. Catches a failed list read as the first provider's machines, which
/// would take the later provider's machines for gone.
#[test]
fn a_failed_list_of_any_provider_fails_the_whole() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let a = Owner::new("a", &["i-a"], &calls);
    let b = Owner {
        broken: true,
        ..Owner::new("b", &["i-b"], &calls)
    };
    let (live, _dir) = live_with(
        "broken-list",
        vec![Arc::new(a), Arc::new(b)],
        Arc::default(),
    );
    assert_eq!(live.list(), Err(ProviderError::new("the list failed")));
}

/// Asserts that a launch goes to the provider its plan names, `b` here, and that the machine
/// it launched is then released through `b` alone, although `a` would take the release too,
/// and the second machine the launch reported is read by id through `b` alone (which says it
/// is gone, without `a` being asked) and named as
/// `b`'s; that the later provider's offers are
/// those of its catalog once fetched (none before), and the first provider's none of them
/// (the first's come from the checks); and that the count of catalog fetches is both
/// providers'. Catches a launch sent to the first provider whatever the plan says, a
/// launched machine routed to the wrong provider, the first provider's catalog read as a
/// later one's, and a fetch of a later catalog that does not end a request's wait.
#[tokio::test]
async fn a_launch_goes_to_the_provider_its_plan_names() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let a: Arc<dyn CapacityProvider> = Arc::new(Owner::new("a", &["i-b"], &calls));
    let b: Arc<dyn CapacityProvider> = Arc::new(Owner::new("b", &["i-b"], &calls));
    let duration = std::time::Duration::from_secs(3600);
    let catalogs = Catalogs::new()
        .with(Arc::clone(&a), duration, duration)
        .with(Arc::clone(&b), duration, duration);
    let catalogs = Arc::new(catalogs);
    let (live, _dir) = live_with("plan-names", vec![a, b], Arc::clone(&catalogs));
    let now = Millis::new(5_000_000);
    assert!(live.later_offers(now)[&ProviderId::known("b")].is_empty());
    assert_eq!(live.catalog_fetches(), 0);
    catalogs
        .refresh_due(Instant::from_millis(now.as_millis()))
        .await;
    let later = live.later_offers(now);
    assert_eq!(later.keys().collect::<Vec<_>>(), [&ProviderId::known("b")]);
    assert_eq!(later[&ProviderId::known("b")], [offer()]);
    assert_eq!(live.catalog_fetches(), 2);

    let plan = AcquirePlan {
        name: name("a1"),
        acquire: AcquireId::new("a1").unwrap(),
        attempt: 0,
        pool: "burst".to_owned(),
        provider: ProviderId::known("b"),
        offer: offer(),
        lifetime: None,
    };
    let answer = live.acquire(&plan, Secret::new("token".to_owned()));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    let launched = HostRef("i-b".to_owned());
    assert_eq!(
        live.release(&launched),
        Released::Terminating(HostStatus::Terminating)
    );
    let extra = HostRef("i-b-extra".to_owned());
    assert!(live.is_gone(&extra));
    let later = live.listed_by();
    let named: Vec<_> = later.into_iter().collect();
    let b = ProviderId::known("b");
    assert_eq!(
        named,
        [("i-b".to_owned(), b.clone()), ("i-b-extra".to_owned(), b)]
    );
    let calls = taken(&calls);
    assert_eq!(
        calls,
        ["b:acquire:i-b", "b:release:i-b", "b:observe:i-b-extra"]
    );
}
