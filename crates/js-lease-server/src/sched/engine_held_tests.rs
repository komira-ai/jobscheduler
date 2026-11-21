//! The registry's calls the server makes on its async workers while a pass over the queued
//! jobs holds the placement engine: a heartbeat, the check of one, the silence a read call
//! shows, the hosts admission reads and the tick's housekeeping all return without waiting
//! for the pass, and the heartbeat reaches the engine once the pass lets go. The test holds
//! the engine on its own thread, as a pass does, and makes the calls on the same thread: a
//! call that waited for the engine would never return. Everything runs in process on a
//! manual clock; no listener is bound.

use js_registry::{Credential, HostState};
use js_types::HostId;

use super::engine_rig::*;
use super::wiring_tests::JOIN;

/// Asserts that while a pass holds the engine, a heartbeat of `h1` carrying its second reading
/// (the first, alone, gives the engine no rate and leaves the fallback) is
/// accepted, its check passes, the read calls' silence of the host answers (none: it just
/// beat), the hosts list it live, and the tick's housekeeping runs; and that once the pass
/// lets go the engine charges the host from the two readings: a fresh view, not the fallback. Catches
/// the registry holding its lock while it waits for the engine, which parks every async
/// worker that serves a read call or a heartbeat for as long as the pass decides (the
/// connections then go unread, and a client's cancels pile up until the server ends the
/// connection with `ENHANCE_YOUR_CALM`).
#[test]
fn the_registry_answers_while_a_pass_holds_the_engine() {
    let r = rig("engine_held", &pool(1, 8000, ""), &["h1"], &[]);
    let h1 = HostId::new("h1").unwrap();
    r.beat("h1", Some(reading(1, 4_000, GIB)));
    assert!(r.placer().load_of("h1").unwrap().fallback);
    r.advance(5_000);
    let beat = r.heartbeat("h1", Some(reading(2, 4_000, GIB)));
    let credential = Credential::of_token(JOIN);
    let pass = r.placer().engine();
    let registry = &r.agents.registry;
    assert!(registry.heartbeat(&beat, &credential).is_ok());
    assert!(registry.check_heartbeat(&beat, &credential).is_ok());
    assert_eq!(registry.silent_since(&h1), None);
    let hosts = registry.hosts();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].state, HostState::Live);
    registry.advance();
    drop(pass);
    let charged = r.placer().load_of("h1").unwrap();
    assert!(!charged.fallback);
}
