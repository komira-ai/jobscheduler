//! The rules a record is held to before anything in it is used, one table per rule: what
//! passes and what breaks it, with the rule named.

use std::path::PathBuf;

use js_agent::update_hook::{
    Cordon, Declaration, EXPIRY_SLACK_MS, Invalid, MAX_VERSION_LEN, Params, Record, Rollback,
    SCHEMA, Timeouts, UpdateInfo, validate,
};

const UNIT: &str = "js-agent-crun.service";

fn declaration() -> Declaration {
    Declaration {
        state_dir: PathBuf::from("/var/lib/js-agent-update"),
        units: vec![UNIT.to_owned(), "js-agent-docker.service".to_owned()],
        write_paths: vec![
            PathBuf::from("/opt/js-agent"),
            PathBuf::from("/opt/js-agent-link"),
            PathBuf::from("/var/lib/js-agent-update"),
        ],
        timeouts: Timeouts {
            prepare: 3900,
            cutover: 4320,
            verify: 615,
            rollback: 8220,
            cleanup: 120,
        },
    }
}

fn params() -> Params {
    Params {
        cordon_lease_secs: 300,
        cordon_max_secs: 17355,
        promotion_ceiling_secs: None,
    }
}

/// A record that breaks no rule: the cordon placed, the stop begun, nothing after.
fn good() -> Record {
    let mut record = Record::new(UpdateInfo {
        target: "1.4.0".to_owned(),
        previous: "1.3.2".to_owned(),
        staged: PathBuf::from("/opt/js-agent/releases/1.4.0"),
        previous_target: PathBuf::from("/opt/js-agent/releases/1.3.2"),
        unit: UNIT.to_owned(),
        artifact_digest: format!("sha256:{}", "0123456789abcdef".repeat(4)),
    });
    record.cordon = Cordon {
        placed: true,
        first_ms: 1_000_000,
        expires_ms: 1_300_000,
    };
    record.stop_begun = true;
    record
}

fn check(record: &Record) -> Result<(), Invalid> {
    validate(record, &declaration(), &params())
}

/// The rule `record` breaks, after `change` is applied to a good one.
fn broken(change: impl FnOnce(&mut Record)) -> Invalid {
    let mut record = good();
    change(&mut record);
    check(&record).expect_err("the change should break a rule")
}

fn passes(change: impl FnOnce(&mut Record)) {
    let mut record = good();
    change(&mut record);
    assert_eq!(check(&record), Ok(()));
}

#[test]
fn a_good_record_passes() {
    assert_eq!(check(&good()), Ok(()));
    assert_eq!(check(&Record::new(good().update)), Ok(()));
}

#[test]
fn the_schema_must_be_the_one_the_hook_writes() {
    assert_eq!(
        broken(|r| r.schema = SCHEMA + 1),
        Invalid::Schema { found: SCHEMA + 1 }
    );
    assert_eq!(broken(|r| r.schema = 0), Invalid::Schema { found: 0 });
}

#[test]
fn the_unit_must_be_a_declared_unit() {
    passes(|r| r.update.unit = "js-agent-docker.service".to_owned());
    for unit in [
        "",
        "js-agent-crun",
        "sshd.service",
        "js-agent-crun.service ",
    ] {
        assert_eq!(
            broken(|r| r.update.unit = unit.to_owned()),
            Invalid::Unit {
                found: unit.to_owned()
            },
            "{unit:?}"
        );
    }
}

#[test]
fn versions_are_short_plain_words_and_differ() {
    let longest = "9".repeat(MAX_VERSION_LEN);
    for good_version in ["1", "1.4.0", "1.4.0-rc.1+build_5~x", longest.as_str()] {
        passes(|r| {
            r.update.target = good_version.to_owned();
            r.update.previous = "0.0.1".to_owned();
        });
        passes(|r| {
            r.update.target = "9.9.9".to_owned();
            r.update.previous = good_version.to_owned();
        });
    }
    let too_long = "9".repeat(MAX_VERSION_LEN + 1);
    for bad in [
        "",
        too_long.as_str(),
        "1.4 0",
        "1.4/0",
        "../1.4.0",
        "1.4.0\n",
        "v\u{e9}",
        "1;rm",
    ] {
        assert_eq!(
            broken(|r| r.update.target = bad.to_owned()),
            Invalid::Version { field: "target" },
            "{bad:?}"
        );
        assert_eq!(
            broken(|r| r.update.previous = bad.to_owned()),
            Invalid::Version { field: "previous" },
            "{bad:?}"
        );
    }
    assert_eq!(
        broken(|r| r.update.previous = r.update.target.clone()),
        Invalid::SameVersion
    );
}

