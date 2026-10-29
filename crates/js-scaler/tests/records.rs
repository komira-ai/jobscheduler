//! The records file: its literal form, its version, what it refuses, and the state
//! machine's table of transitions.

mod common;

use std::collections::BTreeMap;

use common::*;
use js_elastic::Offer;
use js_scaler::{
    Cooldown, CooldownReason, OLDEST, Phase, Record, Records, StoredExec, StoredOffer, VERSION,
};
use js_types::ExecMode;

const GOLDEN: &str = include_str!("golden/records_v2.json");
const GOLDEN_V1: &str = include_str!("golden/records_v1.json");

/// The records the golden file holds: one launch with every optional field empty, and
/// an orphan with every one of them set, in the other execution mode, one silent machine
/// and one missing machine.
fn sample() -> Records {
    let launching = record(Phase::Launching, 60, 0);
    let mut orphan = record(Phase::Orphan, 90, MINUTE);
    orphan.acquire = None;
    orphan.name = String::new();
    orphan.offer.exec = StoredExec::SandboxExec;
    orphan.token_expires = Some(at(2 * MINUTE));
    orphan.cloud_id = Some("i-stray".to_owned());
    orphan.retry_at = Some(at(4 * MINUTE));
    orphan.attempt = 3;
    orphan.gone_at = Some(at(5 * MINUTE));
    orphan.registered = true;
    orphan.condemned = true;
    orphan.clean_lists = 2;
    orphan.last_clean = Some(at(3 * MINUTE));
    orphan.seen_live = true;
    let mut records = Records {
        instances: vec![launching, orphan],
        last_step: Some(at(40 * SECOND)),
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
        .silent_since
        .insert("js-made-0-60".to_owned(), at(30 * SECOND));
    records
        .missing_since
        .insert("js-gone-away".to_owned(), at(2 * MINUTE));
    records
}

/// Asserts the literal text of the records file, version 2, for a launch and for an
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

/// Asserts that a file of version 1, written before the silent and missing times were
/// kept, still reads: its machines and cooldowns are what they were and the times are
/// empty, so the counts start again; and that what is written back is version 2.
#[test]
fn a_version_1_file_is_read_and_written_back_as_version_2() {
    let old = Records::decode(GOLDEN_V1).unwrap();
    let expected = Records {
        silent_since: BTreeMap::new(),
        missing_since: BTreeMap::new(),
        last_step: None,
        ..sample()
    };
    assert_eq!(old, expected);
    let text = old.encode().unwrap();
    assert!(text.starts_with("{\n  \"version\": 2,"), "{text}");
    assert_eq!(Records::decode(&text).unwrap(), expected);

    let extra = GOLDEN_V1.replace("\"version\": 1,", "\"version\": 1, \"silent_since\": {},");
    let error = Records::decode(&extra).unwrap_err();
    assert!(error.to_string().contains("silent_since"), "{error}");
}

/// Asserts that a file of version 2 that names no time of its last write reads with none:
/// the scaler then takes the whole span back to each saved time as downtime.
#[test]
fn a_version_2_file_without_the_time_of_its_last_write_reads_with_none() {
    let old = GOLDEN.replace(",\n  \"last_step\": 1000000040000", "");
    assert!(!old.contains("last_step"), "{old}");
    let records = Records::decode(&old).unwrap();
    assert_eq!(records.last_step, None);
    assert_eq!(
        records,
        Records {
            last_step: None,
            ..sample()
        }
    );
}

/// Asserts that every version from `OLDEST` to `VERSION` reads, each from its own golden
/// file, and is written back as `VERSION`; and that the versions on either side, and a
/// number that only wraps into the range, are refused. Raising `OLDEST` or `VERSION` alone
/// changes what this reads, so a golden is owed for each version in the range.
#[test]
fn every_version_in_the_supported_range_reads() {
    for version in OLDEST..=VERSION {
        let path = format!(
            "{}/tests/golden/records_v{version}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let records = Records::decode(&text).unwrap();
        let again = records.encode().unwrap();
        assert!(again.starts_with(&format!("{{\n  \"version\": {VERSION},")));
        assert_eq!(Records::decode(&again).unwrap(), records);
    }
    for version in [
        u64::from(OLDEST) - 1,
        u64::from(VERSION) + 1,
        u64::from(u32::MAX) + 2,
    ] {
        let text = GOLDEN.replace("\"version\": 2", &format!("\"version\": {version}"));
        let error = Records::decode(&text).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("records: version Some({version})")),
            "{error}"
        );
    }
}

