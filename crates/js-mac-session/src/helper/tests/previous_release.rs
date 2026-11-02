//! The GUI teardown record across releases, run against the previous release's own
//! state reader: `previous_release/ledger.rs` is v0.1.128's `src/ledger.rs`, byte for
//! byte (pinned by its SHA-256 below), compiled here where `crate::lease` is what it
//! was built against. That ledger is the state the helper reads when it starts, and the
//! one reader that refuses data it does not know; no release lists the state directory,
//! so no release before this one opens the record.

use sha2::Digest as _;

use super::*;

mod ledger;

/// The SHA-256 of v0.1.128's `crates/js-mac-session/src/ledger.rs`.
const LEDGER_SHA256: &str = "b1f2af0a8a46330ccd6a8e779037adb283bc89d5a5c82026171211666c530fd0";

/// Catches a fixture edited away from the release it stands for: the vendored ledger is
/// the previous release's, unchanged.
#[test]
fn the_previous_releases_ledger_is_the_one_it_shipped() {
    let bytes = include_bytes!("previous_release/ledger.rs");
    let digest = sha2::Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, LEDGER_SHA256);
}

/// Catches a GUI teardown record (or anything a failed GUI teardown writes) that the
/// previous release cannot start on: after this release's failed GUI teardown leaves
/// the record, the previous release's ledger opens the state directory's ledger and
/// reads the lease live, its uid held; after this release completes the delete (and
/// discharges the record), it reads the lease deleted and the uid free.
#[test]
fn the_previous_release_reads_this_releases_state() {
    let rig = rig("previous-reads-new", 1);
    let uid = rig.helper.user_create("73.1", None).unwrap();
    {
        let mut state = rig.host.state();
        state.gui.insert(uid);
        state.domains.insert(format!("gui/{uid}"));
        state.fail.insert("bootout gui");
    }
    assert!(rig.helper.kill_uid("73.1").is_err());
    assert!(rig.dir.join(format!("gui-teardown.{uid}")).exists());
    let path = rig.dir.join("ledger");
    let previous = ledger::Ledger::open(&path).unwrap();
    assert_eq!(
        previous.get(LeaseId::new(73, 1)),
        Some(ledger::Entry {
            uid,
            deleted: false
        })
    );
    assert!(previous.held_uids().contains(&uid));
    assert_eq!(previous.live_leases(), [LeaseId::new(73, 1)]);
    drop(previous);

    rig.host.state().fail.clear();
    assert_eq!(rig.helper.user_delete("73.1"), Ok(true));
    assert!(!rig.dir.join(format!("gui-teardown.{uid}")).exists());
    let previous = ledger::Ledger::open(&path).unwrap();
    assert_eq!(
        previous.get(LeaseId::new(73, 1)),
        Some(ledger::Entry { uid, deleted: true })
    );
    assert!(previous.held_uids().is_empty());
    assert!(previous.live_leases().is_empty());
}

/// Catches a release that cannot take over the state the previous release left, which
/// has no GUI teardown record at all: a ledger the previous release's own ledger wrote,
/// holding a live lease, is opened by this release, whose boot reset reads the absent
/// record as nothing owed, never asks about `gui/<uid>`, and deletes the lease.
#[test]
fn this_release_reads_the_previous_releases_state() {
    let dir = std::sync::Arc::new(scratch("helper-new-reads-previous"));
    let uid = me();
    let mut previous = ledger::Ledger::open(&dir.join("ledger")).unwrap();
    previous.record_created(LeaseId::new(74, 1), uid).unwrap();
    drop(previous);
    let rig = rig_with(
        "new-reads-previous",
        UidRange::new(uid, uid).unwrap(),
        Some(dir),
    );
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(74, 1)]);
    let calls = log(&rig);
    assert!(!calls.iter().any(|c| c.contains("gui/")), "{calls:#?}");
    assert!(!rig.dir.join(format!("gui-teardown.{uid}")).exists());
    assert_eq!(rig.helper.user_create("74.2", None), Ok(uid));
}