#[test]
fn the_artifact_digest_is_sha256_and_64_lowercase_hex_digits() {
    let hex = "0123456789abcdef".repeat(4);
    passes(|r| r.update.artifact_digest = format!("sha256:{hex}"));
    for bad in [
        String::new(),
        hex.clone(),
        "sha256:".to_owned(),
        format!("sha256:{}", &hex[1..]),
        format!("sha256:{hex}0"),
        format!("sha256:{}", hex.to_uppercase()),
        format!("sha512:{hex}"),
        format!("sha256:{}g", &hex[1..]),
        format!(" sha256:{hex}"),
    ] {
        assert_eq!(
            broken(|r| r.update.artifact_digest = bad.clone()),
            Invalid::Digest,
            "{bad:?}"
        );
    }
}

#[test]
fn release_paths_are_plain_and_below_a_declared_release_directory() {
    passes(|r| r.update.staged = PathBuf::from("/opt/js-agent-link/1.4.0"));
    passes(|r| r.update.staged = PathBuf::from("/opt/js-agent/a/b/c/d"));
    let bad = [
        ("relative", "opt/js-agent/1.4.0"),
        ("current directory first", "./opt/js-agent/1.4.0"),
        ("parent", "/opt/js-agent/../../etc/cron.d"),
        ("parent at the end", "/opt/js-agent/1.4.0/.."),
        ("current directory", "/opt/js-agent/./1.4.0"),
        ("doubled slash", "/opt/js-agent//1.4.0"),
        ("trailing slash", "/opt/js-agent/1.4.0/"),
        ("outside", "/srv/js-agent/1.4.0"),
        ("the directory itself", "/opt/js-agent"),
        ("a sibling with the same prefix", "/opt/js-agent-evil/1.4.0"),
        ("the state directory", "/var/lib/js-agent-update/1.4.0"),
        ("the root", "/"),
        ("empty", ""),
    ];
    for (name, path) in bad {
        assert_eq!(
            broken(|r| r.update.staged = PathBuf::from(path)),
            Invalid::Path { field: "staged" },
            "staged, {name}"
        );
        assert_eq!(
            broken(|r| r.update.previous_target = PathBuf::from(path)),
            Invalid::Path {
                field: "previous_target"
            },
            "previous_target, {name}"
        );
    }
}

#[test]
fn a_cordon_that_was_not_placed_has_no_stamps() {
    passes(|r| r.cordon = Cordon::default());
    for cordon in [
        Cordon {
            placed: false,
            first_ms: 1,
            expires_ms: 0,
        },
        Cordon {
            placed: false,
            first_ms: 0,
            expires_ms: 1,
        },
    ] {
        assert_eq!(broken(|r| r.cordon = cordon), Invalid::CordonStamps);
    }
}

#[test]
fn a_placed_cordon_has_a_first_stamp_and_expires_no_earlier() {
    for cordon in [
        Cordon {
            placed: true,
            first_ms: 0,
            expires_ms: 5_000,
        },
        Cordon {
            placed: true,
            first_ms: 0,
            expires_ms: 0,
        },
        Cordon {
            placed: true,
            first_ms: 5_000,
            expires_ms: 4_999,
        },
    ] {
        assert_eq!(broken(|r| r.cordon = cordon), Invalid::CordonStamps);
    }
    passes(|r| {
        r.cordon = Cordon {
            placed: true,
            first_ms: 5_000,
            expires_ms: 5_000,
        }
    });
}

#[test]
fn a_cordon_expires_no_later_than_the_cap_after_its_first_placement() {
    let first_ms = 1_000_000;
    let limit_ms = first_ms + 17_355 * 1000 + EXPIRY_SLACK_MS;
    passes(|r| r.cordon.expires_ms = limit_ms);
    assert_eq!(
        broken(|r| r.cordon.expires_ms = limit_ms + 1),
        Invalid::CordonExpiry {
            expires_ms: limit_ms + 1,
            limit_ms
        }
    );
    // The declared cap decides, not a constant.
    let mut record = good();
    record.cordon.expires_ms = first_ms + 19_000_000;
    assert!(check(&record).is_err());
    let roomy = Params {
        cordon_max_secs: 20_000,
        ..params()
    };
    assert_eq!(validate(&record, &declaration(), &roomy), Ok(()));
}

