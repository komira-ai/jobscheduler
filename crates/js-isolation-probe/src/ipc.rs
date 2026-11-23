//! The interprocess objects a job may leave: a POSIX named semaphore (which no interface of
//! macOS lists, so a reset cannot find it) and System V semaphores and shared memory (which
//! the reset lists with `ipcs` and removes). Each is made, looked up and removed by a name or
//! key derived from the run's token.
//!
//! These are system calls with no safe binding in the workspace's crates, so each is an
//! `unsafe` libc call with its SAFETY argument.

use std::ffi::CString;
use std::io;

/// The POSIX name of the semaphore for `token`: `/jsiso.<token>` (macOS allows 31 bytes).
#[must_use]
pub fn semaphore_name(token: &str) -> String {
    format!("/jsiso.{token}")
}

/// The System V key for `token`: FNV-1a of the token with the low bit set, so never 0
/// (`IPC_PRIVATE`).
#[must_use]
pub fn sysv_key(token: &str) -> libc::key_t {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in format!("js-isolation-probe:{token}").bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (hash | 1).cast_signed()
}

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// Makes the named semaphore `name` (mode 0600, value 1); it must not exist yet.
///
/// # Errors
/// The system's refusal (`EEXIST`, `EPERM`, `EACCES`).
pub fn semaphore_create(name: &str) -> io::Result<()> {
    let c = c_name(name)?;
    let mode: libc::c_uint = 0o600;
    let value: libc::c_uint = 1;
    // SAFETY: `c` is a NUL-terminated string that outlives the call; with O_CREAT the call
    // reads exactly two more arguments, the mode and the value, passed as the unsigned ints
    // a variadic call promotes them to.
    let sem = unsafe { libc::sem_open(c.as_ptr(), libc::O_CREAT | libc::O_EXCL, mode, value) };
    close(sem)
}

/// Opens the named semaphore `name` and closes it again: `Ok` when it exists.
///
/// # Errors
/// `ENOENT` when it does not exist; `EACCES` when it exists and is another user's.
pub fn semaphore_open(name: &str) -> io::Result<()> {
    let c = c_name(name)?;
    // SAFETY: `c` is a NUL-terminated string that outlives the call; without O_CREAT no
    // further argument is read.
    let sem = unsafe { libc::sem_open(c.as_ptr(), 0) };
    close(sem)
}

fn close(sem: *mut libc::sem_t) -> io::Result<()> {
    if sem == libc::SEM_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `sem` is the handle `sem_open` just returned and nothing else holds it; it is
    // closed once, here.
    unsafe { libc::sem_close(sem) };
    Ok(())
}

/// Removes the named semaphore `name`.
///
/// # Errors
/// The system's refusal (`ENOENT`, `EACCES`).
pub fn semaphore_remove(name: &str) -> io::Result<()> {
    let c = c_name(name)?;
    // SAFETY: `c` is a NUL-terminated string that outlives the call.
    let rc = unsafe { libc::sem_unlink(c.as_ptr()) };
    status(rc)
}

fn status(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn id(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Which System V object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysV {
    /// A semaphore set of one semaphore.
    Semaphore,
    /// A shared memory segment of one page.
    SharedMemory,
}

/// Size of the shared memory segment.
const SHM_BYTES: usize = 4096;

impl SysV {
    /// The probe's name for the kind: `sysv-semaphore`, `sysv-shared-memory`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Semaphore => "sysv-semaphore",
            Self::SharedMemory => "sysv-shared-memory",
        }
    }

    fn get(self, key: libc::key_t, flags: libc::c_int) -> io::Result<libc::c_int> {
        // SAFETY: plain integers in, an id or -1 out; no memory of ours is touched (the
        // segment is never attached).
        id(unsafe {
            match self {
                Self::Semaphore => libc::semget(key, 1, flags),
                Self::SharedMemory => libc::shmget(key, SHM_BYTES, flags),
            }
        })
    }

    /// Makes the object of `key` (mode 0600); it must not exist yet. Returns its id.
    ///
    /// # Errors
    /// The system's refusal (`EEXIST`, `EPERM`, `ENOSPC`).
    pub fn create(self, key: libc::key_t) -> io::Result<libc::c_int> {
        self.get(key, libc::IPC_CREAT | libc::IPC_EXCL | 0o600)
    }

    /// The id of the object of `key`.
    ///
    /// # Errors
    /// `ENOENT` when there is none; `EACCES` when it is another user's.
    pub fn lookup(self, key: libc::key_t) -> io::Result<libc::c_int> {
        self.get(key, 0)
    }

    /// Removes the object `id`.
    ///
    /// # Errors
    /// The system's refusal (`EPERM` for another user's, `EINVAL` for a gone one).
    pub fn remove(self, id: libc::c_int) -> io::Result<()> {
        // SAFETY: `IPC_RMID` reads no further argument of semctl, and shmctl's buffer may be
        // null for it.
        status(unsafe {
            match self {
                Self::Semaphore => libc::semctl(id, 0, libc::IPC_RMID),
                Self::SharedMemory => libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()),
            }
        })
    }
}

