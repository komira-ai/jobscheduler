//! The guarded work root of the tests that run as root on a provisioned Mac
//! (`privileged.rs`, `trace_base_set.rs`), and the fixture built under it.
//!
//! The variables a flag-free test binary reads:
//!
//! - `JS_MAC_POOL_PRIV_ROOT`: a directory to work in, owned by root, not writable by
//!   others, not a link, on the boot volume's data volume (not under `/Volumes`: the
//!   data-volume canary is spelled through `/System/Volumes/Data`). Every ancestor of it
//!   must be owned by root or by `JS_MAC_POOL_PRIV_OWNER_UID`, and not group- or
//!   other-writable unless sticky;
//! - `JS_MAC_POOL_PRIV_OWNER_UID`: the invoking (ssh) user's uid, not 0, given by the
//!   operator (never read from the file system);
//! - `JS_MAC_POOL_PRIV_UID` and `JS_MAC_POOL_PRIV_GID`: an unprivileged user and group
//!   (neither 0).

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::Scratch;
use super::ancestry;
use super::mac::Mac;

pub fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

pub fn ids() -> (u32, u32) {
    let uid: u32 = var("JS_MAC_POOL_PRIV_UID").parse().expect("a uid");
    let gid: u32 = var("JS_MAC_POOL_PRIV_GID").parse().expect("a gid");
    assert!(uid != 0, "the pool user is not root");
    assert!(gid != 0, "the pool group is not root");
    (uid, gid)
}

/// The invoking (ssh) user's uid, from the operator. It is the one non-root owner the
/// work root's ancestors may have; nothing on the file system can name it.
pub fn owner_uid() -> u32 {
    let uid: u32 = var("JS_MAC_POOL_PRIV_OWNER_UID").parse().expect("a uid");
    assert!(uid != 0, "the invoking user is not root");
    uid
}

/// The work root, after every check, before the first write.
pub fn work_root() -> PathBuf {
    let root = PathBuf::from(var("JS_MAC_POOL_PRIV_ROOT"));
    assert_eq!(
        std::fs::canonicalize(&root).expect("the work root exists"),
        root,
        "the work root is its canonical form: no link above it"
    );
    let meta = std::fs::symlink_metadata(&root).expect("the work root");
    assert!(meta.is_dir(), "the work root is a directory, not a link");
    assert_eq!(meta.uid(), 0, "the work root belongs to root");
    assert_eq!(meta.mode() & 0o022, 0, "nobody else writes the work root");
    // The root is only as safe as the path to it: refuse before the first write.
    let allowed = ancestry::allowed_owners(owner_uid());
    if let Some(refusal) = ancestry::untrusted_ancestor(&root, Path::new("/"), &allowed) {
        panic!(
            "the work root's ancestor {} is not trusted: {:?}",
            refusal.path.display(),
            refusal.reason
        );
    }
    root
}

/// The fixture below the work root, with the run directory and the user's folder entry
/// handed to the pool user and everything else left to root.
pub fn fixture(tag: &str) -> Mac {
    let (uid, gid) = ids();
    let root = work_root();
    let scratch = Scratch::under(&root, tag);
    let entry = scratch.dir("uid-entry");
    let mac = Mac::build(scratch, entry.clone(), tag, 0);
    for dir in [&mac.run, &entry] {
        std::os::unix::fs::chown(dir, Some(uid), Some(gid)).expect("chown");
    }
    mac
}
