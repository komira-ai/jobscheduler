//! Verify on hello through the scaler's step (design 5.6.1, step 6): what the first hello
//! of a booting machine does to its record, its offer and its pool's brake, on a match, a
//! mismatch, a record with no offer, a host whose shape is not known, and a host the scaler
//! did not launch. Every step is given its time; nothing reads a clock.

mod common;

use std::time::Duration;

use common::*;
use js_elastic::{Instant, Offer, OfferKey};
use js_scaler::{
    Action, CooldownReason, HelloDimension, HostFacts, OfferCooldown, Phase, Records, Settings,
    State, StoredOffer,
};
use js_types::ProviderId;

/// The provider the pool launches with in these tests.
const PROVIDER: &str = "cloud";

/// Live settings of the pool `gpu` whose machines come from [`PROVIDER`], with offers cooled
/// down for two hours after a mismatch.
fn verifying() -> Settings {
    let mut settings = settings("gpu");
    settings.provider = ProviderId::new(PROVIDER).unwrap();
    settings.offer_cooldown = Duration::from_secs(2 * 3600);
    settings
}

/// An offer of eight accelerators spelt by the alias `nvidia-h100-80gb-hbm3`, in zone z1.
fn h100_offer() -> Offer {
    Offer {
        accelerator: accel("nvidia-h100-80gb-hbm3", 8),
        ..offer("gpu_8x_h100", 60)
    }
}

/// A rig of the pool `gpu` holding one machine, booting, acquired for `offer` (as stored),
/// whose provider id is known. Returns the rig and the machine's name.
fn booting_for(offer: StoredOffer) -> (Rig, String) {
    let mut rig = Rig::new(verifying());
    let mut machine = record(Phase::Booting, 60, 0);
    machine.offer = offer;
    let name = machine.name.clone();
    rig.state.records.instances = vec![machine];
    (rig, name)
}

/// The hello of `name`, live, announcing `shape`.
fn hello(name: &str, shape: Offer) -> HostFacts {
    HostFacts {
        shape: Some(Offer {
            price_cents_per_hour: None,
            ..shape
        }),
        ..host(name, "gpu")
    }
}

/// The key of the offer `id` of [`PROVIDER`] in `zone`.
fn key(id: &str, zone: &str) -> OfferKey {
    OfferKey {
        provider: ProviderId::new(PROVIDER).unwrap(),
        offer: id.to_owned(),
        zone: Some(zone.to_owned()),
    }
}

fn instant(millis: u64) -> Instant {
    Instant::from_millis(at(millis).as_millis())
}

/// How many failures in a row the brake of `gpu` holds.
fn failures(rig: &Rig) -> u32 {
    rig.state
        .records
        .brake
        .get("gpu")
        .map_or(0, |brake| brake.failures)
}

/// Asserts that a hello reporting the canonical model of an offer named by an alias, the
/// same count and the same sizes registers the machine: the record is `Registered` and not
/// condemned, nothing is revoked or released, no offer is cooled down, the pool gets no
/// cooldown and its brake's count is unchanged. Catches a match treated as a mismatch, and a
/// matching host that is not registered.
#[test]
fn a_matching_hello_registers_the_host() {
    let offer = h100_offer();
    let (mut rig, name) = booting_for(StoredOffer::from_offer(&offer));
    rig.state.records.brake.insert(
        "gpu".to_owned(),
        js_scaler::Brake {
            failures: 1,
            tripped_at: None,
        },
    );
    let reported = hello(&name, offer.clone());
    let model = &reported
        .shape
        .as_ref()
        .unwrap()
        .accelerator
        .as_ref()
        .unwrap()
        .model;
    assert_eq!(model.as_str(), "nvidia-h100");
    rig.input.hosts = vec![reported];
    let actions = rig.tick(at(SECOND));
    assert_eq!(kinds(&actions), ["Save"]);
    assert_eq!(rig.phase(0), Phase::Registered);
    assert!(rig.record(0).registered);
    assert!(!rig.record(0).condemned);
    assert!(rig.state.records.cooled_offers.is_empty());
    assert!(rig.state.records.cooldowns.is_empty());
    assert_eq!(failures(&rig), 1);
    assert!(rig.state.mismatched.is_empty());
    assert!(rig.state.unverified.is_empty());
}

