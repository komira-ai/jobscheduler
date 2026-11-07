//! The cache's rules with explicit instants as the clock: when a refresh is due, what a
//! read gives before and after a fetch, what a failure keeps, what an invalidation does,
//! and how cool-downs are keyed, extended and forgotten.

use std::time::Duration;

use js_types::{ExecMode, ProviderId};

use super::*;
use crate::offer::PriceUnit;

const HOUR: Duration = Duration::from_secs(3600);
const MS: Duration = Duration::from_millis(1);

fn at(millis: u64) -> Instant {
    Instant::from_millis(millis)
}

fn offer(id: &str, zone: Option<&str>) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: zone.map(str::to_owned),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 1 << 30,
        accelerator: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(100),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

fn failure() -> Result<Vec<Offer>, ProviderError> {
    Err(ProviderError::new("the cloud did not answer"))
}

/// Asserts that a refresh is due before any fetch; that after a fetch at t0 none is due at
/// t0 + interval - 1 ms, when a read gives the same offers, not stale, with their age; and
/// that one is due at exactly t0 + interval, when a second fetch replaces the offers with
/// the new list and the age starts again. Catches an interval that is ignored (a fetch at
/// every tick, or a catalog never fetched again) and one off by a millisecond at the
/// boundary.
#[test]
fn the_cache_refreshes_on_the_interval() {
    let t0 = 5_000;
    let mut cache = CatalogCache::new(HOUR);
    assert!(cache.refresh_due(at(t0)));
    let first = vec![offer("a", Some("z1"))];
    assert_eq!(cache.record(at(t0), Ok(first.clone())), Recorded::Fetched);
    let before = at(t0).saturating_add(HOUR - MS);
    assert!(!cache.refresh_due(before));
    assert_eq!(
        cache.read(before),
        Catalog::Offers {
            offers: &first,
            age: HOUR - MS,
            stale: false,
        }
    );
    let due = at(t0).saturating_add(HOUR);
    assert!(cache.refresh_due(due));
    assert_eq!(
        cache.read(due),
        Catalog::Offers {
            offers: &first,
            age: HOUR,
            stale: true,
        }
    );
    let second = vec![offer("b", Some("z2")), offer("c", None)];
    assert_eq!(cache.record(due, Ok(second.clone())), Recorded::Fetched);
    assert!(!cache.refresh_due(due));
    assert_eq!(
        cache.read(due),
        Catalog::Offers {
            offers: &second,
            age: Duration::ZERO,
            stale: false,
        }
    );
}

/// Asserts that once a good catalog is held, a failed fetch keeps it readable (not empty,
/// not never fetched), sets the last error, and is the first failure of the outage; that
/// the refresh stays due at the next tick (the failure is asked again then, not after
/// another interval); that the second failure in a row is not the first; and that a later
/// success clears the error, replaces the catalog and makes the next failure a first one
/// again. Catches a failure that empties or replaces the catalog, one reported at every
/// tick, and a retry that waits a whole interval.
#[test]
fn a_failed_refresh_keeps_the_last_good_catalog() {
    let mut cache = CatalogCache::new(HOUR);
    let good = vec![offer("a", Some("z1"))];
    cache.record(at(0), Ok(good.clone()));
    let due = at(0).saturating_add(HOUR);
    assert_eq!(
        cache.record(due, failure()),
        Recorded::Failed { first: true }
    );
    assert_eq!(cache.last_error(), Some("the cloud did not answer"));
    assert_eq!(
        cache.read(due),
        Catalog::Offers {
            offers: &good,
            age: HOUR,
            stale: true,
        }
    );
    let next_tick = due.saturating_add(Duration::from_secs(1));
    assert!(cache.refresh_due(next_tick));
    assert_eq!(
        cache.record(next_tick, failure()),
        Recorded::Failed { first: false }
    );
    assert_eq!(
        cache.read(next_tick),
        Catalog::Offers {
            offers: &good,
            age: HOUR + Duration::from_secs(1),
            stale: true,
        }
    );
    let fresh = vec![offer("b", None)];
    assert_eq!(
        cache.record(next_tick, Ok(fresh.clone())),
        Recorded::Fetched
    );
    assert_eq!(cache.last_error(), None);
    assert_eq!(
        cache.read(next_tick),
        Catalog::Offers {
            offers: &fresh,
            age: Duration::ZERO,
            stale: false,
        }
    );
    assert!(!cache.refresh_due(next_tick));
    assert_eq!(
        cache.record(next_tick, failure()),
        Recorded::Failed { first: true }
    );
}

