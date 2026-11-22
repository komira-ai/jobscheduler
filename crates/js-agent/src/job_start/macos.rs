//! The macOS half of `__job-start`: the Seatbelt call, and the execution of the program.
//! All of the mode's `unsafe` code is here.

use std::ffi::{CStr, c_char, c_int};
use std::io::Write;
use std::process::ExitCode;
use std::ptr;

use js_mac_pool::seatbelt::{SandboxInit, StartArgs};
use js_mac_session::disclaim;

/// The status of a run whose profile the library refused: the program has not run.
const REFUSED: u8 = 125;

/// The status of a program that could not be executed.
const NOT_EXECUTED: u8 = 127;

/// The status `sandbox_init_with_parameters` returns when it applied the profile.
const APPLIED: c_int = 0;

/// The flags of the call: none, the profile is text.
const NO_FLAGS: u64 = 0;

// FFI-BOUNDARY: `sandbox_init_with_parameters` reads the profile text and the NULL-ended array
// of parameter strings, all owned by the caller and kept by nobody after it returns; on a
// failure it stores in `errorbuf` a string the library allocated, which the caller frees with
// `sandbox_free_error` and nothing else. It is exported by libsystem_sandbox (part of
// libSystem) but is not in the public `<sandbox.h>`; its signature is the one Seatbelt's own
// tools use.
unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Applies the profile to this process and executes the program in its place.
pub(super) fn start(start: &StartArgs, err: &mut dyn Write) -> ExitCode {
    if let Err(why) = apply(start.init()) {
        let _ = writeln!(err, "js-agent: the Seatbelt profile was refused: {why}");
        return ExitCode::from(REFUSED);
    }
    let why = disclaim::exec(start.program(), start.args());
    let _ = writeln!(
        err,
        "js-agent: executing {}: {why}",
        start.program().to_string_lossy()
    );
    ExitCode::from(NOT_EXECUTED)
}

/// Applies `init` to this process and every process it starts: the library's own message when
/// it refuses the profile.
///
/// This process is a fresh exec with one thread, so the allocation inside
/// `sandbox_init_with_parameters` cannot meet a lock another thread holds.
fn apply(init: &SandboxInit) -> Result<(), String> {
    let mut pointers: Vec<*const c_char> = init.params().iter().map(|s| s.as_ptr()).collect();
    pointers.push(ptr::null());
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: the text and the NULL-ended array point at C strings of `init`, alive for the
    // call; `error` is ours (see the FFI-BOUNDARY comment).
    let rc = unsafe {
        sandbox_init_with_parameters(
            init.text().as_ptr(),
            NO_FLAGS,
            pointers.as_ptr(),
            &mut error,
        )
    };
    if rc == APPLIED {
        return Ok(());
    }
    if error.is_null() {
        return Err(format!("sandbox_init_with_parameters returned {rc}"));
    }
    // SAFETY: `error` is the NUL-ended string the library made; it is read once, then freed
    // once, the way the library says.
    let why = unsafe { CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: as above.
    unsafe { sandbox_free_error(error) };
    Err(why)
}