/// Removes, when dropped, every object a test may have made for its token, so a test that
/// fails half way leaves nothing in the host's IPC namespace.
#[cfg(test)]
pub(crate) struct Sweep(pub String);

#[cfg(test)]
impl Drop for Sweep {
    fn drop(&mut self) {
        let _ = semaphore_remove(&semaphore_name(&self.0));
        for kind in [SysV::Semaphore, SysV::SharedMemory] {
            let _ = kind
                .lookup(sysv_key(&self.0))
                .and_then(|id| kind.remove(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token no other test or run uses: the test's name and this process.
    fn token(test: &str) -> String {
        format!("{test}{}", std::process::id())
    }

    #[test]
    fn a_named_semaphore_is_made_found_and_removed() {
        let _sweep = Sweep(token("s"));
        let name = semaphore_name(&token("s"));
        assert!(name.len() <= 31, "{name}");
        semaphore_create(&name).unwrap();
        assert_eq!(
            semaphore_create(&name).unwrap_err().raw_os_error(),
            Some(libc::EEXIST)
        );
        semaphore_open(&name).unwrap();
        semaphore_remove(&name).unwrap();
        assert_eq!(
            semaphore_open(&name).unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
        assert_eq!(
            semaphore_remove(&name).unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn a_name_with_a_nul_is_invalid() {
        for result in [
            semaphore_create("/a\0b"),
            semaphore_open("/a\0b"),
            semaphore_remove("/a\0b"),
        ] {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
        }
    }

    #[test]
    fn sysv_objects_are_made_found_and_removed() {
        for kind in [SysV::Semaphore, SysV::SharedMemory] {
            let _sweep = Sweep(token(kind.name()));
            let key = sysv_key(&token(kind.name()));
            let made = kind.create(key).unwrap();
            assert_eq!(
                kind.create(key).unwrap_err().raw_os_error(),
                Some(libc::EEXIST)
            );
            assert_eq!(kind.lookup(key).unwrap(), made);
            kind.remove(made).unwrap();
            assert_eq!(
                kind.lookup(key).unwrap_err().raw_os_error(),
                Some(libc::ENOENT)
            );
            assert!(kind.remove(made).is_err());
        }
        assert_eq!(SysV::Semaphore.name(), "sysv-semaphore");
        assert_eq!(SysV::SharedMemory.name(), "sysv-shared-memory");
    }

    #[test]
    fn the_sweep_removes_what_a_failed_test_would_leave() {
        let tag = token("w");
        semaphore_create(&semaphore_name(&tag)).unwrap();
        SysV::Semaphore.create(sysv_key(&tag)).unwrap();
        SysV::SharedMemory.create(sysv_key(&tag)).unwrap();
        drop(Sweep(tag.clone()));
        assert!(semaphore_open(&semaphore_name(&tag)).is_err());
        assert!(SysV::Semaphore.lookup(sysv_key(&tag)).is_err());
        assert!(SysV::SharedMemory.lookup(sysv_key(&tag)).is_err());
    }

    #[test]
    fn keys_differ_by_token_and_are_never_private() {
        assert_ne!(sysv_key("a"), sysv_key("b"));
        assert_eq!(sysv_key("a"), sysv_key("a"));
        assert_ne!(sysv_key("a"), 0);
    }
}