/// Asserts the three effects of a mismatched hello, in the one step that judged it: the
/// record is condemned and `Releasing` (its token revoked and the machine's release asked
/// in that step, after the save that the shell follows by marking the host ending); the
/// offer is cooled down, keyed by provider, instance type and zone, until the step's time
/// plus `offer_cooldown`, with the dimension that failed, and the same instance type in
/// another zone is not; and the pool's brake counts exactly one failure and its cooldown
/// starts. At the step after it, with a request waiting, the host is taken back for nothing
/// (no undrain), nothing is launched while the pool cools down, and no second failure is
/// counted. Catches a missing effect, a key without the zone, a host left bookable, and a
/// mismatch counted at every step that sees it.
#[test]
fn a_mismatch_releases_cools_down_and_counts_toward_the_brake() {
    let offer = h100_offer();
    let (mut rig, name) = booting_for(StoredOffer::from_offer(&offer));
    let wrong = Offer {
        accelerator: accel("nvidia-a100-sxm4-80gb", 8),
        ..offer.clone()
    };
    rig.input.hosts = vec![hello(&name, wrong)];
    let actions = rig.tick(at(SECOND));
    assert_eq!(kinds(&actions), ["Save", "Revoke", "Release"]);
    let Action::Save(saved) = &actions[0] else {
        panic!("the first action is the save");
    };
    assert_eq!(saved.instances[0].phase, Phase::Releasing);
    assert_eq!(actions[1], Action::Revoke(name.clone()));
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert!(rig.record(0).condemned);
    assert!(rig.record(0).registered);

    let until = at(SECOND).plus(Duration::from_secs(2 * 3600));
    let cooled = OfferCooldown {
        provider: PROVIDER.to_owned(),
        offer: "gpu_8x_h100".to_owned(),
        zone: Some("z1".to_owned()),
        until,
        pool: "gpu".to_owned(),
        host: name.clone(),
        failed: vec![HelloDimension::AcceleratorModel],
    };
    assert_eq!(rig.state.records.cooled_offers, [cooled.clone()]);
    assert_eq!(saved.cooled_offers, [cooled.clone()]);
    assert_eq!(rig.state.mismatched, [cooled]);
    let store = rig.state.records.offer_cooldowns();
    assert!(store.is_cooled_down(&key("gpu_8x_h100", "z1"), instant(SECOND)));
    assert!(!store.is_cooled_down(&key("gpu_8x_h100", "z2"), instant(SECOND)));

    assert_eq!(failures(&rig), 1);
    let pool = rig.state.records.cooldowns["gpu"];
    assert_eq!(pool.reason, CooldownReason::BootFailed);
    assert_eq!(pool.until, at(SECOND + 10 * MINUTE));

    rig.input.waiting = vec![request("r1")];
    let next = rig.tick(at(2 * SECOND));
    assert_eq!(count(&next, "Undrain"), 0);
    assert_eq!(count(&next, "Acquire"), 0);
    assert_eq!(rig.phase(0), Phase::Releasing);
    assert_eq!(failures(&rig), 1);
}

/// Asserts that the offer cool-down outlives a restart: the records written at the
/// mismatch, read back, hold it, and a scaler started from them keeps it until its end: a
/// step at `offer_cooldown` less a millisecond still holds it (and the store reads it as
/// cooling down), and the step at `offer_cooldown` drops it and writes the records. Catches
/// a cool-down kept only in memory, and an end that is off by one.
#[test]
fn the_cooldown_survives_a_restart_and_expires_on_the_clock() {
    let offer = h100_offer();
    let (mut rig, name) = booting_for(StoredOffer::from_offer(&offer));
    let short = Offer {
        memory_bytes: 1 << 30,
        ..offer.clone()
    };
    rig.input.hosts = vec![hello(&name, short)];
    rig.tick(at(0));
    let text = rig.state.records.encode().unwrap();
    assert!(text.contains("\"cooled_offers\""), "{text}");
    let read = Records::decode(&text).unwrap();
    assert_eq!(read, rig.state.records);
    assert_eq!(read.cooled_offers[0].failed, [HelloDimension::Memory]);

    let end = 2 * 3600 * SECOND;
    let store = read.offer_cooldowns();
    let cooled = key("gpu_8x_h100", "z1");
    assert!(store.is_cooled_down(&cooled, instant(end - 1)));
    assert!(!store.is_cooled_down(&cooled, instant(end)));

    let mut restarted = Rig::new(verifying());
    restarted.state = State::new(read);
    restarted.input.hosts = Vec::new();
    restarted.tick(at(end - 1));
    assert_eq!(restarted.state.records.cooled_offers.len(), 1);
    let store = restarted.state.records.offer_cooldowns();
    assert!(store.is_cooled_down(&cooled, instant(end - 1)));
    let actions = restarted.tick(at(end));
    assert!(restarted.state.records.cooled_offers.is_empty());
    let Some(Action::Save(saved)) = actions.first() else {
        panic!("the expiry is written: {:?}", kinds(&actions));
    };
    assert!(saved.cooled_offers.is_empty());
}

