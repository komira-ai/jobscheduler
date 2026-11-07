//! Verify on hello through the scaler's shell over a fake world (design 5.6.1, step 6): a
//! machine whose hello does not match the offer it was acquired for is marked ending after
//! the records are written, released, logged and alerted on; a catalog row whose alias names
//! a model the machine never has cannot loop acquire, release, acquire past the pool's
//! brake; and a hello of a machine with no offer record is logged. Time is an injected
//! clock; nothing sleeps.

use std::collections::BTreeMap;
use std::time::Duration;

use js_elastic::Offer;
use js_jobs::Needs;
use js_registry::HostView;
use js_scaler::{Blocked, HelloDimension, Phase, Records, StoredOffer};
use js_types::{Model, Resources, Shape, accelerator_of};

use crate::fakes::{Rig, job_with, live, logged, offer, record, records, rig, spec_of, view};

/// The catalog row: one accelerator under the alias `nvidia-a100-sxm4-80gb`, which the alias
/// table resolves to the A100 family pinned to its 80 GB variant.
fn alias_offer() -> Offer {
    Offer {
        id: "gpu_1x_a100_sxm4".to_owned(),
        accelerator: Some(accelerator_of(
            &Model::parse("nvidia-a100-sxm4-80gb").unwrap(),
            1,
            None,
        )),
        ..offer()
    }
}

/// A rig whose provider offers only [`alias_offer`] and whose one queued job, `r1`, asks
/// for one A100 of any variant: only that offer fits it.
fn wrong_alias_rig() -> Rig {
    let rig = rig(Records::default(), live());
    let a100 = Resources::new(1000, 1 << 30, 1, Some("nvidia-a100".to_owned())).unwrap();
    rig.world().offers = vec![alias_offer()];
    rig.world().queued = vec![job_with("r1", spec_of(Needs::Shape(a100)))];
    rig
}

/// The hello of `name` from a machine of the size of the offer whose one accelerator is the
/// 40 GB A100, the model the provider actually delivers for the row.
fn delivered(name: &str) -> HostView {
    let mut host = view(name);
    let claim = Resources::new(8000, 64 << 30, 1, Some("nvidia-a100-sxm4-40gb".to_owned()));
    host.claim = Some(Shape::new(claim.unwrap(), BTreeMap::new()));
    host
}

/// Has each machine the provider holds and the registry does not show yet say hello, as
/// [`delivered`]. Returns the names that did.
fn hellos(rig: &Rig) -> Vec<String> {
    let mut world = rig.world();
    let new: Vec<String> = world
        .machines
        .iter()
        .filter_map(|held| held.name.clone())
        .filter(|name| world.hosts.iter().all(|host| host.id.as_str() != name))
        .collect();
    for name in &new {
        world.hosts.push(delivered(name));
    }
    new
}

/// How many acquires the fake provider was asked for.
fn acquires(rig: &Rig) -> usize {
    rig.log()
        .iter()
        .filter(|entry| entry.starts_with("acquire:"))
        .count()
}

/// Asserts that a catalog row whose alias names a model the machine never has stops after
/// the brake's trip: a demand only that offer fits, a live pool, and a fake clock stepped an
/// hour at a time through eight rounds of launch and hello. Exactly `boot_fail_trip_after`
/// (3) machines are acquired, each mismatch cools the offer down (provider, instance type
/// and zone) and counts a failed boot, the pool then reports the brake as what blocks it,
/// and no further acquire is made until the brake is reset, after which one is. Catches a
/// loop that goes on past the trip, an offer cool-down that is skipped, and a mismatch that
/// does not count toward the trip.
#[tokio::test]
async fn a_wrong_alias_cannot_loop_past_the_brake() {
    let rig = wrong_alias_rig();
    let mut judged = Vec::new();
    for _ in 0..8 {
        rig.scaler.tick().await;
        let said = hellos(&rig);
        rig.scaler.tick().await;
        let state = rig.scaler.state();
        for name in said {
            let held = state
                .records
                .instances
                .iter()
                .find(|each| each.name == name);
            let held = held.expect("the machine has a record");
            assert_eq!(held.phase, Phase::Releasing, "{name}");
            assert!(held.condemned, "{name}");
            let cooled = &state.records.cooled_offers;
            assert_eq!(cooled.len(), 1, "{cooled:?}");
            assert_eq!(cooled[0].offer, "gpu_1x_a100_sxm4");
            assert_eq!(cooled[0].zone.as_deref(), Some("z1"));
            assert_eq!(cooled[0].host, name);
            assert_eq!(cooled[0].failed, [HelloDimension::AcceleratorModel]);
            judged.push(state.records.brake["gpu"].failures);
        }
        rig.advance(Duration::from_secs(3600));
    }
    assert_eq!(acquires(&rig), 3, "{:?}", rig.log());
    assert_eq!(judged, [1, 2, 3]);
    let state = rig.scaler.state();
    assert!(state.records.tripped("gpu"));
    assert_eq!(state.blocked["gpu"], Blocked::Tripped);
    assert!(!rig.log().iter().any(|entry| entry.starts_with("UNSAFE")));

    assert!(rig.scaler.reset_brake("gpu"));
    rig.scaler.tick().await;
    assert_eq!(acquires(&rig), 4, "{:?}", rig.log());
}