/// Asserts that before any fetch the cache reads as never fetched, with no error; that a
/// first fetch that fails leaves it so (an error is not an empty catalog), with the error
/// kept and the fetch still due; and that a second failure is not the first. Catches a
/// failure read as an empty catalog, which would refuse work admission must keep.
#[test]
fn a_never_fetched_catalog_is_may_offer() {
    let mut cache = CatalogCache::new(HOUR);
    assert_eq!(cache.read(at(0)), Catalog::Never);
    assert_eq!(cache.last_error(), None);
    assert_eq!(
        cache.record(at(0), failure()),
        Recorded::Failed { first: true }
    );
    assert_eq!(cache.read(at(1)), Catalog::Never);
    assert_eq!(cache.last_error(), Some("the cloud did not answer"));
    assert!(cache.refresh_due(at(1)));
    assert_eq!(
        cache.record(at(1), failure()),
        Recorded::Failed { first: false }
    );
    assert_eq!(cache.read(at(2)), Catalog::Never);
}

/// Asserts that a catalog that was read and holds no offer reads as an empty list of
/// offers, which is not never fetched. Catches an empty answer folded into "never fetched",
/// which would admit work no provider could ever serve.
#[test]
fn an_empty_catalog_that_was_read_is_not_never() {
    let mut cache = CatalogCache::new(HOUR);
    cache.record(at(0), Ok(Vec::new()));
    assert_eq!(
        cache.read(at(0)),
        Catalog::Offers {
            offers: &[],
            age: Duration::ZERO,
            stale: false,
        }
    );
    assert_ne!(cache.read(at(0)), Catalog::Never);
}

/// Asserts that an invalidation makes a refresh due before the interval has passed while
/// the offers stay readable, marked stale; that it does not fetch or forget anything by
/// itself; and that the next fetch clears it. A failed fetch after it leaves the
/// invalidation in place. Catches an invalidation that drops the offers (which would refuse
/// work a refresh a moment later admits) and one that outlives the fetch it asked for.
#[test]
fn an_invalidation_makes_a_refresh_due_and_keeps_the_offers() {
    let mut cache = CatalogCache::new(HOUR);
    let held = vec![offer("a", Some("z1"))];
    cache.record(at(0), Ok(held.clone()));
    cache.invalidate();
    assert!(cache.refresh_due(at(1)));
    assert_eq!(
        cache.read(at(1)),
        Catalog::Offers {
            offers: &held,
            age: MS,
            stale: true,
        }
    );
    cache.record(at(2), failure());
    assert!(cache.refresh_due(at(3)));
    cache.record(at(3), Ok(held.clone()));
    assert!(!cache.refresh_due(at(4)));
    assert_eq!(
        cache.read(at(4)),
        Catalog::Offers {
            offers: &held,
            age: MS,
            stale: false,
        }
    );
}

/// Asserts that an invalidation of a cache that has never fetched leaves it never fetched
/// and due. Catches an invalidation that makes up an empty catalog.
#[test]
fn an_invalidation_before_any_fetch_reads_as_never() {
    let mut cache = CatalogCache::new(HOUR);
    cache.invalidate();
    assert_eq!(cache.read(at(0)), Catalog::Never);
    assert!(cache.refresh_due(at(0)));
}

