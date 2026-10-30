//! Hardening: a panicking source, concurrent ensures against the budget, evictions that
//! fail, trees that are not current, a second open of one directory, pins and modes.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::common::{Gate, Rig, run, sized, wanted};
use js_treestore::{Config, Error, Step, TreeStore};

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn a_panicking_source_leaves_no_flight_and_nothing_on_disk() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    rig.source.0.panic_fetches.store(1, Ordering::SeqCst);
    let store = rig.open(rig.cfg());
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| store.ensure(&wanted(&l))));
    assert!(outcome.is_err());
    assert_eq!(store.stats().in_flight, 0);
    assert!(rig.names("tmp").is_empty());
    assert!(rig.names("stage").is_empty());
    // A later caller for the same blob does not wait.
    store.ensure(&wanted(&l)).unwrap();
}

#[test]
#[ignore = "blocks forever against the store before the flight guard"]
fn a_waiting_caller_retries_after_the_leader_panics() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let gate = rig.source.hold_fetches();
    rig.source.0.panic_fetches.store(1, Ordering::SeqCst);
    let store = rig.open(rig.cfg());
    let want = wanted(&l);
    thread::scope(|scope| {
        let first = scope.spawn(|| store.ensure(&want));
        rig.source.wait_entered(1);
        let second = scope.spawn(|| store.ensure(&want));
        while store.stats().waiting < 1 {
            thread::yield_now();
        }
        gate.open();
        assert!(first.join().is_err());
        assert!(second.join().unwrap().is_ok());
    });
    assert_eq!(store.stats().in_flight, 0);
}