/// Asserts what the shell does with one mismatched hello: the records that release the
/// machine and cool its offer down are written first, the registry is then told the host is
/// ending (so nothing more is booked on it) before the machine's release is asked; the log
/// names the host, the offer and the dimension that failed; an alert names the offer while
/// its cool-down holds; and a scaler started from the written records keeps the cool-down
/// and the alert. Catches a release asked before the records are written, a host left
/// bookable, and a cool-down a restart loses.
#[tokio::test]
async fn a_mismatched_hello_is_marked_ending_after_the_save_and_alerted() {
    let rig = wrong_alias_rig();
    rig.scaler.tick().await;
    let name = hellos(&rig).remove(0);
    rig.world().log.clear();
    rig.scaler.tick().await;
    let log = rig.log();
    let at = |entry: String| log.iter().position(|each| *each == entry);
    let saved = at("save".to_owned()).expect("the records are written");
    let marked = at(format!("MarkEnding:{name}")).expect("the host is marked ending");
    let released = at(format!("release:{name}")).expect("the release is asked");
    assert!(saved < marked && marked < released, "{log:?}");
    assert_eq!(
        rig.world().hosts[0].mark,
        Some(js_registry::HostMark::Releasing)
    );

    let said = logged();
    let line = said
        .lines()
        .find(|line| line.contains("does not match the offer") && line.contains(&name));
    let line = line.expect("the mismatch is logged");
    assert!(line.contains("gpu_1x_a100_sxm4"), "{line}");
    assert!(line.contains("accelerator_model"), "{line}");
    let cooled_alert = |alerts: Vec<String>| {
        alerts
            .into_iter()
            .filter(|alert| alert.contains("is cooled down"))
            .collect::<Vec<_>>()
    };
    let alerts = cooled_alert(rig.scaler.alerts());
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains(&name), "{alerts:?}");

    let written = rig.world().saved.clone().expect("the records were written");
    assert_eq!(written.cooled_offers.len(), 1);
    let again = rig_from(written);
    again.scaler.tick().await;
    assert_eq!(again.scaler.state().records.cooled_offers.len(), 1);
    assert_eq!(cooled_alert(again.scaler.alerts()).len(), 1);
}

/// A scaler of the live pool that starts from `records`, with the clock where the first
/// rig's was when they were written.
fn rig_from(records: Records) -> Rig {
    let rig = rig(records, live());
    rig.advance(Duration::from_secs(1));
    rig
}

/// Asserts that the hello of a booting machine whose record holds no offer (made by
/// `StoredOffer::nothing`) registers it, condemns nothing, cools nothing down, and is logged
/// as a hello with no offer record, naming the host. Catches a lost record that is judged as
/// an offer of nothing, and one that passes unseen.
#[tokio::test]
async fn a_hello_with_no_offer_record_is_logged() {
    let mut lost = record("js-lost-offer", Phase::Booting);
    lost.offer = StoredOffer::nothing("gpu_1x_a10", Some("z1".to_owned()));
    let rig = rig(records(vec![lost]), live());
    rig.world().hosts = vec![view("js-lost-offer")];
    rig.scaler.tick().await;
    let state = rig.scaler.state();
    assert_eq!(state.records.instances[0].phase, Phase::Registered);
    assert!(!state.records.instances[0].condemned);
    assert!(state.records.cooled_offers.is_empty());
    assert!(state.records.cooldowns.is_empty());
    let said = logged();
    let line = said
        .lines()
        .find(|line| line.contains("no offer record") && line.contains("js-lost-offer"));
    assert!(line.is_some(), "{said}");
}
