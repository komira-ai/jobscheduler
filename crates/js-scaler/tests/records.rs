//! The records file: its literal form, its version, what it refuses, and the state
//! machine's table of transitions.

mod common;

use common::*;
use js_elastic::Offer;
use js_scaler::{Cooldown, CooldownReason, Phase, Record, Records, StoredExec, StoredOffer};
use js_types::ExecMode;

const GOLDEN: &str = include_str!("golden/records_v1.json");

/// The records the golden file holds: one launch with every optional field empty, and
/// an orphan with every one of them set, in the other execution mode.
fn sample() -> Records {
    let launching = record(Phase::Launching, 60, 0);
    let mut orphan = record(Phase::Orphan, 90, MINUTE);
    orphan.acquire = None;
    orphan.name = String::new();
    orphan.offer.exec = StoredExec::SandboxExec;
    orphan.token_expires = Some(at(2 * MINUTE));
    orphan.cloud_id = Some("i-stray".to_owned());
    orphan.retry_at = Some(at(4 * MINUTE));
    orphan.gone_at = Some(at(5 * MINUTE));
    orphan.registered = true;
    orphan.condemned = true;
    orphan.clean_lists = 2;
    orphan.last_clean = Some(at(3 * MINUTE));
    let mut records = Records {
        launches: 2,
        instances: vec![launching, orphan],
        ..Records::default()
    };
    records.cooldowns.insert(
        "gpu".to_owned(),
        Cooldown {
            until: at(10 * MINUTE),
            reason: CooldownReason::BootFailed,
        },
    );
    records
}

/// Asserts the literal text of the records file, version 1, for a launch and for an
/// orphan with every field set: names, order, the phase and reason spellings, the
/// execution mode spelling and the times as milliseconds. A renamed or reordered field,
/// or a changed spelling, changes the text.
#[test]
fn the_records_file_has_this_text() {
    assert_eq!(sample().encode().unwrap(), GOLDEN.trim_end());
}

/// Asserts that the golden text reads back as the records it was written from.
#[test]
fn the_golden_text_reads_back() {
    assert_eq!(Records::decode(GOLDEN).unwrap(), sample());
    assert_eq!(
        Records::decode(&Records::default().encode().unwrap()).unwrap(),
        Records::default()
    );
}

/// Asserts what is refused: text that is not JSON, a file of another version or of none,
/// a field the format does not have, a missing field, and an acquire id that is not one.
/// Each error says what is wrong.
#[test]
fn a_file_that_is_not_records_of_this_version_is_refused() {
    let broken = Records::decode("{not json").unwrap_err();
    assert!(broken.to_string().starts_with("records: "), "{broken}");

    let other = GOLDEN.replace("\"version\": 1", "\"version\": 2");
    let error = Records::decode(&other).unwrap_err();
    assert_eq!(
        error.to_string(),
        "records: version Some(2), this reader reads 1"
    );

    let none = Records::decode("{}").unwrap_err();
    assert_eq!(
        none.to_string(),
        "records: version None, this reader reads 1"
    );

    let extra = GOLDEN.replace("\"launches\": 2,", "\"launches\": 2, \"surprise\": 1,");
    let error = Records::decode(&extra).unwrap_err();
    assert!(error.to_string().contains("surprise"), "{error}");

    let missing = GOLDEN.replace("\"launches\": 2,", "");
    let error = Records::decode(&missing).unwrap_err();
    assert!(error.to_string().contains("launches"), "{error}");

    let bad_id = GOLDEN.replace("made-0-60", "not an id");
    let error = Records::decode(&bad_id).unwrap_err();
    assert!(error.to_string().contains("an id is 1 to 128"), "{error}");
}

/// Asserts that an offer's shape survives the file: both execution modes, a gpu class and
/// none, and that it prices and rebuilds the offer it was taken from.
#[test]
fn a_stored_offer_rebuilds_its_offer() {
    let sandbox = Offer {
        exec: ExecMode::SandboxExec,
        ..offer("mac", 40)
    };
    let bare = Offer {
        gpu_class: None,
        gpus: 0,
        ..offer("cpu", 10)
    };
    for original in [offer("a10", 60), sandbox, bare] {
        let stored = StoredOffer::from_offer(&original);
        assert_eq!(
            stored.to_offer(original.price_cents_per_hour.unwrap()),
            original
        );
    }
    let unknown = StoredOffer::nothing("x", None);
    assert_eq!(unknown.exec, StoredExec::Container);
    let shown = unknown.to_offer(5);
    assert_eq!(shown.cpu_millis, 0);
    assert_eq!(shown.gpu_class, None);
}

const ALL: [Phase; 11] = [
    Phase::Planned,
    Phase::Launching,
    Phase::Unresolved,
    Phase::Booting,
    Phase::Registered,
    Phase::Draining,
    Phase::Releasing,
    Phase::Refused,
    Phase::Failed,
    Phase::Gone,
    Phase::Orphan,
];

/// Asserts the state machine's table, pair by pair: exactly the transitions of section
/// 6.2 of the design are allowed, and every other pair of phases is refused.
#[test]
fn the_state_machine_allows_exactly_the_listed_transitions() {
    let allowed = [
        (Phase::Planned, Phase::Launching),
        (Phase::Launching, Phase::Planned),
        (Phase::Launching, Phase::Booting),
        (Phase::Launching, Phase::Refused),
        (Phase::Launching, Phase::Unresolved),
        (Phase::Launching, Phase::Failed),
        (Phase::Unresolved, Phase::Booting),
        (Phase::Unresolved, Phase::Failed),
        (Phase::Booting, Phase::Registered),
        (Phase::Booting, Phase::Releasing),
        (Phase::Registered, Phase::Draining),
        (Phase::Registered, Phase::Releasing),
        (Phase::Draining, Phase::Registered),
        (Phase::Draining, Phase::Releasing),
        (Phase::Releasing, Phase::Gone),
        (Phase::Failed, Phase::Releasing),
        (Phase::Orphan, Phase::Gone),
    ];
    for from in ALL {
        for to in ALL {
            assert_eq!(
                from.may_become(to),
                allowed.contains(&(from, to)),
                "{from:?} to {to:?}"
            );
        }
    }
}

/// Asserts what counts as a machine that exists or may exist, by phase, and that a
/// failed acquire stops counting at its second clean list.
#[test]
fn only_some_phases_count() {
    let counted: Vec<Phase> = ALL
        .into_iter()
        .filter(|phase| record(*phase, 60, 0).counts())
        .collect();
    assert_eq!(
        counted,
        [
            Phase::Planned,
            Phase::Launching,
            Phase::Unresolved,
            Phase::Booting,
            Phase::Registered,
            Phase::Draining,
            Phase::Releasing,
            Phase::Failed,
            Phase::Orphan,
        ]
    );
    let mut settled: Record = record(Phase::Failed, 60, 0);
    settled.clean_lists = 2;
    assert!(!settled.counts());
}
