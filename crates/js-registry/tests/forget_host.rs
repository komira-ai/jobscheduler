//! The forgetting of one host now (`Registry::forget_host`), for the operator's retirement of
//! a host id: a lost or awaiting host leaves the pool's count, the bindings and the placer's
//! picture, and a live host is kept.

mod support;

use std::sync::{Arc, Mutex};

use js_registry::{Binding, Credential, Placer, Refusal};
use js_types::HostId;
use support::*;

/// A placer that writes down the hosts it is told are gone.
#[derive(Default)]
struct Gone {
    hosts: Mutex<Vec<String>>,
}

impl Placer for Gone {
    fn host_gone(&self, host: &HostId) {
        self.hosts.lock().unwrap().push(host.as_str().to_owned());
    }
}

/// Asserts that a pool at its cap of one refuses a new id while its one host is held; that the
/// host is kept while it is live, then forgotten once it is lost (it leaves the listing and the
/// bindings, and the placer is told it is gone, once), after which the new id is admitted; and
/// that a second forget, and one of a host the registry never held, change nothing. Catches a
/// forget of a live host, a forgotten host that still counts toward the cap or keeps its
/// binding, and a repeat that tells the placer again.
#[test]
fn a_lost_host_is_forgotten_and_its_place_in_the_pool_is_free_again() {
    let gone = Arc::new(Gone::default());
    let fx = with_placer(config(1), gone.clone());
    fx.join("old", "t");
    let old = host("old");
    let full = fx.registry.hello(&hello("new"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);

    assert!(!fx.registry.forget_host(&old), "a live host was forgotten");
    assert_eq!(fx.registry.hosts().len(), 1);
    fx.pass_grace();
    assert!(fx.registry.forget_host(&old), "a lost host was kept");
    assert!(fx.registry.hosts().is_empty(), "the host is still listed");
    assert!(!fx.registry.bindings().contains_key(&old), "its binding");
    assert_eq!(*gone.hosts.lock().unwrap(), ["old"]);

    assert_eq!(fx.join("new", "t"), 1, "the new id at the cap");
    assert!(!fx.registry.forget_host(&old), "a repeat forgot");
    assert!(!fx.registry.forget_host(&host("never")), "an unknown host");
    assert_eq!(*gone.hosts.lock().unwrap(), ["old"], "told again");
}

/// Asserts that a host put back after a restart and not heard since (awaiting, which counts
/// toward the cap) is forgotten, with its binding, and that a new id then joins the full pool.
/// Catches a forget that keeps every host it has not heard from, which is the host a server
/// restart puts back from the stored bindings.
#[test]
fn an_awaiting_host_is_forgotten() {
    let fx = fixture(1);
    let binding = Binding::new(Credential::of_token("t"), None);
    let taint = js_agent_wire::Taint {
        reason: "residue".into(),
        run_id: None,
        check: None,
    };
    fx.registry
        .restore_tainted(&host("old"), binding.with_taint(Some(taint)));
    let full = fx.registry.hello(&hello("new"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);
    assert!(fx.registry.forget_host(&host("old")));
    assert!(fx.registry.bindings().is_empty());
    assert_eq!(fx.join("new", "t"), 1);
}
