//! The state directory across releases, run against two earlier releases' own state
//! readers, each the one reader that refuses data it does not know:
//!
//! - `previous_release/ledger.rs` is v0.1.128's `src/ledger.rs` (the release before the
//!   pool), byte for byte (pinned by its SHA-256 below), compiled here where `crate::lease`
//!   is what it was built against;
//! - `previous_release/pool_ledger.rs` and `pool_ledger/pool.rs` are the ledger of the
//!   release with the pool, the one before this one (`ledger.rs` and `ledger/pool.rs` as
//!   that release shipped them, pinned by their SHA-256 below). That ledger names the pool's
//!   slots with a type of this crate's `lease` module that this release no longer has, so
//!   the vendored files differ from what shipped in the lines named in [`ShippedAs`] and
//!   nowhere else (the test that pins them undoes those lines before it hashes).
//!
//! The ledger file is the one format every release reads and writes (`created`, `deleted`).
//! The pool file (`pool`) is the pool release's alone: this release never writes or reads
//! it, and its boot reset removes it before it deletes the pool's accounts, so a rollback to
//! the pool release finds no pool and makes its own afresh. This release adds one file
//! beside the ledger, the GUI teardown record, which no release lists the state directory
//! for.

use sha2::Digest as _;

use super::*;

mod ledger;
#[allow(
    dead_code,
    reason = "the pool release's whole ledger is compiled as it shipped; the tests use part of it"
)]
mod pool_ledger;

/// What the pool release's ledger takes from this crate's `lease` module: the lease id's
/// text form and the pool's slot, as that release defined them.
mod lease {
    pub(super) use crate::lease::{Dotted, parse_lease};

    /// A slot of the pool, as the pool release's `lease::Slot`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) struct Slot(u32);

    impl Slot {
        pub(super) const fn new(index: u32) -> Self {
            Self(index)
        }
    }

    impl std::fmt::Display for Slot {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::str::FromStr for Slot {
        type Err = String;