#[test]
fn a_cap_too_large_to_add_saturates_and_never_wraps() {
    let mut record = good();
    // The cap in milliseconds does not fit: the limit is the largest number.
    let huge = Params {
        cordon_max_secs: u64::MAX,
        ..params()
    };
    record.cordon = Cordon {
        placed: true,
        first_ms: 1,
        expires_ms: u64::MAX,
    };
    assert_eq!(validate(&record, &declaration(), &huge), Ok(()));
    // The sum does not fit either: it does not wrap to a small limit that refuses a stamp
    // near the end of time.
    record.cordon = Cordon {
        placed: true,
        first_ms: u64::MAX - 10,
        expires_ms: u64::MAX,
    };
    assert_eq!(validate(&record, &declaration(), &params()), Ok(()));
}

#[test]
fn a_mark_needs_the_mark_it_follows_from() {
    type Set = fn(&mut Record);
    // (the mark, how to set it alone, the mark it needs)
    let rules: [(&str, Set, &str); 8] = [
        ("link_moved", |r| r.link_moved = true, "stop_begun"),
        ("start_issued", |r| r.start_issued = true, "link_moved"),
        ("verify_ok", |r| r.verify_ok = true, "start_issued"),
        ("rb.drained", |r| r.rb.drained = true, "stop_begun"),
        ("rb.stop_begun", |r| r.rb.stop_begun = true, "rb.drained"),
        (
            "rb.link_restored",
            |r| r.rb.link_restored = true,
            "stop_begun",
        ),
        (
            "rb.start_issued",
            |r| r.rb.start_issued = true,
            "rb.link_restored",
        ),
        ("rb.hello_ok", |r| r.rb.hello_ok = true, "rb.start_issued"),
    ];
    for (mark, set, needs) in rules {
        // With nothing begun: the mark alone breaks its rule (a mark that needs the stop
        // is checked with the stop unset).
        let mut record = good();
        record.stop_begun = false;
        set(&mut record);
        assert_eq!(
            check(&record),
            Err(Invalid::Marks { mark, needs }),
            "{mark}"
        );
    }
}

#[test]
fn marks_that_follow_one_another_pass() {
    let forward = |r: &mut Record| {
        r.stop_begun = true;
        r.link_moved = true;
        r.start_issued = true;
        r.verify_ok = true;
    };
    passes(forward);
    passes(|r| {
        r.stop_begun = true;
        r.link_moved = true;
    });
    // A rollback of a failed cutover: relink, start, hello.
    passes(|r| {
        r.rb = Rollback {
            drained: false,
            stop_begun: false,
            link_restored: true,
            start_issued: true,
            hello_ok: true,
        }
    });
    // A rollback of a failed verify: drain, stop, relink, start, hello.
    passes(|r| {
        forward(r);
        r.rb = Rollback {
            drained: true,
            stop_begun: true,
            link_restored: true,
            start_issued: true,
            hello_ok: true,
        }
    });
}

#[test]
fn every_rule_has_words_for_the_log() {
    let texts = [
        (Invalid::Schema { found: 3 }, "schema 3 is not 1"),
        (
            Invalid::Unit {
                found: "x".to_owned(),
            },
            "unit \"x\" is not a declared unit",
        ),
        (
            Invalid::SameVersion,
            "target and previous version are the same",
        ),
        (
            Invalid::Digest,
            "artifact_digest is not sha256: and 64 lowercase hex digits",
        ),
        (
            Invalid::CordonStamps,
            "the cordon's placed mark and its stamps disagree",
        ),
        (
            Invalid::CordonExpiry {
                expires_ms: 9,
                limit_ms: 5,
            },
            "cordon expiry 9 is past the cap 5",
        ),
        (
            Invalid::Marks {
                mark: "verify_ok",
                needs: "start_issued",
            },
            "verify_ok is set but start_issued is not",
        ),
    ];
    for (invalid, text) in texts {
        assert_eq!(invalid.to_string(), text);
    }
    assert!(
        Invalid::Path { field: "staged" }
            .to_string()
            .starts_with("staged is not")
    );
    assert!(
        Invalid::Version { field: "target" }
            .to_string()
            .starts_with("target is not")
    );
}
