use super::super::Ledger;
use super::*;

fn dir(name: &str) -> js_testkit::dir::TestDir {
    js_testkit::dir::TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-session-pool-{name}"),
    )
}

fn slot(n: u32) -> Slot {
    Slot::new(n)
}

fn state(ledger: &Ledger, n: u32) -> Option<SlotState> {
    ledger.slot(slot(n)).map(|entry| entry.state)
}

/// Catches: a slot's state kept only in memory, under which a restart forgets which
/// user is lent to which lease, or that it was broken; and a lease and its slot written
/// in the wrong order (a crash between the two must never leave a live lease without
/// its slot).
#[test]
fn every_slot_state_survives_reopening() {
    let dir = dir("reopen");
    let path = dir.join("ledger");
    let mut ledger = Ledger::open(&path).unwrap();
    let (a, b) = (LeaseId::new(9, 1), LeaseId::new(9, 2));
    for n in 0..4 {
        ledger.record_slot(slot(n), 600 + n).unwrap();
    }
    assert_eq!(state(&ledger, 0), Some(SlotState::Resetting));
    for n in 0..3 {
        ledger.record_free(slot(n)).unwrap();
    }
    ledger.record_broken(slot(3)).unwrap();
    ledger.record_leased(slot(0), a, false).unwrap();
    ledger.record_leased(slot(1), b, true).unwrap();
    ledger.record_released(slot(0), a).unwrap();
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(dir.join("pool")).unwrap(),
        "made 0 600\nmade 1 601\nmade 2 602\nmade 3 603\nfree 0\nfree 1\nfree 2\n\
         broken 3\nleased 0 9.1\nleased 1 9.2 admin\nresetting 0\n"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "created 9.1 600\ncreated 9.2 601\ndeleted 9.1\n"
    );
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.slots(),
        [
            (
                slot(0),
                SlotEntry {
                    uid: 600,
                    state: SlotState::Resetting
                }
            ),
            (
                slot(1),
                SlotEntry {
                    uid: 601,
                    state: SlotState::Leased {
                        lease: b,
                        admin: true
                    }
                }
            ),
            (
                slot(2),
                SlotEntry {
                    uid: 602,
                    state: SlotState::Free
                }
            ),
            (
                slot(3),
                SlotEntry {
                    uid: 603,
                    state: SlotState::Broken
                }
            ),
        ]
    );
    assert_eq!(ledger.slot_of(b), Some(slot(1)));
    assert_eq!(ledger.slot_of(a), None);
    assert_eq!(ledger.slot(slot(9)), None);
}

