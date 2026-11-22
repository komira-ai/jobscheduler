//! Where the bundle store lies, checked at the agent's start: beside the scratch root and not in
//! it, and below only directories a lease user can walk through. Each test says the defect it
//! catches.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use js_agent::SystemCommands;
use js_agent::bundle::{BundleHost, DirFacts, Settings, SystemHost};
use js_agent::{Cli, Platform};

use super::fake::{FakeHost, root_dir};
use crate::support::{Scratch, platform};

const STORE: &str = "/var/db/js-agent/scratch/bundles";
const SCRATCH: &str = "/var/db/js-agent/scratch/runs";

fn mac() -> Platform {
    platform("macos", "aarch64")
}

/// A host with an allow list that holds one digest.
fn host() -> Arc<FakeHost> {
    let host = FakeHost::new();
    host.file("/etc/js/allow", &format!("sha256:{}\n", "a".repeat(64)));
    host
}

/// The flags of a farm host with the store at `store`.
fn from_cli(host: &FakeHost, store: &str) -> Result<Option<Settings>, String> {
    let args = [
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--bundle-store",
        store,
        "--scratch-root",
        SCRATCH,
        "--bundle-budget-gib",
        "40",
        "--bundle-allow-file",
        "/etc/js/allow",
    ];
    Settings::from_cli(&Cli::try_parse_from(args).unwrap(), &mac(), host)
}

/// The directories of the farm layout: every one from `/` to the store root's, the store with
/// the mode the agent makes it and the others 0755.
fn layout() -> BTreeMap<PathBuf, DirFacts> {
    let above = [
        "/",
        "/var",
        "/var/db",
        "/var/db/js-agent",
        "/var/db/js-agent/scratch",
    ];
    above
        .into_iter()
        .map(|dir| (PathBuf::from(dir), root_dir(0o755)))
        .chain([(PathBuf::from(STORE), root_dir(0o711))])
        .collect()
}

/// A store in the scratch root, or the scratch root itself, is refused by name before the host
/// is asked to make or check anything: the agent's start removes every entry of the scratch
/// root that is not a run's directory. A store beside the scratch root whose name only starts
/// with the scratch root's is accepted. Catches a store that is deleted at each start.
#[test]
fn a_store_inside_the_scratch_root_is_refused() {
    for store in [SCRATCH.to_owned(), format!("{SCRATCH}/bundles")] {
        let host = host();
        let why = from_cli(&host, &store).unwrap_err();
        assert_eq!(
            why,
            format!(
                "--bundle-store: {store} is inside --scratch-root {SCRATCH}: the agent's start \
                 removes every entry of the scratch root that is not a run's directory, so the \
                 store would be deleted at each start; put it beside the scratch root, on the \
                 same volume"
            )
        );
        assert!(host.get(|s| s.prepared.is_empty() && s.started.is_empty()));
    }
    let beside = format!("{SCRATCH}-bundles");
    let settings = from_cli(&host(), &beside).unwrap().unwrap();
    assert_eq!(settings.store.to_str(), Some(beside.as_str()));
}

/// The host is asked to prepare exactly the store before the volume checks, and a layout in which
/// every directory from `/` down to the store is root's, closed to writes and searchable by
/// others is accepted. Catches a start that checks a store it never made, and a check that
/// refuses the good layout.
#[test]
fn the_store_is_prepared_before_the_volume_checks_and_a_good_layout_is_accepted() {
    let host = host();
    host.set(|s| s.dirs = Some(layout()));
    assert!(from_cli(&host, STORE).unwrap().is_some());
    assert_eq!(host.get(|s| s.prepared.clone()), vec![PathBuf::from(STORE)]);
    assert_eq!(host.get(|s| s.started.len()), 1);
}

/// A store with a directory above it, or itself, that a lease user cannot search is refused
/// naming the first such directory from the top, with the reason (no `o+x`, not root's, writable
/// by group or others, not a directory, or not read at all), and the volume checks are not
/// reached. Catches a store whose bundles no lease user can run, which shows only as exit 71 at
/// the first job.
#[test]
fn a_store_below_a_directory_a_lease_user_cannot_search_is_refused() {
    let tail = "a lease user could not walk down to the store through it";
    let not_searchable = "cannot be searched by others (no o+x)";
    let other = DirFacts {
        uid: 501,
        ..root_dir(0o755)
    };
    let file = DirFacts {
        is_dir: false,
        ..root_dir(0o711)
    };
    let cases: [(&str, DirFacts, &str); 6] = [
        ("/var/db/js-agent/scratch", root_dir(0o700), not_searchable),
        ("/var/db", other, "is not owned by uid 0"),
        ("/var", root_dir(0o775), "is writable by group or others"),
        ("/var", root_dir(0o757), "is writable by group or others"),
        (STORE, file, "is not a directory"),
        (STORE, root_dir(0o710), not_searchable),
    ];
    for (dir, facts, fault) in cases {
        let host = host();
        let mut dirs = layout();
        dirs.insert(PathBuf::from(dir), facts);
        if dir != STORE {
            // The store is bad too: the first fault from the top is the one named.
            dirs.insert(PathBuf::from(STORE), root_dir(0o700));
        }
        host.set(|s| s.dirs = Some(dirs));
        let why = from_cli(&host, STORE).unwrap_err();
        assert_eq!(why, format!("--bundle-store: {dir} {fault}: {tail}"));
        assert!(host.get(|s| s.started.is_empty()));
    }

    let host = host();
    let mut dirs = layout();
    dirs.remove(Path::new("/var/db"));
    host.set(|s| s.dirs = Some(dirs));
    let why = from_cli(&host, STORE).unwrap_err();
    assert_eq!(
        why,
        format!("--bundle-store: /var/db cannot be looked at: no such directory: {tail}")
    );
}

/// The machine's own host makes a missing store directory with mode 0711 and leaves one that is
/// there as it is, checks the way from `/` against the user it trusts, and says so when the store
/// cannot be made (no parent). The user it trusts here is one that owns no directory, so the
/// first directory, `/`, is the one named. Catches a store made with a mode a lease user cannot
/// search, a store that is chmodded under an operator, and a way that is checked from the
/// middle.
#[test]
fn the_machines_host_makes_the_store_and_names_the_first_directory_it_does_not_trust() {
    const NOBODY: u32 = u32::MAX - 1;
    let host = SystemHost::new(SystemCommands::default()).with_owner(NOBODY);
    let tail = "a lease user could not walk down to the store through it";
    let refusal = format!("/ is not owned by uid {NOBODY}: {tail}");
    let dir = Scratch::new("store-layout-system");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;

    let made = dir.path("bundles");
    assert_eq!(host.prepare_store(&made), Err(refusal.clone()));
    assert_eq!(mode(&made), 0o711);

    let there = dir.path("there");
    fs::create_dir(&there).unwrap();
    fs::set_permissions(&there, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(host.prepare_store(&there), Err(refusal));
    assert_eq!(mode(&there), 0o700);

    let orphan = dir.path("no/parent");
    let why = fs::create_dir(&orphan).unwrap_err();
    assert_eq!(
        host.prepare_store(&orphan),
        Err(format!("{}: {why}", orphan.display()))
    );
}