        fn from_str(text: &str) -> Result<Self, String> {
            let canonical = !text.is_empty()
                && text.bytes().all(|b| b.is_ascii_digit())
                && (text == "0" || !text.starts_with('0'));
            canonical
                .then(|| text.parse().ok())
                .flatten()
                .map(Self)
                .ok_or_else(|| format!("pool slot {text:?} is not a number"))
        }
    }
}

/// The SHA-256 of v0.1.128's `crates/js-mac-session/src/ledger.rs`.
const LEDGER_SHA256: &str = "b1f2af0a8a46330ccd6a8e779037adb283bc89d5a5c82026171211666c530fd0";

/// The SHA-256 of the pool release's `crates/js-mac-session/src/ledger.rs`.
const POOL_LEDGER_SHA256: &str = "8f5c03b7eb6a05c8e4641362f6538cdfea250d85d772979d8baf3e4eae54d571";

/// The SHA-256 of the pool release's `crates/js-mac-session/src/ledger/pool.rs`.
const POOL_FILE_SHA256: &str = "2452820992a28e9fc1810bbb1e5bd38e9a47d6a4f0bd0aa39346133192bb3da8";

/// The line of each vendored file of the pool release that names where it takes the lease
/// id's text form and the pool's slot from: this module's `lease`, where the release had
/// `crate::lease`. (A `crate::` path, so the line sorts where the shipped one did and
/// `cargo fmt` leaves the file as it is.)
const VENDORED_USE: &str =
    "use crate::helper::tests::previous_release::lease::{Dotted, Slot, parse_lease};";

/// How a vendored file of the pool release differs from what shipped: the line it took its
/// `lease` items from, and (for the pool file) its test module's declaration, whose file is
/// not vendored.
struct ShippedAs {
    /// The line as the vendored file has it.
    vendored: &'static str,
    /// The line as it shipped.
    shipped: &'static str,
    /// What the shipped file ends with, which the vendored one leaves off.
    tail: &'static str,
}

fn sha256(text: &str) -> String {
    let digest = sha2::Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Catches a fixture edited away from the release it stands for: the vendored ledger is
/// the previous release's, unchanged.
#[test]
fn the_previous_releases_ledger_is_the_one_it_shipped() {
    let bytes = include_bytes!("previous_release/ledger.rs");
    let digest = sha2::Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, LEDGER_SHA256);
}

/// Catches a fixture edited away from the pool release it stands for: its ledger and its
/// pool file, each with the changes [`ShippedAs`] names undone, are the files that shipped,
/// byte for byte.
#[test]
fn the_pool_releases_ledger_is_the_one_it_shipped() {
    let files = [
        (
            include_str!("previous_release/pool_ledger.rs"),
            ShippedAs {
                vendored: VENDORED_USE,
                shipped: "use crate::lease::{Dotted, Slot, parse_lease};",
                tail: "",
            },
            POOL_LEDGER_SHA256,
        ),
        (
            include_str!("previous_release/pool_ledger/pool.rs"),
            ShippedAs {
                vendored: VENDORED_USE,
                shipped: "use crate::lease::{Dotted, Slot, parse_lease};",
                tail: "\n#[cfg(test)]\nmod tests;\n",
            },
            POOL_FILE_SHA256,
        ),
    ];
    for (text, edit, sha) in files {
        assert_eq!(text.matches(edit.vendored).count(), 1, "{}", edit.vendored);
        let shipped = text.replace(edit.vendored, edit.shipped) + edit.tail;
        assert_eq!(sha256(&shipped), sha, "{}", edit.shipped);
    }
}

/// Catches a GUI teardown record, or a lease of this release (or anything a failed GUI
/// teardown writes) that the previous release cannot start on: after this release makes an
/// account for a lease and its failed GUI teardown leaves the record, the previous release's
/// ledger opens the state directory's ledger and reads the lease live, its uid held; after
/// this release ends the lease (deleting its account, and discharging the record), it reads
/// the lease deleted and the uid free.
#[test]
fn the_previous_release_reads_this_releases_state() {
    let rig = rig("previous-reads-new", 1);
    let uid = rig.helper.user_create("73.1").unwrap();
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
/// has no GUI teardown record and no pool file at all: a ledger the previous release's
/// own ledger wrote, holding a live lease, is opened by this release, whose boot reset
/// reads the absent record as nothing owed, never asks about `gui/<uid>`, and deletes the
/// lease's own account; and the uid the next lease gets is the one after the old ledger's
/// last, so the two releases choose uids in the same order (the old ledger's last uid
/// is the new one's, and the new ledger's is the old one's).
#[test]
fn this_release_reads_the_previous_releases_state() {
    let dir = std::sync::Arc::new(scratch("helper-new-reads-previous"));
    let uid = me() + 1;
    let mut previous = ledger::Ledger::open(&dir.join("ledger")).unwrap();
    previous.record_created(LeaseId::new(74, 1), uid).unwrap();
    assert_eq!(previous.last_uid(), Some(uid));
    drop(previous);
    let rig = rig_with(
        "new-reads-previous",
        UidRange::new(me(), me() + 2).unwrap(),
        Some(dir),
    );
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(74, 1)]);
    let calls = log(&rig);
    assert!(!calls.iter().any(|c| c.contains("gui/")), "{calls:#?}");
    assert!(calls.contains(&"delete_user kbf-lease-74-1".to_owned()));
    assert!(
        !calls.iter().any(|c| c.starts_with("create_user")),
        "{calls:#?}"
    );
    assert!(!rig.dir.join(format!("gui-teardown.{uid}")).exists());
    // The next lease goes after the previous release's last uid, not back to the first.
    assert_eq!(rig.helper.user_create("74.2"), Ok(uid + 1));
    let previous = ledger::Ledger::open(&rig.dir.join("ledger")).unwrap();
    assert_eq!(previous.last_uid(), Some(uid + 1));
    assert_eq!(
        previous.get(LeaseId::new(74, 1)),
        Some(ledger::Entry { uid, deleted: true })
    );
}