/// Catches: a record that does not follow from its slot's state accepted (a slot lent
/// twice, a lease on two slots, a uid on two slots, a slot freed that was never reset),
/// in memory or on reopening, and a refused record that still reached the file.
#[test]
fn a_record_must_follow_from_its_slot_s_state() {
    let dir = dir("admit");
    let path = dir.join("ledger");
    let mut ledger = Ledger::open(&path).unwrap();
    let (a, b) = (LeaseId::new(8, 1), LeaseId::new(8, 2));
    let refused = |result: io::Result<()>| result.unwrap_err().to_string();
    assert_eq!(
        refused(ledger.record_free(slot(0))),
        "slot 0 was never made"
    );
    ledger.record_slot(slot(0), 600).unwrap();
    assert_eq!(
        refused(ledger.record_slot(slot(0), 601)),
        "slot 0 made twice"
    );
    assert_eq!(
        refused(ledger.record_slot(slot(1), 600)),
        "slot 1's uid 600 is slot 0's"
    );
    assert_eq!(
        refused(ledger.record_leased(slot(0), a, false)),
        "slot 0 is not free"
    );
    assert_eq!(
        refused(ledger.record_orphan_reset(slot(0))),
        "slot 0 is not leased"
    );
    ledger.record_free(slot(0)).unwrap();
    assert_eq!(
        refused(ledger.record_free(slot(0))),
        "slot 0 is neither resetting nor broken"
    );
    assert_eq!(
        refused(ledger.record_broken(slot(0))),
        "slot 0 is not resetting"
    );
    ledger.record_slot(slot(1), 601).unwrap();
    ledger.record_free(slot(1)).unwrap();
    ledger.record_leased(slot(0), a, false).unwrap();
    assert_eq!(
        refused(ledger.record_leased(slot(1), a, false)),
        "lease 8.1 created twice"
    );
    assert_eq!(
        refused(ledger.record_released(slot(1), a)),
        "slot 1 is not leased"
    );
    assert_eq!(
        refused(ledger.record_orphan_reset(slot(0))),
        "slot 0 is leased to lease 8.1, which is live"
    );
    assert_eq!(
        refused(ledger.record_released(slot(0), b)),
        "lease 8.2 deleted but not live"
    );
    ledger.record_leased(slot(1), b, false).unwrap();
    assert_eq!(
        refused(ledger.record_released(slot(0), b)),
        "slot 0 is leased to lease 8.1, not 8.2"
    );
    // Nothing refused was written.
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(dir.join("pool")).unwrap(),
        "made 0 600\nfree 0\nmade 1 601\nfree 1\nleased 0 8.1\nleased 1 8.2\n"
    );
    // A lease is on one slot only, also in a file written by hand.
    for (bad, why) in [
        (
            "made 0 600\nfree 0\nmade 1 601\nfree 1\nleased 0 8.1\nleased 1 8.1\n",
            "lease 8.1 holds slot 0 already",
        ),
        ("made 0 600\nresetting 0\n", "slot 0 is not leased"),
        (
            "made 0 600\nfree 0\nfree 0\n",
            "slot 0 is neither resetting nor broken",
        ),
        (
            "made 0 600\nfree 0\nleased 0 8.1\nbroken 0\n",
            "slot 0 is not resetting",
        ),
        ("made 0 600\nleased 0 8.1\n", "slot 0 is not free"),
        ("made 0 600\nmade 0 601\n", "slot 0 made twice"),
        ("free 3\n", "slot 3 was never made"),
        ("made 0 six\n", "bad uid \"six\""),
        ("made 00 600\n", "pool slot \"00\" is not a number"),
        ("leased 0 8\n", "lease id \"8\" is not <term>.<seq>"),
        (
            "leased 0 8.1 root\n",
            "unknown record \"leased 0 8.1 root\"",
        ),
        ("emptied 0\n", "unknown record \"emptied 0\""),
    ] {
        std::fs::write(dir.join("pool"), bad).unwrap();
        std::fs::write(&path, "").unwrap();
        let error = Ledger::open(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bad:?}");
        assert!(error.to_string().ends_with(why), "{bad:?}: {error}");
        assert!(
            error.to_string().contains("pool: line "),
            "{bad:?}: {error}"
        );
    }
}

/// Catches: a torn last record of the pool's file kept (glued onto the next one) or
/// read; and the pool's file opened through a link planted at its path.
#[test]
fn a_torn_pool_record_is_cut_and_a_link_refused() {
    let dir = dir("torn");
    let path = dir.join("ledger");
    std::fs::write(dir.join("pool"), "made 0 600\nfree").unwrap();
    let mut ledger = Ledger::open(&path).unwrap();
    assert_eq!(state(&ledger, 0), Some(SlotState::Resetting));
    ledger.record_free(slot(0)).unwrap();
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(dir.join("pool")).unwrap(),
        "made 0 600\nfree 0\n"
    );

    let linked = self::dir("link");
    std::fs::write(linked.join("elsewhere"), "").unwrap();
    std::os::unix::fs::symlink(linked.join("elsewhere"), linked.join("pool")).unwrap();
    assert!(Ledger::open(&linked.join("ledger")).is_err());
}

/// Catches: memory updated although the pool's record never reached the disk.
#[test]
fn a_failed_pool_write_changes_nothing() {
    let dir = dir("fail");
    let path = dir.join("ledger");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.record_slot(slot(0), 600).unwrap();
    ledger.fail_writes(&path, true);
    assert!(ledger.record_free(slot(0)).is_err());
    assert_eq!(state(&ledger, 0), Some(SlotState::Resetting));
}
