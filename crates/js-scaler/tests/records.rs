//! The records file: its literal form, its version, what it refuses, and the state
//! machine's table of transitions.

mod common;

use std::collections::BTreeMap;

use common::*;
use js_elastic::Offer;
use js_scaler::{
    Brake, Cooldown, CooldownReason, OLDEST, Phase, Record, Records, StoredExec, StoredOffer,
    VERSION,
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
        accelerator: None,
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
    assert_eq!(shown.accelerator, None);
}

/// Asserts that a record written when offers spelt Arm `aarch64` rebuilds an `arm64`
/// offer, which fits an `arm64` demand, and that another architecture is kept as written.
/// Catches a rebuilt offer that keeps `aarch64`, so a machine launched before the upgrade
/// would no longer count for the request it was launched for.
#[test]
fn a_stored_aarch64_offer_rebuilds_as_arm64() {
    let mut stored = StoredOffer::from_offer(&offer("arm", 60));
    stored.arch = "aarch64".to_owned();
    let rebuilt = stored.to_offer(60);
    assert_eq!(rebuilt.arch, "arm64");
    assert_eq!(rebuilt.isa_level, None);
    let mut wanted = demand();
    wanted.arch = Some("arm64".to_owned());
    assert_eq!(rebuilt.fit(&wanted), Ok(()));
    stored.arch = "riscv64".to_owned();
    assert_eq!(stored.to_offer(60).arch, "riscv64");
}