/// Asserts the arithmetic of an instant: it keeps its milliseconds, adds a duration,
/// saturates at the last instant for a sum past it (a duration with more milliseconds than
/// an instant holds included), orders by time, and gives an age of nothing for a read
/// made before the fetch it reads. Catches an overflow that wraps a far end around to the
/// past, which would end a cool-down at once.
#[test]
fn an_instant_adds_and_saturates() {
    assert_eq!(at(7).millis(), 7);
    assert_eq!(at(7).saturating_add(Duration::from_millis(3)), at(10));
    assert_eq!(at(u64::MAX - 1).saturating_add(HOUR), at(u64::MAX));
    assert_eq!(at(0).saturating_add(Duration::MAX), at(u64::MAX));
    assert!(at(1) < at(2));
    let mut cache = CatalogCache::new(HOUR);
    cache.record(at(10), Ok(Vec::new()));
    assert_eq!(
        cache.read(at(5)),
        Catalog::Offers {
            offers: &[],
            age: Duration::ZERO,
            stale: false,
        }
    );
}

fn key(provider: &'static str, offer_id: &str, zone: Option<&str>) -> OfferKey {
    OfferKey::of(&ProviderId::known(provider), &offer(offer_id, zone))
}

/// Asserts that a key cooled down at t0 for an hour is cooling down at t0 and at
/// end - 1 ms, and not at the end instant nor after. Catches a cool-down that never ends
/// and one that ends a millisecond early or late.
#[test]
fn a_cooled_down_offer_is_not_picked_until_the_cooldown_ends() {
    let mut cooldowns = Cooldowns::new();
    let t0 = at(1_000);
    let end = t0.saturating_add(HOUR);
    let a10 = key("cloud", "gpu_1x_a10", Some("z1"));
    assert!(!cooldowns.is_cooled_down(&a10, t0));
    cooldowns.cool_down(a10.clone(), t0, HOUR);
    assert!(cooldowns.is_cooled_down(&a10, t0));
    assert!(cooldowns.is_cooled_down(&a10, at(end.millis() - 1)));
    assert!(!cooldowns.is_cooled_down(&a10, end));
    assert!(!cooldowns.is_cooled_down(&a10, end.saturating_add(HOUR)));
}

/// Asserts that a cool-down covers its provider, instance type and zone only: the same
/// type in another zone, the same type with no zone, the same type and zone of another
/// provider, and another type in the same zone are each not cooling down. Catches a key
/// that leaves out the zone (one full zone would stop a type everywhere) or the provider.
#[test]
fn a_cooldown_is_keyed_by_provider_instance_type_and_zone() {
    let mut cooldowns = Cooldowns::new();
    let cooled = key("cloud", "gpu_1x_a10", Some("z1"));
    cooldowns.cool_down(cooled.clone(), at(0), HOUR);
    assert_eq!(
        cooled,
        OfferKey {
            provider: ProviderId::known("cloud"),
            offer: "gpu_1x_a10".to_owned(),
            zone: Some("z1".to_owned()),
        }
    );
    assert!(cooldowns.is_cooled_down(&cooled, at(1)));
    for other in [
        key("cloud", "gpu_1x_a10", Some("z2")),
        key("cloud", "gpu_1x_a10", None),
        key("other-cloud", "gpu_1x_a10", Some("z1")),
        key("cloud", "gpu_8x_a100", Some("z1")),
    ] {
        assert!(!cooldowns.is_cooled_down(&other, at(1)), "{other:?}");
    }
}

/// Asserts that cooling a key down again extends it to the later end, and that a second,
/// shorter cool-down does not bring the first one's end forward. Catches a second
/// cool-down that overwrites the first.
#[test]
fn a_second_cooldown_never_shortens_the_first() {
    let mut cooldowns = Cooldowns::new();
    let a10 = key("cloud", "gpu_1x_a10", Some("z1"));
    cooldowns.cool_down(a10.clone(), at(0), HOUR);
    cooldowns.cool_down(a10.clone(), at(10), MS);
    let first_end = at(0).saturating_add(HOUR);
    assert!(cooldowns.is_cooled_down(&a10, at(first_end.millis() - 1)));
    assert!(!cooldowns.is_cooled_down(&a10, first_end));
    cooldowns.cool_down(a10.clone(), at(100), HOUR);
    let later_end = at(100).saturating_add(HOUR);
    assert!(cooldowns.is_cooled_down(&a10, first_end));
    assert!(cooldowns.is_cooled_down(&a10, at(later_end.millis() - 1)));
    assert!(!cooldowns.is_cooled_down(&a10, later_end));
    assert_eq!(cooldowns.len(), 1);
}

