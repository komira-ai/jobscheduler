//! The seccomp profile every contained job runs under, and the file it is kept in.
//!
//! Left alone, Docker and Podman filter a container's system calls with a profile that
//! is built into the engine and changes with its version. The agent passes a profile of
//! its own instead (`--security-opt seccomp=<file>`), so the filter a job meets does not
//! depend on which engine release the host has.
//!
//! The profile is the default profile of containers/common v0.57.4 (the one Podman
//! ships; Docker's own default is the same list of calls): `defaultAction` is
//! `SCMP_ACT_ERRNO`, and its entries carry `caps` and `arches` conditions that the
//! engines resolve themselves for the container's capability set and the host's
//! architecture. It differs from that release in one place: the four `personality`
//! entries also allow `ADDR_NO_RANDOMIZE`, which turns off address-space randomization
//! for the calling process and the programs it starts inside the container only.
//! Sanitizers, `gdb`, `rr` and `valgrind` set it, and a build job runs them; the
//! comparison masks that single bit out and still requires one of the four allowed
//! personas, so `READ_IMPLIES_EXEC` and every other flag or persona stay refused. The
//! Provenance: the upstream file is containers/common tag v0.57.4,
//! `pkg/seccomp/seccomp.json`, sha256
//! `cc374cf23846ce1f62f4dc807a8e2b8673c783c6f56cb475467621035d281e6c`. The embedded file
//! equals it, as parsed JSON, except for four added `personality` entries (the
//! `ADDR_NO_RANDOMIZE` ones above). The
//! file is `seccomp.json` beside this one; a test pins its sha256, so an edit of the
//! profile is a reviewed change.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The pinned profile.
pub const PROFILE: &str = include_str!("seccomp.json");

/// The name of the profile's file. The `+` is not in the grammar of a run id, so no run's
/// directory can have this name.
pub const PROFILE_FILE: &str = "seccomp+pinned.json";

/// Makes sure `dir` holds the pinned profile under [`PROFILE_FILE`] and returns the
/// file's path as UTF-8 text, the value that follows `seccomp=`.
///
/// A file that holds the pinned bytes is left alone; any other (missing, changed,
/// shorter) is replaced through a temporary file and a rename, so the engine never reads
/// half a profile. `dir` is the agent's scratch root: only the agent may enter it, and
/// the engine, which runs as root on this machine, reads the file from there.
///
/// # Errors
///
/// `dir` is not UTF-8, holds a comma (Docker splits a `--security-opt` value on commas, so
/// the path would be read as more than a file name), or the file cannot be written.
pub fn install_profile(dir: &Path) -> io::Result<String> {
    let path = dir.join(PROFILE_FILE);
    let Some(text) = path.to_str().filter(|text| !text.contains(',')) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the directory must be UTF-8 without a comma",
        ));
    };
    if fs::read(&path).is_ok_and(|have| have == PROFILE.as_bytes()) {
        return Ok(text.to_owned());
    }
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!("{PROFILE_FILE}.{}.{n}.tmp", std::process::id()));
    fs::write(&temp, PROFILE)
        .and_then(|()| fs::set_permissions(&temp, fs::Permissions::from_mode(0o644)))
        .and_then(|()| fs::rename(&temp, &path))
        .map(|()| text.to_owned())
}
