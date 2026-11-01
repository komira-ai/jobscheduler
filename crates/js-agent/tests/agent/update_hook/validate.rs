//! The rules a record is held to before anything in it is used, one table per rule: what
//! passes and what breaks it, with the rule named.

use std::path::PathBuf;

use js_agent::update_hook::{
    CUTOVER_MARGIN_SECS, Cordon, Declaration, EXPIRY_SLACK_MS, Invalid, MAX_VERSION_LEN, POLL_SECS,
    PREPARE_MARGIN_SECS, Params, Record, Rollback, SCHEMA, START_WAIT_SECS, Timeouts, UpdateInfo,
    VERIFY_MARGIN_SECS, validate,
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
        runtime: "crun".to_owned(),
    }
}

/// The table the updater would have passed for the update `record` is of.
fn params_for(record: &Record) -> Params {
    Params {
        cordon_lease_secs: 300,
        cordon_max_secs: 17355,
        drain_wait_secs: 3600,
        defer_retry_secs: 300,
        artifact_digest: record.update.artifact_digest.clone(),
        promotion_ceiling_secs: None,
        staged_release: record.update.staged.clone(),
        previous_release: record.update.previous_target.clone(),
        release_link: None,
        target_version: record.update.target.clone(),
        previous_version: record.update.previous.clone(),
    }
}

fn params() -> Params {
    params_for(&good())
}

/// A record that breaks no rule: the cordon placed, the stop begun, nothing after.
fn good() -> Record {
    let mut record = Record::new(UpdateInfo {
        target: "1.4.0".to_owned(),
        previous: Some("1.3.2".to_owned()),
        staged: PathBuf::from("/opt/js-agent/releases/1.4.0"),
        previous_target: Some(PathBuf::from("/opt/js-agent/releases/1.3.2")),
        unit: UNIT.to_owned(),
        artifact_digest: format!("sha256:{}", "0123456789abcdef".repeat(4)),
    });
    record.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: 1_000_000,
        expires_ms: 1_300_000,
    };
    record.stop_begun = true;
    record
}