/// Asserts what is refused: text that is not JSON, a file of another version or of none,
/// a field the format does not have, a missing field, and an acquire id that is not one.
/// Each error says what is wrong.
#[test]
fn a_file_that_is_not_records_of_this_version_is_refused() {
    let broken = Records::decode("{not json").unwrap_err();
    assert!(broken.to_string().starts_with("records: "), "{broken}");

    let other = GOLDEN.replace("\"version\": 2", "\"version\": 3");
    let error = Records::decode(&other).unwrap_err();
    assert_eq!(
        error.to_string(),
        "records: version Some(3), this reader reads 1 to 2"
    );

    let none = Records::decode("{}").unwrap_err();
    assert_eq!(
        none.to_string(),
        "records: version None, this reader reads 1 to 2"
    );

    let extra = GOLDEN.replace("\"version\": 2,", "\"version\": 2, \"surprise\": 1,");
    let error = Records::decode(&extra).unwrap_err();
    assert!(error.to_string().contains("surprise"), "{error}");

    let missing = GOLDEN.replace("\"clean_lists\": 0,", "");
    let error = Records::decode(&missing).unwrap_err();
    assert!(error.to_string().contains("clean_lists"), "{error}");

    let bad_id = GOLDEN.replace("made-0-60", "not an id");
    let error = Records::decode(&bad_id).unwrap_err();
    assert!(error.to_string().contains("an id is 1 to 128"), "{error}");
}

/// Asserts that `seen_live` survives the file, and that a file written before the field
/// existed (every record without it) still reads, with the field `true`: the safe direction,
/// as the lost-Welcome rule stays off for a host that may have been heard from.
#[test]
fn seen_live_is_kept_and_an_old_file_without_it_reads_true() {
    let kept = Records::decode(&sample().encode().unwrap()).unwrap();
    assert!(!kept.instances[0].seen_live);
    assert!(kept.instances[1].seen_live);

    let old: String = GOLDEN
        .replace(",\n      \"seen_live\": false", "")
        .replace(",\n      \"seen_live\": true", "");
    assert!(!old.contains("seen_live"));
    let read = Records::decode(&old).unwrap();
    assert!(read.instances.iter().all(|each| each.seen_live));
    let mut expected = sample();
    expected.instances[0].seen_live = true;
    assert_eq!(read, expected);
}

/// Asserts the same for a file of version 1: the field is read where the file has it, reads
/// `true` where it has none, and is written back, as version 2, with the value read.
#[test]
fn seen_live_reads_from_a_version_1_file_with_and_without_it() {
    let with = Records::decode(GOLDEN_V1).unwrap();
    assert!(!with.instances[0].seen_live);
    assert!(with.instances[1].seen_live);

    let old: String = GOLDEN_V1
        .replace(",\n      \"seen_live\": false", "")
        .replace(",\n      \"seen_live\": true", "");
    assert!(!old.contains("seen_live"));
    let read = Records::decode(&old).unwrap();
    assert!(read.instances.iter().all(|each| each.seen_live));
    let text = read.encode().unwrap();
    assert!(text.starts_with("{\n  \"version\": 2,"), "{text}");
    assert!(!text.contains("\"seen_live\": false"), "{text}");
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
