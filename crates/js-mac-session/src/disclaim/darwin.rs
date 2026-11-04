//! The macOS half: the system's calls behind the seams of `super::plan`, the closure the
//! child runs, and the responsible-pid query.

use std::collections::BTreeMap;
use std::ffi::{OsString, c_int, c_short};
use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use super::Steps;
use super::plan::{self, AttributeCalls, Plan, Strings};

// FFI-BOUNDARY: two functions libSystem exports and Apple's public headers do not
// declare. Neither takes or gives ownership of memory: `setdisclaim` writes into the
// attribute object its caller owns (made by `posix_spawnattr_init` and freed by
// `posix_spawnattr_destroy`, both in `Attributes`), and the pid query takes and returns a
// number.
unsafe extern "C" {
    fn responsibility_spawnattrs_setdisclaim(
        attr: *mut libc::posix_spawnattr_t,
        disclaim: c_int,
    ) -> c_int;
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
}

// The flag `plan` asks for is the C library's.
const _: () = assert!(libc::POSIX_SPAWN_SETEXEC == plan::SETEXEC as c_int);

/// `posix_spawn` attributes that execute in place with the responsibility disclaimed.
pub(super) struct Attributes(libc::posix_spawnattr_t);

// SAFETY: the attribute object is owned by this value alone. `new` writes it before the
// value can be shared, and after that it is only read (`posix_spawn` copies it); `drop`
// frees it, once.
unsafe impl Send for Attributes {}
// SAFETY: as for `Send`: a shared reference only reads it.
unsafe impl Sync for Attributes {}

/// The real calls, on the attribute object they make.
struct Calls(libc::posix_spawnattr_t);

impl AttributeCalls for Calls {
    fn init(&mut self) -> c_int {
        // SAFETY: `self.0` is writable; `posix_spawnattr_init` makes an attribute object
        // and stores its address there.
        unsafe { libc::posix_spawnattr_init(&raw mut self.0) }
    }

    fn set_flags(&mut self, flags: c_short) -> c_int {
        // SAFETY: called only after `init` succeeded (`plan::attributes`), on its object.
        unsafe { libc::posix_spawnattr_setflags(&raw mut self.0, flags) }
    }

    fn set_disclaim(&mut self, disclaim: c_int) -> c_int {
        // SAFETY: as for `set_flags`.
        unsafe { responsibility_spawnattrs_setdisclaim(&raw mut self.0, disclaim) }
    }

    fn destroy(&mut self) {
        // SAFETY: called only after `init` succeeded, once, on its object.
        unsafe { libc::posix_spawnattr_destroy(&raw mut self.0) };
    }
}

impl Attributes {
    /// Makes the attributes (`plan::attributes`).
    ///
    /// # Errors
    /// The system refused one of the calls; nothing is left to free.
    pub(super) fn new() -> io::Result<Self> {
        let mut calls = Calls(std::ptr::null_mut());
        plan::attributes(&mut calls)?;
        Ok(Self(calls.0))
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        Calls(self.0).destroy();
    }
}

/// The environment a command's program gets: this process's, or none when it was cleared,
/// with the variables the command set or removed since.
pub(super) fn environment(cleared: bool, command: &Command) -> Vec<(OsString, OsString)> {
    let mut env: BTreeMap<OsString, OsString> = match cleared {
        true => BTreeMap::new(),
        false => std::env::vars_os().collect(),
    };
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => env.insert(name.to_owned(), value.to_owned()),
            None => env.remove(name),
        };
    }
    env.into_iter().collect()
}

/// Executes `plan` in this process's place with `attributes` (`Plan::run`); returns only
/// when it could not, with the reason. Allocates nothing.
pub(super) fn exec(plan: &Plan, attributes: &Attributes) -> io::Error {
    let envp = plan.envp();
    let mut attempt = |file: &Strings, argv: &Strings| -> c_int {
        let mut pid: libc::pid_t = 0;
        // SAFETY: the file is a NUL-terminated string, and the argument and environment
        // vectors are null-ended vectors of NUL-terminated strings, all owned by `plan`;
        // the attributes are an object `Attributes::new` made. With `POSIX_SPAWN_SETEXEC`
        // the call returns only on failure.
        unsafe {
            libc::posix_spawn(
                &raw mut pid,
                file.first(),
                std::ptr::null(),
                &raw const attributes.0,
                argv.pointers().cast(),
                envp.pointers().cast(),
            )
        }
    };
    io::Error::from_raw_os_error(plan.run(&mut attempt))
}

/// Makes `command` run `steps`, if any, in the child and then execute `plan` with
/// `attributes`, its responsibility disclaimed.
pub(super) fn disclaim(
    command: &mut Command,
    plan: Plan,
    attributes: Attributes,
    steps: Option<Steps>,
) {
    let mut steps = steps;
    let last = move || -> io::Result<()> {
        if let Some(steps) = steps.as_mut() {
            steps()?;
        }
        Err(exec(&plan, &attributes))
    };
    // SAFETY: the closure runs in the child between fork and exec. `steps` is
    // async-signal-safe and allocates nothing (the contract of `Program::with_steps`), and
    // `exec` calls only `posix_spawn`, with the plan and the attributes made before the
    // fork. It is the command's only closure, so it runs after the standard library's
    // set-up (ids, working directory, streams), and nothing runs after it.
    unsafe {
        command.pre_exec(last);
    }
}

/// See `super::responsible_pid`.
pub(super) fn responsible_pid(pid: u32) -> io::Result<u32> {
    let pid = plan::pid_of(pid)?;
    // SAFETY: a query by number; nothing is passed by address.
    let answer = unsafe { responsibility_get_pid_responsible_for_pid(pid) };
    plan::answered(answer, io::Error::last_os_error())
}
