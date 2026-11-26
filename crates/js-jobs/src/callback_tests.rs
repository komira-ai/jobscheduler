use super::*;

/// The time `secs` seconds after the store clock's origin.
fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

/// The key every test signs with.
fn key() -> SigningKey {
    SigningKey::new(vec![b'k'; 32]).expect("a key")
}

/// A callback to a URL with a reference in its path, tried for `deadline_seconds`.
fn asked(deadline_seconds: Option<u32>) -> JobCallback {
    JobCallback {
        url: "https://hooks.example/js-done/secret-ref?token=t".to_owned(),
        key: CallbackKey::Key(key()),
        deadline_seconds,
    }
}

/// The armed row of [`asked`] for the job `job-1`.
fn armed(deadline_seconds: Option<u32>) -> CallbackRecord {
    let job = JobId::parse("job-1").expect("a job id");
    CallbackRecord::armed(
        job,
        &asked(deadline_seconds),
        key(),
        "d2ViaG9vaw".to_owned(),
    )
}

/// [`armed`], due at 100 s with its deadline at 160 s.
fn pending() -> CallbackRecord {
    armed(Some(60)).due(at(100), at(10_000)).expect("armed")
}

/// Asserts the whole `Debug` text of a claim whose URL carries a reference in its path and
/// query: the job and the `webhook-id`, nothing of the URL or the key. Catches a derived
/// `Debug` coming back, which prints the URL whole into a log line or a failed assertion.
#[test]
fn a_claims_debug_shows_its_job_and_webhook_id_alone() {
    let (_, claim) = pending().claimed(&InstanceId::new("server-a"), at(100));
    let claim = claim.expect("a claim");
    assert_eq!(
        format!("{claim:?}"),
        r#"CallbackClaim { job: JobId("job-1"), webhook_id: "d2ViaG9vaw", .. }"#
    );
}

/// Asserts the whole `Debug` text of a job's callback and of its key in both forms: the
/// deadline alone, and the form's name. Catches a derived `Debug`, which would print the URL
/// (and its reference) or the key's bytes into any log line that prints a job.
#[test]
fn a_callbacks_debug_shows_its_deadline_alone() {
    let callback = asked(Some(60));
    assert_eq!(
        format!("{callback:?}"),
        "JobCallback { deadline_seconds: Some(60), .. }"
    );
    assert_eq!(format!("{:?}", callback.key), "Key(..)");
    assert_eq!(
        format!("{:?}", CallbackKey::Digest(callback.key.digest())),
        "Digest(..)"
    );
}

/// Asserts that two callbacks are the same when their URL, deadline and key's SHA-256 agree,
/// whichever form each key is in, and differ when any of the three does; and that a digested
/// callback holds the key's SHA-256 and nothing else changed. Catches a comparison of the key's
/// form rather than its digest (a resubmit after the end would conflict) and one that leaves a
/// field out (a resubmit with another URL would be the same job).
#[test]
fn a_callback_is_compared_by_its_keys_digest() {
    let callback = asked(Some(60));
    let digested = callback.digested();
    let expected: [u8; 32] = Sha256::digest(key().as_bytes()).into();
    assert_eq!(digested.key.digest().bytes(), &expected);
    assert_eq!(
        (digested.url.as_str(), digested.deadline_seconds),
        (callback.url.as_str(), Some(60))
    );
    assert!(callback.same_as(&digested) & digested.same_as(&callback));
    assert_eq!(digested.digested(), digested);
    let other_key = SigningKey::new(vec![b'o'; 32]).expect("a key");
    let others = [
        JobCallback {
            url: "https://hooks.example/js-done/other".to_owned(),
            ..callback.clone()
        },
        JobCallback {
            key: CallbackKey::Key(other_key),
            ..callback.clone()
        },
        asked(None),
    ];
    for other in &others {
        assert!(!callback.same_as(other), "{other:?}");
    }
}

/// Asserts the wait after each failed try: 5 s, doubling, capped at 10 minutes, also for the
/// largest try count. Catches a wait that never doubles, a cap left out, and an exponent that
/// overflows.
#[test]
fn the_wait_doubles_from_five_seconds_to_ten_minutes() {
    let waits: Vec<u64> = (0..=9).map(|tries| wait_after(tries).as_secs()).collect();
    assert_eq!(waits, vec![5, 5, 10, 20, 40, 80, 160, 320, 600, 600]);
    assert_eq!(wait_after(u32::MAX), LONGEST_CALLBACK_WAIT);
}

/// Asserts that an armed row is made due at the end with its first try then and its deadline
/// the end plus the one it asked for (a day when it asked for none), cut to the retention end;
/// and that a row that is not armed is not made due again. Catches a deadline counted from the
/// submit, one not cut, and a second end that moves a delivery on.
#[test]
fn an_armed_row_is_made_due_once() {
    let due = armed(Some(60)).due(at(100), at(10_000)).expect("armed");
    assert_eq!(
        (due.state, due.next_try_at, due.deadline_at),
        (CallbackState::Pending, Some(at(100)), Some(at(160)))
    );
    assert_eq!(due.due_at(), Some(at(100)));
    let day = armed(None).due(at(100), at(1_000_000)).expect("armed");
    assert_eq!(day.deadline_at, Some(at(86_500)));
    let cut = armed(None).due(at(100), at(200)).expect("armed");
    assert_eq!(cut.deadline_at, Some(at(200)));
    assert_eq!(due.due(at(300), at(10_000)), None);
    assert_eq!(armed(Some(60)).due_at(), None);
    assert!(!armed(Some(60)).claimable(at(u64::MAX >> 2)));
}