/// Asserts the two cases that judge nothing, kept apart. A booting record made by
/// `StoredOffer::nothing` (the offer record was lost) whose hello is far below the offer
/// the machine was launched for (it got past the registry's shape floor) is registered, not
/// condemned, cools nothing down, counts no failure, and is named as unverified for the
/// shell to log. A booting record with a recorded offer whose host's shape is not known
/// since a restart is registered and not condemned either, and is not named unverified: its
/// offer record is not lost. Catches a missing record read as "offers nothing" and the hello
/// judged against it, and the reverse.
#[test]
fn a_hello_with_no_offer_record_is_checked_against_the_shape_floor_only() {
    let lost = StoredOffer::nothing("gpu_8x_h100", Some("z1".to_owned()));
    assert!(!lost.is_recorded());
    assert!(StoredOffer::from_offer(&h100_offer()).is_recorded());
    let (mut rig, name) = booting_for(lost);
    let floor_only = Offer {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        accelerator: accel("nvidia-a10", 1),
        ..h100_offer()
    };
    rig.input.hosts = vec![hello(&name, floor_only)];
    let actions = rig.tick(at(SECOND));
    assert_eq!(kinds(&actions), ["Save"]);
    assert_eq!(rig.phase(0), Phase::Registered);
    assert!(!rig.record(0).condemned);
    assert!(rig.state.records.cooled_offers.is_empty());
    assert!(rig.state.records.cooldowns.is_empty());
    assert_eq!(failures(&rig), 0);
    assert_eq!(rig.state.unverified, [name]);

    let (mut unknown, name) = booting_for(StoredOffer::from_offer(&h100_offer()));
    let mut silent = hello(&name, h100_offer());
    silent.shape = None;
    unknown.input.hosts = vec![silent];
    unknown.tick(at(SECOND));
    assert_eq!(unknown.phase(0), Phase::Registered);
    assert!(!unknown.record(0).condemned);
    assert!(unknown.state.records.cooled_offers.is_empty());
    assert!(unknown.state.unverified.is_empty());
    assert_eq!(failures(&unknown), 0);
}

/// Asserts that a host the scaler did not launch (a static host of the pool, joined with the
/// pool's join token) whose shape matches no offer of the pool is untouched: no drain,
/// revoke or release, no offer cooled down, no failure counted. Catches a check that judges
/// every host of the pool rather than the machines the scaler acquired.
#[test]
fn a_hello_of_a_host_the_scaler_did_not_launch_is_untouched() {
    let mut rig = Rig::new(verifying());
    let stranger = Offer {
        cpu_millis: 1,
        accelerator: None,
        ..h100_offer()
    };
    rig.input.hosts = vec![hello("static-1", stranger)];
    let actions = rig.tick(at(SECOND));
    for kind in ["Drain", "ReleaseHost", "Revoke", "Release", "Retire"] {
        assert_eq!(count(&actions, kind), 0, "{kind}");
    }
    assert!(rig.state.records.instances.is_empty());
    assert!(rig.state.records.cooled_offers.is_empty());
    assert_eq!(failures(&rig), 0);
}

/// Asserts that a second mismatch of an offer already cooling down keeps one entry for the
/// key, with the later end and the newer host and dimensions, and that a mismatch of another
/// offer adds its own. Catches a second cool-down that shortens the first, and entries
/// duplicated per machine.
#[test]
fn a_second_mismatch_of_a_cooled_offer_extends_its_one_entry() {
    let offer = h100_offer();
    let (mut rig, first) = booting_for(StoredOffer::from_offer(&offer));
    let mut again = record(Phase::Booting, 60, MINUTE);
    again.offer = StoredOffer::from_offer(&offer);
    let second = again.name.clone();
    let mut other = record(Phase::Booting, 60, 2 * MINUTE);
    other.offer = StoredOffer::from_offer(&Offer {
        zone: Some("z2".to_owned()),
        ..offer.clone()
    });
    let third = other.name.clone();
    rig.state.records.instances.push(again);
    rig.state.records.instances.push(other);
    let wrong = Offer {
        cpu_millis: 1,
        ..offer.clone()
    };
    rig.input.hosts = vec![hello(&first, wrong.clone())];
    rig.tick(at(0));
    rig.input.hosts = vec![
        hello(&first, wrong.clone()),
        hello(
            &second,
            Offer {
                accelerator: accel("nvidia-h100-80gb-hbm3", 4),
                ..offer.clone()
            },
        ),
    ];
    rig.tick(at(MINUTE));
    let cooled = &rig.state.records.cooled_offers;
    assert_eq!(cooled.len(), 1);
    assert_eq!(
        cooled[0].until,
        at(MINUTE).plus(Duration::from_secs(2 * 3600))
    );
    assert_eq!(cooled[0].host, second);
    assert_eq!(cooled[0].failed, [HelloDimension::AcceleratorCount]);

    rig.input.hosts = vec![hello(&third, wrong)];
    rig.tick(at(2 * MINUTE));
    let zones: Vec<Option<&str>> = rig
        .state
        .records
        .cooled_offers
        .iter()
        .map(|each| each.zone.as_deref())
        .collect();
    assert_eq!(zones, [Some("z1"), Some("z2")]);
}