/// Asserts how an offer's stated ISA level meets the records file, in both directions
/// between this release and the one before it:
///
/// - the level is held by name and rebuilds the offer it was taken from;
/// - a file written with a stated level is byte for byte the file written with none, so
///   the previous release, whose reader refuses an unknown key, reads it (its own golden,
///   `records_v2.json`, is that text);
/// - this release reads a file that carries the key, as a later release will write it, and
///   one without it (the previous release's) as stating none;
/// - a held name that is not a level rebuilds an offer stating none.
///
/// Catches a release that writes the key before the release it rolls back to can read it,
/// which would refuse the records file and stop the pool at its restart.
#[test]
fn a_stated_isa_level_crosses_the_records_file_both_ways() {
    let level: js_types::IsaLevel = "x86-64-v3".parse().unwrap();
    let original = Offer {
        isa_level: Some(level),
        ..offer("a10", 60)
    };
    let stored = StoredOffer::from_offer(&original);
    assert_eq!(stored.isa_level.as_deref(), Some("x86-64-v3"));
    assert_eq!(stored.to_offer(60), original);

    let mut with_level = sample();
    with_level.instances[0].offer = stored.clone();
    let mut without = with_level.clone();
    without.instances[0].offer.isa_level = None;
    let written = with_level.encode().unwrap();
    assert_eq!(written, without.encode().unwrap());
    assert!(!written.contains("isa_level"), "{written}");
    assert_eq!(sample().encode().unwrap(), GOLDEN.trim_end());

    let previous = Records::decode(&written).unwrap();
    assert_eq!(previous.instances[0].offer.isa_level, None);
    let anchor = "\"arch\": \"x86_64\",";
    assert!(written.contains(anchor), "{written}");
    let later = written.replacen(
        anchor,
        &format!("{anchor} \"isa_level\": \"x86-64-v3\","),
        1,
    );
    let read = Records::decode(&later).unwrap();
    assert_eq!(read.instances[0].offer, stored);

    let mut unknown = stored;
    unknown.isa_level = Some("x86-64-v9".to_owned());
    assert_eq!(unknown.to_offer(60).isa_level, None);
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

/// Asserts that the `go` and `pool_kinds` keys are read, not dropped: a file of version 2 that
/// holds them reads with the go and the kinds as written, writes them back, and reads again as
/// the same records. A `go` of the wrong type is refused, as any other field is. The keys are
/// absent from a file written while no pool has launched, and the earlier reader that ignores
/// them (#452) reads either. It catches a key that is read and lost on the next save.
#[test]
fn the_go_and_pool_kinds_keys_are_read_and_written_back() {
    let with = GOLDEN.replacen(
        "\"last_step\"",
        "\"go\": {\"gpu\": {\"at\": 5, \"settled\": 7}},\n  \"pool_kinds\": {\"gpu\": \"x\"},\n  \"last_step\"",
        1,
    );
    let records = Records::decode(&with).unwrap();
    let go = js_scaler::Go {
        at: js_scaler::Millis::new(5),
        settled: 7,
    };
    assert_eq!(records.go["gpu"], go);
    assert_eq!(records.pool_kinds["gpu"], "x");
    let text = records.encode().unwrap();
    assert!(
        text.contains("\"go\"") && text.contains("\"pool_kinds\""),
        "{text}"
    );
    assert_eq!(Records::decode(&text).unwrap(), records);

    let without = Records::decode(GOLDEN).unwrap();
    assert!(without.go.is_empty() && without.pool_kinds.is_empty());
    let odd = GOLDEN.replacen("\"last_step\"", "\"go\": 3,\n  \"last_step\"", 1);
    assert!(Records::decode(&odd).is_err());
}

/// Asserts that the failure brake is written and read back whole (the count and the instant
/// of the trip, a pool with neither a count nor a trip is not written), that a file written
/// before the key reads with no brake, and that a file without any brake has no `brake` key.
/// It catches a brake that a restart would lose, and one that leaks into files that have none.
#[test]
fn the_failure_brake_round_trips_and_an_older_file_reads_without_it() {
    let mut records = Records::decode(GOLDEN).unwrap();
    assert!(records.brake.is_empty());
    assert!(!records.encode().unwrap().contains("\"brake\""));
    records.brake.insert(
        "gpu".to_owned(),
        Brake {
            failures: 2,
            tripped_at: None,
        },
    );
    records.brake.insert(
        "tpu".to_owned(),
        Brake {
            failures: 0,
            tripped_at: Some(at(7 * MINUTE)),
        },
    );
    assert_eq!(
        format!("{:?}", records.brake["gpu"]),
        "Brake { failures: 2, tripped_at: None }"
    );
    let text = records.encode().unwrap();
    assert!(text.contains("\"brake\""), "{text}");
    assert_eq!(Records::decode(&text).unwrap(), records);
    let bare = GOLDEN.replacen(
        "\"last_step\"",
        "\"brake\": {\"gpu\": {\"failures\": 1}},\n  \"last_step\"",
        1,
    );
    let read = Records::decode(&bare).unwrap();
    assert_eq!(read.brake["gpu"].failures, 1);
    assert_eq!(read.brake["gpu"].tripped_at, None);
}

/// Asserts that a brake written by a newer release reads: an extra field is ignored and a
/// missing `failures` reads as zero, with the fields it does have kept. It catches a reader
/// that refuses a records file because a later release added a field to the brake or
/// stopped writing one.
#[test]
fn a_brake_with_an_unknown_field_and_no_failures_reads() {
    let brake = format!(
        "\"brake\": {{\"gpu\": {{\"future\": 7, \"tripped_at\": {}}}}},\n  \"last_step\"",
        at(7 * MINUTE).as_millis()
    );
    let text = GOLDEN.replacen("\"last_step\"", &brake, 1);
    let read = Records::decode(&text).unwrap();
    assert_eq!(read.brake["gpu"].failures, 0);
    assert_eq!(read.brake["gpu"].tripped_at, Some(at(7 * MINUTE)));
}

/// Asserts that an offer with an accelerator round-trips through the record with its
/// family, variant, count and the variant's nominal memory, and that the keys written are
/// still `gpus` and `gpu_class` (the variant spelling, else the family). Catches a record
/// that drops the variant or the memory, or one that writes a key the release before
/// refuses.
#[test]
fn an_offer_with_an_accelerator_round_trips_through_the_record() {
    let original = Offer {
        accelerator: accel("nvidia-a100-sxm4-80gb", 4),
        ..offer("a100", 300)
    };
    let stored = StoredOffer::from_offer(&original);
    assert_eq!(stored.gpus, 4);
    assert_eq!(stored.gpu_class.as_deref(), Some("nvidia-a100-sxm4-80gb"));
    let rebuilt = stored.to_offer(300);
    assert_eq!(rebuilt, original);
    let held = rebuilt.accelerator.unwrap();
    assert_eq!(held.model.as_str(), "nvidia-a100");
    assert_eq!(held.variant.unwrap().as_str(), "nvidia-a100-sxm4-80gb");
    assert_eq!(held.count, 4);
    assert_eq!(held.memory_per_device, Some(80 << 30));

    let json = serde_json::to_value(&stored).unwrap();
    assert_eq!(json["gpus"], 4);
    assert_eq!(json["gpu_class"], "nvidia-a100-sxm4-80gb");
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "arch",
            "cpu_millis",
            "disk_bytes",
            "exec",
            "gpu_class",
            "gpus",
            "id",
            "memory_bytes",
            "os",
            "zone"
        ]
    );

    let family = StoredOffer::from_offer(&offer("a10", 60));
    assert_eq!(family.gpu_class.as_deref(), Some("nvidia-a10"));
    let mut hand_edited = family;
    hand_edited.gpu_class = Some("Not A Model".to_owned());
    assert_eq!(hand_edited.to_offer(60).accelerator, None);
}