/// Catches a rollback to the pool release that cannot start on, or reads wrongly, the
/// state this release leaves: the leases of per-job accounts (live, and deleted), and no
/// pool at all (the pool file is gone, so the old release makes its pool afresh and never
/// finds a slot for a lease that was not lent one). The state is the pool release's own,
/// left by its pool and a lease it had lent a slot; this release's boot reset ends it, and
/// its own leases come after.
#[test]
fn the_pool_release_opens_the_state_this_release_leaves() {
    let dir = std::sync::Arc::new(scratch("helper-pool-release-reads-new"));
    let uid = me();
    {
        let mut pool = pool_ledger::Ledger::open(&dir.join("ledger")).unwrap();
        let slot = lease::Slot::new(0);
        pool.record_slot(slot, uid).unwrap();
        pool.record_free(slot).unwrap();
        pool.record_leased(slot, LeaseId::new(75, 1)).unwrap();
    }
    assert!(dir.join("pool").exists());
    let rig = rig_with(
        "pool-release-reads-new",
        UidRange::new(uid, uid + 1).unwrap(),
        Some(dir),
    );
    rig.host.state().users.insert(
        "kbf-lease-pool-0".to_owned(),
        NewUser {
            name: "kbf-lease-pool-0".to_owned(),
            uid,
            gid: my_gid(),
            home: rig.dir.join("Users/kbf-lease-pool-0"),
        },
    );
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(75, 1)]);
    assert!(!rig.dir.join("pool").exists());
    assert_eq!(rig.helper.user_create("75.2"), Ok(uid + 1));
    assert_eq!(rig.helper.user_create("75.3"), Ok(uid));
    assert_eq!(rig.helper.user_delete("75.2"), Ok(true));

    let previous = pool_ledger::Ledger::open(&rig.dir.join("ledger")).unwrap();
    let entry = |sequence| previous.get(LeaseId::new(75, sequence));
    assert_eq!(entry(1), Some(pool_ledger::Entry { uid, deleted: true }));
    assert_eq!(
        entry(2),
        Some(pool_ledger::Entry {
            uid: uid + 1,
            deleted: true
        })
    );
    assert_eq!(
        entry(3),
        Some(pool_ledger::Entry {
            uid,
            deleted: false
        })
    );
    assert_eq!(previous.live_leases(), [LeaseId::new(75, 3)]);
    assert!(previous.held_uids().contains(&uid));
    // No slot: the pool release makes its pool afresh, and no lease is leased a slot.
    assert!(previous.slots().is_empty());
    assert_eq!(previous.slot_of(LeaseId::new(75, 3)), None);
}

/// Catches a release that cannot take over the state the pool release left (its ledger and
/// its pool file, the pool's accounts in the directory, a lease lent a slot): the boot reset
/// of this release, which holds Full Disk Access, removes the pool file before it deletes
/// the first of the pool's accounts (so a rollback cannot find a pool of slots whose
/// accounts are gone), deletes every account of the pool and ends the lease lent a slot, the
/// uids of all freed: each lease it ends, and no one it keeps. A pool account whose uid is
/// outside the range is not touched, and without the grant none of it runs.
#[test]
fn this_release_boot_resets_the_state_the_pool_release_wrote() {
    let dir = std::sync::Arc::new(scratch("helper-new-reads-pool-release"));
    let uid = me();
    {
        let mut pool = pool_ledger::Ledger::open(&dir.join("ledger")).unwrap();
        for n in 0..3 {
            let slot = lease::Slot::new(n);
            pool.record_slot(slot, uid + n).unwrap();
            pool.record_free(slot).unwrap();
        }
        pool.record_leased(lease::Slot::new(0), LeaseId::new(76, 1))
            .unwrap();
    }
    let range = UidRange::new(uid, uid + 3).unwrap();
    let account = |name: &str, uid: u32, dir: &Path| NewUser {
        name: name.to_owned(),
        uid,
        gid: my_gid(),
        home: dir.join("Users").join(name),
    };
    let boot = |without_grant: bool| {
        let rig = rig_with("new-reads-pool-release", range, Some(dir.clone()));
        for n in 0..3 {
            let name = format!("kbf-lease-pool-{n}");
            let user = account(&name, uid + n, &rig.dir);
            rig.host.state().users.insert(name, user);
        }
        // A pool account outside the range: never the helper's to delete.
        let outside = account("kbf-lease-pool-9", uid + 90, &rig.dir);
        rig.host
            .state()
            .users
            .insert("kbf-lease-pool-9".to_owned(), outside);
        if without_grant {
            rig.host.state().privacy_dir = Some(io::ErrorKind::PermissionDenied);
        }
        rig
    };

    // Without the grant nothing of the pool is touched, and the pool file stays. The lease
    // lent a slot cannot be shown gone while the pool's record has its uid, so it is held.
    let rig = boot(true);
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, Vec::<LeaseId>::new());
    assert_eq!(reset.held, [LeaseId::new(76, 1)]);
    assert!(dir.join("pool").exists());
    assert_eq!(rig.host.state().users.len(), 4);
    drop(rig);

    // With it, the pool file goes before the first account does.
    let rig = boot(false);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (pool_file, found) = (dir.join("pool"), seen.clone());
    rig.host.state().after_delete = Some(Box::new(move |_| {
        found.lock().unwrap().push(pool_file.exists());
    }));
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.kept, None);
    assert_eq!(reset.deleted, [LeaseId::new(76, 1)]);
    assert_eq!(reset.held, Vec::<LeaseId>::new());
    assert_eq!(*seen.lock().unwrap(), [false, false, false]);
    assert!(!dir.join("pool").exists());
    let names: Vec<String> = rig.host.state().users.keys().cloned().collect();
    assert_eq!(names, ["kbf-lease-pool-9"]);
}