#[test]
fn concurrent_ensures_of_two_trees_hold_the_budget() {
    let rig = Rig::new();
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    let held = Gate::closed();
    let reached = Arc::new(AtomicBool::new(false));
    let first = Arc::new(AtomicBool::new(true));
    let (hold, flag, once) = (held.clone(), reached.clone(), first.clone());
    let cfg = Config {
        budget_bytes: 150,
        crash: Some(Arc::new(move |step| {
            // The first tree to be placed waits there, before its bytes are in the table.
            if step == Step::Renamed && once.swap(false, Ordering::SeqCst) {
                flag.store(true, Ordering::SeqCst);
                hold.wait();
            }
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let (wa, wb) = (wanted(&a), wanted(&b));
    thread::scope(|scope| {
        let x = scope.spawn(|| store.ensure(&wa));
        while !reached.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        // The other tree does not fit beside the one being placed.
        let other = store.ensure(&wb);
        held.open();
        x.join().unwrap().unwrap();
        assert!(matches!(other, Err(Error::NoSpace { .. })));
    });
    assert!(store.stats().used <= 150);
    assert!(rig.present(&a.diff_id));
    assert!(!rig.present(&b.diff_id));
}

#[test]
fn an_eviction_that_cannot_move_the_tree_keeps_it_counted_and_is_an_error() {
    let rig = Rig::new();
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    let store = rig.open(Config {
        budget_bytes: 100,
        ..rig.cfg()
    });
    store.ensure(&wanted(&a)).unwrap();
    std::fs::remove_dir(rig.root().join("trash")).unwrap();
    assert!(store.ensure(&wanted(&b)).is_err());
    assert!(rig.present(&a.diff_id));
    assert!(!rig.present(&b.diff_id));
    assert_eq!(store.stats().trees, 1);
    assert_eq!(store.stats().used, 100);
}

#[test]
fn a_stale_tree_that_could_not_be_trashed_is_not_relabelled_current() {
    let rig = Rig::new();
    let a = sized("f", 10, 1);
    rig.have(&a);
    let first = rig.open(rig.cfg());
    first.ensure(&wanted(&a)).unwrap();
    drop(first);
    let mark = rig.root().join("trees").join(a.diff_id.hex()).join("OLD");
    std::fs::write(&mark, b"made by the old extractor").unwrap();
    let store = rig.open(Config {
        extractor_version: 2,
        ..rig.cfg()
    });
    let trash = rig.root().join("trash");
    std::fs::remove_dir(&trash).unwrap();
    assert!(store.ensure(&wanted(&a)).is_err());
    std::fs::create_dir(&trash).unwrap();
    store.ensure(&wanted(&a)).unwrap();
    assert!(!mark.exists());
    let record = std::fs::read_to_string(
        rig.root()
            .join("trees")
            .join(format!("{}.sealed", a.diff_id.hex())),
    )
    .unwrap();
    assert!(record.contains("\"extractor_version\":2"));
}

#[test]
fn a_second_open_of_a_directory_is_refused_and_takes_nothing_from_the_first() {
    let rig = Rig::new();
    let a = sized("f", 10, 1);
    rig.have(&a);
    let refused = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let (source, sealer, root) = (rig.source.clone(), rig.sealer.clone(), rig.root());
    let plain = rig.cfg();
    let (seen, flag) = (refused.clone(), done.clone());
    let cfg = Config {
        crash: Some(Arc::new(move |step| {
            if step == Step::Renamed && !flag.swap(true, Ordering::SeqCst) {
                let second = TreeStore::open(
                    &root,
                    source.clone(),
                    sealer.clone(),
                    plain.clone(),
                    &BTreeSet::new(),
                );
                *seen.lock().unwrap() = Some(second.is_err());
            }
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let ready = store.ensure(&wanted(&a)).unwrap();
    assert_eq!(*refused.lock().unwrap(), Some(true));
    assert!(ready.path.is_dir());
    assert!(rig.present(&a.diff_id));
}

#[test]
fn pin_directories_of_trees_that_are_gone_are_swept_at_open() {
    let rig = Rig::new();
    let gone = sized("f", 10, 1);
    let first = rig.open(rig.cfg());
    drop(first);
    let dir = rig.root().join("pins").join(gone.diff_id.hex());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("r1"), b"").unwrap();
    let _store = rig.open_live(rig.cfg(), &BTreeSet::from([run("r1")]));
    assert!(rig.names("pins").is_empty());
}

#[test]
fn entries_have_the_modes_the_design_states() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (root, sink) = (rig.root(), seen.clone());
    let cfg = Config {
        crash: Some(Arc::new(move |step| {
            if step == Step::Staged {
                let stage = std::fs::read_dir(root.join("stage")).unwrap().next();
                sink.lock()
                    .unwrap()
                    .push(mode(&stage.unwrap().unwrap().path()));
            }
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    store.ensure(&wanted(&l)).unwrap();
    store.pin(&l.diff_id, &run("r1")).unwrap();
    let root = rig.root();
    assert_eq!(*seen.lock().unwrap(), [0o700]);
    assert_eq!(mode(&root.join("trees")), 0o711);
    for sub in ["tmp", "stage", "trash", "pins"] {
        assert_eq!(mode(&root.join(sub)), 0o700, "{sub}");
    }
    assert_eq!(mode(&root.join("pins").join(l.diff_id.hex())), 0o700);
    let record = root
        .join("trees")
        .join(format!("{}.sealed", l.diff_id.hex()));
    assert_eq!(mode(&record), 0o600);
    assert_eq!(mode(&root.join("lock")), 0o600);
}

#[test]
fn a_record_is_written_only_after_the_seal_returned_synced() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (sink, syncs, root, diff) = (
        seen.clone(),
        rig.sealer.syncs.clone(),
        rig.root(),
        l.diff_id.clone(),
    );
    let cfg = Config {
        crash: Some(Arc::new(move |step| {
            let record = root
                .join("trees")
                .join(format!("{}.sealed", diff.hex()))
                .exists();
            sink.lock()
                .unwrap()
                .push((step, syncs.load(Ordering::SeqCst), record));
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    store.ensure(&wanted(&l)).unwrap();
    let seen = seen.lock().unwrap();
    let find = |step: Step| *seen.iter().find(|(s, ..)| *s == step).unwrap();
    assert_eq!(find(Step::Unpacked), (Step::Unpacked, 0, false));
    assert_eq!(find(Step::Sealed), (Step::Sealed, 1, false));
    assert_eq!(find(Step::Renamed), (Step::Renamed, 1, false));
    assert_eq!(find(Step::Recorded), (Step::Recorded, 1, true));
}

#[test]
fn ensure_pinned_pins_the_tree_before_it_is_returned_and_on_a_hit() {
    let rig = Rig::new();
    let l = sized("f", 100, 1);
    rig.have(&l);
    let store = rig.open(Config {
        budget_bytes: 100,
        ..rig.cfg()
    });
    let handle = store.ensure_pinned(&wanted(&l), &run("r1")).unwrap();
    assert!(handle.path().join("f").is_file());
    assert_eq!(store.stats().pinned, 1);
    // A hit pins too, and the same run cannot pin twice.
    let second = store.ensure_pinned(&wanted(&l), &run("r2")).unwrap();
    assert!(matches!(
        store.ensure_pinned(&wanted(&l), &run("r2")),
        Err(Error::AlreadyPinned)
    ));
    // Pinned: another tree finds no room.
    let other = sized("f", 100, 2);
    rig.have(&other);
    assert!(matches!(
        store.ensure(&wanted(&other)),
        Err(Error::NoSpace { .. })
    ));
    store.unpin(handle);
    store.unpin(second);
    store.ensure(&wanted(&other)).unwrap();
}

#[test]
fn a_tree_that_cannot_fit_the_budget_is_refused_before_the_fetch() {
    let rig = Rig::new();
    let l = sized("f", 100, 1);
    rig.have(&l);
    let store = rig.open(Config {
        budget_bytes: 500,
        ..rig.cfg()
    });
    let mut want = wanted(&l);
    want.expected_bytes = Some(501);
    assert!(matches!(
        store.ensure(&want),
        Err(Error::NoSpace {
            needed: 501,
            budget: 500,
            ..
        })
    ));
    assert_eq!(rig.source.fetches(), 0);
    assert_eq!(store.stats().in_flight, 0);
    want.expected_bytes = Some(500);
    store.ensure(&want).unwrap();
}

#[test]
fn a_tree_being_deleted_cannot_be_pinned_and_a_caller_for_it_waits_then_rebuilds() {
    let rig = Rig::new();
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    let store = rig.open(Config {
        budget_bytes: 100,
        ..rig.cfg()
    });
    store.ensure(&wanted(&a)).unwrap();
    let gate = Gate::closed();
    *rig.sealer.hold_unseal.lock().unwrap() = Some(gate.clone());
    thread::scope(|scope| {
        // Making room for b deletes a, and the deletion waits in the sealer.
        let evicting = scope.spawn(|| store.ensure(&wanted(&b)));
        while rig.sealer.unsealing.load(Ordering::SeqCst) < 1 {
            thread::yield_now();
        }
        assert!(matches!(
            store.pin(&a.diff_id, &run("r1")),
            Err(Error::Absent(_))
        ));
        let wanting = scope.spawn(|| store.ensure(&wanted(&a)));
        while store.stats().waiting < 1 {
            thread::yield_now();
        }
        *rig.sealer.hold_unseal.lock().unwrap() = None;
        gate.open();
        evicting.join().unwrap().unwrap();
        // a is rebuilt for its caller (and b is evicted for it in turn).
        wanting.join().unwrap().unwrap();
    });
    assert!(rig.present(&a.diff_id));
    assert!(store.stats().used <= 100);
}

#[test]
fn a_second_extraction_of_a_tree_waits_for_the_placement_in_progress() {
    let rig = Rig::new();
    let files: &[(&str, &[u8])] = &[("f", b"same content")];
    let zstd = crate::common::layer(files);
    let tar = crate::common::tar_layer(files);
    rig.have(&zstd);
    rig.have(&tar);
    let held = Gate::closed();
    let reached = Arc::new(AtomicBool::new(false));
    let first = Arc::new(AtomicBool::new(true));
    let (hold, flag, once) = (held.clone(), reached.clone(), first.clone());
    let cfg = Config {
        crash: Some(Arc::new(move |step| {
            if step == Step::Renamed && once.swap(false, Ordering::SeqCst) {
                flag.store(true, Ordering::SeqCst);
                hold.wait();
            }
            false
        })),
        ..rig.cfg()
    };
    let store = rig.open(cfg);
    let (a, b) = (wanted(&zstd), wanted(&tar));
    thread::scope(|scope| {
        let x = scope.spawn(|| store.ensure(&a));
        while !reached.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        let y = scope.spawn(|| store.ensure_pinned(&b, &run("r1")));
        while store.stats().waiting < 1 {
            thread::yield_now();
        }
        held.open();
        x.join().unwrap().unwrap();
        // The second finds the first's tree current, pins it and discards its own stage.
        assert!(y.join().unwrap().is_ok());
    });
    assert_eq!(rig.sealer.seal_count(), 2);
    assert_eq!(store.stats().trees, 1);
    assert_eq!(store.stats().pinned, 1);
    assert!(rig.names("stage").is_empty());
}

#[test]
fn a_tree_in_place_that_the_store_does_not_hold_is_not_taken_for_the_new_content() {
    let rig = Rig::new();
    let l = sized("f", 10, 1);
    rig.have(&l);
    let store = rig.open(rig.cfg());
    let squatter = rig.root().join("trees").join(l.diff_id.hex());
    std::fs::create_dir_all(&squatter).unwrap();
    std::fs::write(squatter.join("other"), b"not ours").unwrap();
    assert!(matches!(store.ensure(&wanted(&l)), Err(Error::Conflict(_))));
    assert!(squatter.join("other").is_file());
    assert!(
        !rig.root()
            .join("trees")
            .join(format!("{}.sealed", l.diff_id.hex()))
            .exists()
    );
    assert!(rig.names("stage").is_empty());
}