/// Asserts that forgetting at an instant drops the cool-downs that have ended by then (one
/// ending exactly then included) and keeps the one still running, which still cools its
/// key down. Catches a forget that drops a running cool-down or keeps every one.
#[test]
fn forget_expired_drops_only_ended_cooldowns() {
    let mut cooldowns = Cooldowns::new();
    assert!(cooldowns.is_empty());
    let short = key("cloud", "a", Some("z1"));
    let exact = key("cloud", "b", Some("z1"));
    let long = key("cloud", "c", Some("z1"));
    cooldowns.cool_down(short.clone(), at(0), Duration::from_millis(5));
    cooldowns.cool_down(exact.clone(), at(0), Duration::from_millis(10));
    cooldowns.cool_down(long.clone(), at(0), Duration::from_millis(11));
    assert_eq!(cooldowns.len(), 3);
    cooldowns.forget_expired(at(10));
    assert_eq!(cooldowns.len(), 1);
    assert!(!cooldowns.is_empty());
    assert!(cooldowns.is_cooled_down(&long, at(10)));
    assert!(!cooldowns.is_cooled_down(&short, at(10)));
    assert!(!cooldowns.is_cooled_down(&exact, at(10)));
}

/// Asserts that a cool-down held until an end written down earlier cools its key down to
/// that end and not past it, and that holding a key again keeps the later end whichever
/// comes first, as a fresh cool-down does. Catches an end read back after a restart that
/// is ignored, moved or shortened.
#[test]
fn a_held_cooldown_lasts_to_its_end_and_is_never_shortened() {
    let mut cooldowns = Cooldowns::new();
    let a10 = key("cloud", "gpu_1x_a10", Some("z1"));
    cooldowns.hold_until(a10.clone(), at(100));
    assert!(cooldowns.is_cooled_down(&a10, at(0)));
    assert!(cooldowns.is_cooled_down(&a10, at(99)));
    assert!(!cooldowns.is_cooled_down(&a10, at(100)));
    cooldowns.hold_until(a10.clone(), at(50));
    assert!(cooldowns.is_cooled_down(&a10, at(99)));
    cooldowns.hold_until(a10.clone(), at(200));
    assert!(cooldowns.is_cooled_down(&a10, at(199)));
    assert!(!cooldowns.is_cooled_down(&a10, at(200)));
    cooldowns.cool_down(a10.clone(), at(0), Duration::from_millis(10));
    assert!(cooldowns.is_cooled_down(&a10, at(199)));
    assert_eq!(cooldowns.len(), 1);
}

/// Asserts that merging takes in every cool-down of the other store to its end, keeps the
/// later end of a key both hold whichever side has it, and leaves the other store as it
/// was. Catches a merge that drops a key, or that lets the earlier end win.
#[test]
fn a_merge_keeps_every_key_and_the_later_end() {
    let (a10, h100, a100) = (
        key("cloud", "gpu_1x_a10", Some("z1")),
        key("cloud", "gpu_8x_h100", Some("z1")),
        key("cloud", "gpu_8x_a100", Some("z1")),
    );
    let mut held = Cooldowns::new();
    held.hold_until(a10.clone(), at(100));
    held.hold_until(h100.clone(), at(10));
    let mut read = Cooldowns::new();
    read.hold_until(a10.clone(), at(50));
    read.hold_until(h100.clone(), at(70));
    read.hold_until(a100.clone(), at(30));
    held.merge(&read);
    assert_eq!(held.len(), 3);
    assert!(held.is_cooled_down(&a10, at(99)));
    assert!(held.is_cooled_down(&h100, at(69)));
    assert!(!held.is_cooled_down(&h100, at(70)));
    assert!(held.is_cooled_down(&a100, at(29)));
    assert!(!held.is_cooled_down(&a100, at(30)));
    assert_eq!(read.len(), 3);
    assert!(!read.is_cooled_down(&a10, at(50)));
}
