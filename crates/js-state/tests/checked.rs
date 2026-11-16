//! `Checked`: a store whose writing transactions also check one key.

use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::{Checked, Claim, Cond, ManualClock, Store, Txn, Version};

fn fake() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn version_of(store: &dyn Store, key: &str) -> Version {
    store.get(key).expect("reads").expect("a row").version
}

// A writing transaction commits while the key holds the version the store was made with, and
// conflicts on the key, writing nothing, once another writer has moved it: a put, a delete
// and a counter each. Reads and the empty transaction are not checked. It catches a store
// that lets a writer whose claim is gone commit, and one that checks a read.
#[test]
fn a_writing_transaction_commits_only_while_the_checked_key_holds() {
    let inner = fake();
    inner
        .txn(Txn::new().put("node/h", "1", Cond::Any))
        .expect("reads")
        .expect("commits");
    let held = version_of(&*inner, "node/h");
    let checked = Checked::new(Arc::clone(&inner), "node/h", Cond::Version(held));

    checked
        .txn(Txn::new().put("job/a", "x", Cond::Absent))
        .expect("answers")
        .expect("the claim holds");
    assert!(
        checked.txn(Txn::new()).expect("answers").is_ok(),
        "an empty transaction"
    );

    // Another connection takes the host.
    inner
        .txn(Txn::new().put("node/h", "2", Cond::Any))
        .expect("reads")
        .expect("commits");
    for (label, txn) in [
        ("a put", Txn::new().put("job/b", "x", Cond::Absent)),
        ("a delete", Txn::new().delete("job/a", Cond::Any)),
        ("a counter", Txn::new().next("n")),
    ] {
        let lost = checked.txn(txn).expect("answers");
        let conflict = lost.expect_err(label);
        assert_eq!(conflict.key, "node/h", "{label}");
    }
    assert!(
        checked.get("job/b").expect("reads").is_none(),
        "a refused put wrote"
    );
    assert!(
        checked.get("job/a").expect("reads").is_some(),
        "a refused delete deleted"
    );
    assert!(checked.txn(Txn::new()).expect("answers").is_ok());
    assert!(checked.scan_prefix("job/").expect("reads").len() == 1);
    assert!(checked.head().expect("reads").is_some());
}

// A decision made on locked rows is checked too: its transaction conflicts once the key moved.
#[test]
fn a_claimed_decision_is_checked_too() {
    let inner = fake();
    inner
        .txn(
            Txn::new()
                .put("node/h", "1", Cond::Any)
                .put("q/a", "queued", Cond::Any),
        )
        .expect("reads")
        .expect("commits");
    let held = version_of(&*inner, "node/h");
    let checked = Checked::new(Arc::clone(&inner), "node/h", Cond::Version(held));
    let claim = ["q/a".to_owned()];
    let placed = checked
        .claim_decide(&claim, &mut |_| {
            Some(Txn::new().put("p/a", "1", Cond::Absent))
        })
        .expect("answers");
    assert!(matches!(placed, Claim::Committed(_)), "{placed:?}");
    inner
        .txn(Txn::new().put("node/h", "2", Cond::Any))
        .expect("reads")
        .expect("commits");
    let lost = checked
        .claim_decide(&claim, &mut |_| {
            Some(Txn::new().put("p/b", "1", Cond::Absent))
        })
        .expect("answers");
    assert!(
        matches!(lost, Claim::Conflict(ref c) if c.key == "node/h"),
        "{lost:?}"
    );
    assert!(inner.get("p/b").expect("reads").is_none());
}

// Every other call of the store goes to the store it holds, as it is: ranges and the feed, its
// trim, the reaper, the counters, the time, and the head. It catches a delegate that answers
// for another call.
#[test]
fn every_other_call_goes_to_the_store_it_holds() {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use js_state::{Revision, ScanRange};

    let clock = Arc::new(ManualClock::default());
    let inner = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    inner
        .txn(
            Txn::new()
                .put("node/h", "1", Cond::Any)
                .put("a/1", "x", Cond::Any)
                .put_ttl("a/2", "y", Duration::from_secs(1), Cond::Any),
        )
        .unwrap()
        .unwrap();
    let held = version_of(&*inner, "node/h");
    let checked = Checked::new(Arc::clone(&inner), "node/h", Cond::Version(held));
    let limit = NonZeroUsize::new(10).unwrap();
    let page = checked.scan_range(ScanRange::new("a/", limit)).unwrap();
    assert_eq!(page.rows.len(), 2);
    let feed = checked.changes(Revision::ZERO, 10).unwrap();
    assert_eq!(feed.rows.len(), 3);
    assert_eq!(checked.commit_stats(), inner.commit_stats());
    assert_eq!(checked.now(), inner.now());
    clock.advance(Duration::from_secs(2));
    assert_eq!(checked.reap_expired().unwrap(), 1);
    assert_eq!(checked.trim_feed(Revision(1)).unwrap(), 1);
}