fn check(record: &Record) -> Result<(), Invalid> {
    validate(record, &declaration(), &params_for(record))
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
    for good_version in [
        "1",
        "1.4.0",
        "1.4.0-rc.1+build_5~x",
        "1.-0",
        "1.0.",
        longest.as_str(),
    ] {
        passes(|r| {
            r.update.target = good_version.to_owned();
            r.update.previous = Some("0.0.1".to_owned());
        });
        passes(|r| {
            r.update.target = "9.9.9".to_owned();
            r.update.previous = Some(good_version.to_owned());
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
        "..",
        "1..0",
        "1.0..",
        "-1.0",
        "-",
    ] {
        assert_eq!(
            broken(|r| r.update.target = bad.to_owned()),
            Invalid::Version { field: "target" },
            "{bad:?}"
        );
        assert_eq!(
            broken(|r| r.update.previous = Some(bad.to_owned())),
            Invalid::Version { field: "previous" },
            "{bad:?}"
        );
    }
    assert_eq!(
        broken(|r| r.update.previous = Some(r.update.target.clone())),
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
fn release_paths_are_plain_and_outside_the_state_directory() {
    passes(|r| r.update.staged = PathBuf::from("/opt/js-agent-link/1.4.0"));
    passes(|r| r.update.staged = PathBuf::from("/srv/anywhere/a/b/c/d"));
    // Only the state directory is out of bounds, and by whole components.
    passes(|r| r.update.staged = PathBuf::from("/var/lib/js-agent-update2/x"));
    let bad = [
        ("relative", "opt/js-agent/1.4.0"),
        ("current directory first", "./opt/js-agent/1.4.0"),
        ("parent", "/opt/js-agent/../../etc/cron.d"),
        ("parent at the end", "/opt/js-agent/1.4.0/.."),
        ("current directory", "/opt/js-agent/./1.4.0"),
        ("doubled slash", "/opt/js-agent//1.4.0"),
        ("trailing slash", "/opt/js-agent/1.4.0/"),
        (
            "below the state directory",
            "/var/lib/js-agent-update/1.4.0",
        ),
        ("the state directory", "/var/lib/js-agent-update"),
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
            broken(|r| r.update.previous_target = Some(PathBuf::from(path))),
            Invalid::Path {
                field: "previous_target"
            },
            "previous_target, {name}"
        );
    }
}

#[test]
fn a_first_install_has_no_previous_version_or_release() {
    let first = |r: &mut Record| {
        r.update.previous = None;
        r.update.previous_target = None;
    };
    passes(first);
    // Both sides must say so: the table naming a previous the record lacks is a disagreement.
    let mut record = good();
    first(&mut record);
    assert_eq!(
        validate(&record, &declaration(), &params()),
        Err(Invalid::ParamsMismatch { field: "previous" })
    );
    // And the record naming a previous the first-install table lacks.
    let mut table = params();
    table.previous_version = None;
    table.previous_release = None;
    assert_eq!(
        validate(&good(), &declaration(), &table),
        Err(Invalid::ParamsMismatch { field: "previous" })
    );
}

#[test]
fn the_record_must_say_what_the_params_table_says() {
    let record = good();
    assert_eq!(validate(&record, &declaration(), &params()), Ok(()));
    type Change = fn(&mut Params);
    let cases: [(&str, Change); 4] = [
        ("target", |p| p.target_version = "1.4.1".to_owned()),
        ("previous", |p| {
            p.previous_version = Some("1.3.1".to_owned())
        }),
        ("staged", |p| {
            p.staged_release = PathBuf::from("/opt/js-agent/releases/other")
        }),
        ("previous_target", |p| {
            p.previous_release = Some(PathBuf::from("/opt/js-agent/releases/other"))
        }),
    ];
    for (field, change) in cases {
        let mut table = params();
        change(&mut table);
        assert_eq!(
            validate(&record, &declaration(), &table),
            Err(Invalid::ParamsMismatch { field }),
            "{field}"
        );
    }
    // Equal is not enough: the record's own value must still be sound.
    let mut record = good();
    record.update.staged = PathBuf::from("/opt/js-agent/../../etc");
    assert_eq!(
        validate(&record, &declaration(), &params_for(&record)),
        Err(Invalid::Path { field: "staged" })
    );
}

#[test]
fn a_cordon_that_was_not_placed_has_no_stamps() {
    passes(|r| r.cordon = Cordon::default());
    for cordon in [
        Cordon {
            placed: false,
            provisional: false,
            first_ms: 1,
            expires_ms: 0,
        },
        Cordon {
            placed: false,
            provisional: false,
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
            provisional: false,
            first_ms: 0,
            expires_ms: 5_000,
        },
        Cordon {
            placed: true,
            provisional: false,
            first_ms: 0,
            expires_ms: 0,
        },
        Cordon {
            placed: true,
            provisional: false,
            first_ms: 5_000,
            expires_ms: 4_999,
        },
    ] {
        assert_eq!(broken(|r| r.cordon = cordon), Invalid::CordonStamps);
    }
    passes(|r| {
        r.cordon = Cordon {
            placed: true,
            provisional: false,
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
        provisional: false,
        first_ms: 1,
        expires_ms: u64::MAX,
    };
    assert_eq!(validate(&record, &declaration(), &huge), Ok(()));
    // The sum does not fit either: it does not wrap to a small limit that refuses a stamp
    // near the end of time.
    record.cordon = Cordon {
        placed: true,
        provisional: false,
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

#[test]
fn the_comparison_with_the_table_is_exact_not_by_path_components() {
    // The record's path is plain; the table spells the same directory another way.
    for spelling in [
        "/opt/js-agent/releases/1.4.0/",
        "/opt/js-agent/./releases/1.4.0",
        "/opt/js-agent//releases/1.4.0",
        "/opt/js-agent/releases/x/../1.4.0",
    ] {
        let mut table = params();
        table.staged_release = PathBuf::from(spelling);
        assert_eq!(
            validate(&good(), &declaration(), &table),
            Err(Invalid::ParamsMismatch { field: "staged" }),
            "{spelling}"
        );
        let mut table = params();
        table.previous_release = Some(PathBuf::from(spelling));
        assert_eq!(
            validate(&good(), &declaration(), &table),
            Err(Invalid::ParamsMismatch {
                field: "previous_target"
            }),
            "{spelling}"
        );
    }
}

#[test]
fn the_previous_version_and_the_previous_release_come_together_or_not_at_all() {
    // The record with one but not the other.
    for change in [
        (|r: &mut Record| r.update.previous = None) as fn(&mut Record),
        |r| r.update.previous_target = None,
    ] {
        let mut record = good();
        change(&mut record);
        assert_eq!(
            validate(&record, &declaration(), &params_for(&record)),
            Err(Invalid::Unpaired { side: "record" })
        );
    }
    // The table with one but not the other: refused before the two are compared.
    for change in [
        (|p: &mut Params| p.previous_version = None) as fn(&mut Params),
        |p| p.previous_release = None,
    ] {
        let mut table = params();
        change(&mut table);
        assert_eq!(
            validate(&good(), &declaration(), &table),
            Err(Invalid::Unpaired { side: "params" })
        );
    }
}

#[test]
fn the_staged_and_the_previous_release_differ() {
    assert_eq!(
        broken(|r| r.update.previous_target = Some(r.update.staged.clone())),
        Invalid::SameRelease
    );
}

#[test]
fn the_new_rules_have_words_for_the_log() {
    assert_eq!(
        Invalid::Unpaired { side: "params" }.to_string(),
        "the previous version and the previous release must both be present or both absent in the params"
    );
    assert_eq!(
        Invalid::SameRelease.to_string(),
        "the staged release and the previous release are the same"
    );
}

#[test]
fn the_recorded_digest_must_be_the_one_the_updater_passes() {
    let record = good();
    let other = Params {
        artifact_digest: format!("sha256:{}", "f".repeat(64)),
        ..params_for(&record)
    };
    assert_eq!(
        validate(&record, &declaration(), &other),
        Err(Invalid::ParamsMismatch {
            field: "artifact_digest"
        })
    );
}

#[test]
fn a_provisional_cordon_is_a_placed_one() {
    passes(|r| r.cordon.provisional = true);
    assert_eq!(
        broken(|r| r.cordon = Cordon {
            provisional: true,
            ..Cordon::default()
        }),
        Invalid::CordonStamps
    );
}

#[test]
fn the_declared_prepare_timeout_must_outlast_the_drain_wait_by_the_margin() {
    let record = good();
    let mut declaration = declaration();
    let params = params_for(&record);
    let needed = params.drain_wait_secs + PREPARE_MARGIN_SECS;
    declaration.timeouts.prepare = needed;
    assert_eq!(validate(&record, &declaration, &params), Ok(()));
    declaration.timeouts.prepare = needed - 1;
    let error = validate(&record, &declaration, &params).unwrap_err();
    assert_eq!(
        error,
        Invalid::PrepareTimeout {
            prepare: needed - 1,
            wait: params.drain_wait_secs
        }
    );
    let words = error.to_string();
    assert!(words.contains("3659") && words.contains("3600"), "{words}");
}

#[test]
fn the_declared_cutover_timeout_must_leave_time_to_stop() {
    let record = good();
    let mut declaration = declaration();
    let params = params_for(&record);
    let needed = START_WAIT_SECS + CUTOVER_MARGIN_SECS + 1;
    declaration.timeouts.cutover = needed;
    assert_eq!(validate(&record, &declaration, &params), Ok(()));
    declaration.timeouts.cutover = needed - 1;
    let error = validate(&record, &declaration, &params).unwrap_err();
    assert_eq!(
        error,
        Invalid::CutoverTimeout {
            cutover: needed - 1
        }
    );
    assert!(error.to_string().contains("180"), "{error}");
}

#[test]
fn a_first_install_may_move_the_link_without_a_stop_but_not_start_before_it() {
    let mut record = good();
    record.stop_begun = false;
    record.update.previous = None;
    record.update.previous_target = None;
    record.cordon = Cordon::default();
    record.link_moved = true;
    let params = params_for(&record);
    assert_eq!(validate(&record, &declaration(), &params), Ok(()));
    record.link_moved = false;
    record.start_issued = true;
    assert_eq!(
        validate(&record, &declaration(), &params),
        Err(Invalid::Marks {
            mark: "start_issued",
            needs: "link_moved"
        })
    );
}

#[test]
fn an_update_with_a_previous_release_may_not_move_the_link_without_a_stop() {
    let mut record = good();
    record.stop_begun = false;
    record.link_moved = true;
    assert!(record.update.previous.is_some());
    assert_eq!(
        validate(&record, &declaration(), &params_for(&record)),
        Err(Invalid::Marks {
            mark: "link_moved",
            needs: "stop_begun"
        })
    );
}

#[test]
fn the_declared_verify_timeout_must_outlast_the_margin_and_one_poll() {
    let record = good();
    let mut declaration = declaration();
    let params = params_for(&record);
    let needed = VERIFY_MARGIN_SECS + POLL_SECS + 1;
    declaration.timeouts.verify = needed;
    assert_eq!(validate(&record, &declaration, &params), Ok(()));
    for refused in [needed - 1, VERIFY_MARGIN_SECS, 1, 0] {
        declaration.timeouts.verify = refused;
        let error = validate(&record, &declaration, &params).unwrap_err();
        assert!(
            error.to_string().contains("verify timeout"),
            "{refused}: {error}"
        );
        assert!(error.to_string().contains(&refused.to_string()), "{error}");
    }
}

/// `record` with the session the server showed before the stop.
fn with_pre_stop(record: &Record, session_ms: u64) -> Record {
    let mut record = record.clone();
    record.pre_stop_session_ms = Some(session_ms);
    record
}

#[test]
fn the_session_before_the_stop_is_allowed_with_the_stop_mark_and_after_it() {
    let begun = good();
    assert!(begun.stop_begun);
    assert_eq!(check(&with_pre_stop(&begun, 7)), Ok(()));
    let mut moved = good();
    moved.link_moved = true;
    assert_eq!(check(&with_pre_stop(&moved, 7)), Ok(()));
    let mut done = good();
    done.link_moved = true;
    done.start_issued = true;
    done.verify_ok = true;
    assert_eq!(check(&with_pre_stop(&done, 0)), Ok(()));
    // Absent is allowed anywhere: the server may have shown no session.
    assert_eq!(check(&good()), Ok(()));
}

#[test]
fn the_session_before_the_stop_needs_the_stop_mark() {
    let mut record = good();
    record.stop_begun = false;
    assert_eq!(
        check(&with_pre_stop(&record, 7)),
        Err(Invalid::Marks {
            mark: "pre_stop_session_ms",
            needs: "stop_begun"
        })
    );
}

#[test]
fn a_first_install_has_no_session_before_the_stop() {
    let mut record = good();
    record.update.previous = None;
    record.update.previous_target = None;
    record.stop_begun = false;
    record.cordon = Cordon::default();
    assert_eq!(check(&record), Ok(()));
    let with = with_pre_stop(&record, 7);
    assert_eq!(
        check(&with),
        Err(Invalid::Marks {
            mark: "pre_stop_session_ms",
            needs: "stop_begun"
        })
    );
    // Even with a stop mark, which a first install never writes.
    let mut marked = with;
    marked.stop_begun = true;
    assert_eq!(
        check(&marked),
        Err(Invalid::Marks {
            mark: "pre_stop_session_ms",
            needs: "a previous release"
        })
    );
}

#[test]
fn a_record_without_the_session_before_the_stop_is_still_read_and_a_missing_one_is_not_written() {
    let record = good();
    let mut value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("pre_stop_session_ms").is_none(),
        "a record without a session is written as an older hook wrote it"
    );
    value.as_object_mut().unwrap().remove("pre_stop_session_ms");
    assert_eq!(serde_json::from_value::<Record>(value).unwrap(), record);
    let with = serde_json::to_value(with_pre_stop(&record, 9)).unwrap();
    assert_eq!(with["pre_stop_session_ms"], serde_json::json!(9));
}
