//! The drop to the slot user, the limits and the Seatbelt profile. Each runs in the supervisor,
//! a fresh process the agent started for the one job, with one thread and no other for its
//! whole life.

use std::ffi::{CStr, c_char, c_int};
use std::io;
use std::ptr;

use js_mac_pool::seatbelt::SandboxInit;
use rustix::process::{Resource, Rlimit, setrlimit};

// FFI-BOUNDARY: `sandbox_init_with_parameters` reads the profile text and the NULL-ended array
// of parameter strings, all owned by the caller (`SandboxInit`) and kept by nobody after it
// returns; on a failure it stores in `errorbuf` a string the library allocated, which the
// caller frees once with `sandbox_free_error` and nothing else. The function is exported by
// libsystem_sandbox (part of libSystem) but is not in the public `<sandbox.h>`; its signature
// is the one Seatbelt's own tools use.
unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Drops this process to `uid`, with `gid` its only group, for good: after the drop, a
/// `setuid(0)` that succeeds is an error.
pub(crate) fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    let groups = [gid];
    // SAFETY: `groups` is ours, alive for the call, and holds the 1 entry given; `setgroups`
    // only reads it. `setgid` and `setuid` take plain values.
    unsafe {
        if libc::setgroups(1, groups.as_ptr()) != 0
            || libc::setgid(gid) != 0
            || libc::setuid(uid) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: `setuid` takes a plain value and touches no memory of ours.
    if unsafe { libc::setuid(0) } == 0 {
        return Err(io::Error::other("root could be regained after the drop"));
    }
    Ok(())
}

/// Sets the largest file this process and its children may write to `max_file_bytes`
/// (`RLIMIT_FSIZE`) and the most processes its user may have to `max_processes`
/// (`RLIMIT_NPROC`), each soft and hard, so the job cannot raise them.
pub(crate) fn limits(max_file_bytes: u64, max_processes: u32) -> io::Result<()> {
    let both = |value: u64| Rlimit {
        current: Some(value),
        maximum: Some(value),
    };
    setrlimit(Resource::Fsize, both(max_file_bytes))?;
    setrlimit(Resource::Nproc, both(u64::from(max_processes)))?;
    Ok(())
}

/// Applies the profile `init` (its text and parameters) to this process and every process it
/// starts; the library's own message when it refuses the profile.
///
/// The library allocates as it compiles the profile. That is why this runs here, in the
/// supervisor, and not in a child of the agent between `fork` and `exec`: this process has
/// one thread, so no other thread can hold the allocator's lock while it does.
pub(crate) fn apply_profile(init: &SandboxInit) -> io::Result<()> {
    let mut params: Vec<*const c_char> = init.params().iter().map(|p| p.as_ptr()).collect();
    params.push(ptr::null());
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: the text and every pointer of the NULL-ended array point at C strings `init`
    // owns, alive for the call; `error` is ours (see FFI-BOUNDARY).
    let rc = unsafe {
        sandbox_init_with_parameters(init.text().as_ptr(), 0, params.as_ptr(), &mut error)
    };
    if rc == 0 {
        return Ok(());
    }
    if error.is_null() {
        return Err(io::Error::other(format!(
            "sandbox_init_with_parameters returned {rc}"
        )));
    }
    // SAFETY: `error` is the NUL-ended string the library made; it is read once here, then
    // freed once below, the way the library says.
    let why = unsafe { CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: as above; `error` is not used after.
    unsafe { sandbox_free_error(error) };
    Err(io::Error::other(why))
}