/// Asserts a claim: one more try at its time, held for 30 s by its instance, the claim naming
/// the job, URL, key and `webhook-id`; held, the row is claimable again only at the claim's end.
/// A claim at or past the deadline, and one of a row without a key, expire the row instead,
/// erase its key and make no claim. Catches a claim that counts no try, one that holds the
/// row for another time, and a call sent past its deadline.
#[test]
fn a_claim_holds_the_row_or_expires_it() {
    let by = InstanceId::new("server-a");
    let (held, claim) = pending().claimed(&by, at(110));
    let claim = claim.expect("a claim");
    assert_eq!(
        (
            &claim.job,
            claim.url.as_str(),
            &claim.key,
            claim.webhook_id.as_str()
        ),
        (&pending().job, pending().url.as_str(), &key(), "d2ViaG9vaw")
    );
    assert_eq!(
        (held.tries, held.last_try_at, held.claim_until),
        (1, Some(at(110)), Some(at(140)))
    );
    assert!(held.holds(&claim.held));
    assert_eq!(held.due_at(), Some(at(140)));
    assert!(!held.claimable(at(139)) & held.claimable(at(140)));
    let other = Held {
        by: InstanceId::new("server-b"),
        tries: 1,
    };
    assert!(!held.holds(&other));
    let earlier = Held {
        by: InstanceId::new("server-a"),
        tries: 0,
    };
    assert!(!held.holds(&earlier));
    assert!(!pending().holds(&claim.held));

    let (expired, none) = pending().claimed(&by, at(160));
    assert_eq!(none, None);
    assert_eq!(
        (
            expired.state,
            expired.key,
            expired.tries,
            expired.next_try_at
        ),
        (CallbackState::Expired, None, 0, None)
    );
    let keyless = CallbackRecord {
        key: None,
        ..pending()
    };
    let (expired, none) = keyless.claimed(&by, at(110));
    assert_eq!((expired.state, none), (CallbackState::Expired, None));
}

/// Asserts each answer: a 2xx delivers at its time and a 410 abandons, each erasing the key
/// and the claim; a failure keeps its class, frees the claim and is due again after the wait,
/// cut to the deadline, and a failure at the deadline expires the row. Catches an answer that
/// keeps the key after the delivery ended, a retry past the deadline, and a failure that keeps
/// the claim (so no other instance could take the row).
#[test]
fn each_answer_moves_the_row() {
    let by = InstanceId::new("server-a");
    let (held, _) = pending().claimed(&by, at(100));
    let delivered = held.clone().answered(CallbackAnswer::Delivered, at(101));
    assert_eq!(
        (
            delivered.state,
            delivered.delivered_at,
            delivered.key.clone(),
            delivered.claimed_by.clone()
        ),
        (CallbackState::Delivered, Some(at(101)), None, None)
    );
    assert_eq!(delivered.due_at(), None);
    let abandoned = held.clone().answered(CallbackAnswer::Abandoned, at(101));
    assert_eq!(
        (abandoned.state, abandoned.delivered_at, abandoned.key),
        (CallbackState::Abandoned, None, None)
    );
    let failed = held.clone().answered(
        CallbackAnswer::Failed(FailureClass::RefusedAddress),
        at(101),
    );
    assert_eq!(
        (
            failed.state,
            failed.next_try_at,
            failed.last_failure,
            failed.claimed_by.clone(),
            failed.claim_until,
            failed.key.is_some()
        ),
        (
            CallbackState::Pending,
            Some(at(106)),
            Some(FailureClass::RefusedAddress),
            None,
            None,
            true
        )
    );
    let cut = held
        .clone()
        .answered(CallbackAnswer::Failed(FailureClass::Failed), at(158));
    assert_eq!(cut.next_try_at, Some(at(160)));
    let late = held.answered(CallbackAnswer::Failed(FailureClass::Failed), at(160));
    assert_eq!(
        (late.state, late.key, late.last_failure),
        (CallbackState::Expired, None, Some(FailureClass::Failed))
    );
    let undated = CallbackRecord {
        deadline_at: None,
        ..pending()
    };
    let failed = undated.answered(CallbackAnswer::Failed(FailureClass::Failed), at(101));
    assert_eq!(failed.next_try_at, Some(at(106)));
}

/// Asserts the boot's raise: a pending row's deadline below the floor is put at the floor; a
/// later deadline, an armed row and a delivered one are left. Catches a raise that lowers a
/// deadline, or that gives a row whose delivery ended a deadline again.
#[test]
fn a_boot_raises_only_a_pending_deadline_below_it() {
    let raised = pending().raised_at_boot(at(500)).expect("raised");
    assert_eq!(raised.deadline_at, Some(at(500)));
    assert_eq!(
        CallbackRecord {
            deadline_at: pending().deadline_at,
            ..raised
        },
        pending()
    );
    assert_eq!(pending().raised_at_boot(at(160)), None);
    assert_eq!(armed(Some(60)).raised_at_boot(at(500)), None);
    let delivered = pending().answered(CallbackAnswer::Delivered, at(101));
    assert_eq!(delivered.raised_at_boot(at(500)), None);
}

/// Asserts that a row's status is its state, tries, last try, last failure, delivery time and
/// deadline. Catches a status that shows another field in place of one.
#[test]
fn a_status_reads_the_row() {
    let (held, _) = pending().claimed(&InstanceId::new("server-a"), at(110));
    let failed = held.answered(CallbackAnswer::Failed(FailureClass::Failed), at(111));
    assert_eq!(
        failed.status(),
        CallbackStatus {
            state: CallbackState::Pending,
            tries: 1,
            last_try_at: Some(at(110)),
            last_failure: Some(FailureClass::Failed),
            delivered_at: None,
            deadline_at: Some(at(160)),
        }
    );
}
