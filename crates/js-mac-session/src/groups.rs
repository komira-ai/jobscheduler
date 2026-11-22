//! The group list a process started as a lease user holds: exactly the user's own.
//!
//! A process the root helper starts as a lease user must hold that user's uid, primary
//! gid and supplementary groups, and no group of the helper's. Emptying the list with
//! `setgroups(0, NULL)`, which the standard library does when root sets a uid and no
//! groups were given, is wrong on macOS: XNU treats a count below 1 as 1 with a
//! zero-filled array, so the process would hold group 0 (`wheel`) and could read what
//! only `wheel` may (`/etc/sudoers`). The list is therefore resolved here, in the
//! parent, with `getgrouplist`, which is not async-signal-safe and so cannot be called
//! between `fork` and `exec`; the child passes the result to `setgroups` unchanged.

use std::ffi::CString;

/// The most groups the kernel's list holds (`NGROUPS_MAX`): 16 on macOS, 65536 on Linux.
#[cfg(target_os = "macos")]
pub const MAX_GROUPS: usize = 16;
/// The most groups the kernel's list holds (`NGROUPS_MAX`): 16 on macOS, 65536 on Linux.
#[cfg(not(target_os = "macos"))]
pub const MAX_GROUPS: usize = 65536;

/// The type `getgrouplist` fills its list with: `int` on macOS, `gid_t` elsewhere.
#[cfg(target_os = "macos")]
type Raw = libc::c_int;
/// The type `getgrouplist` fills its list with: `int` on macOS, `gid_t` elsewhere.
#[cfg(not(target_os = "macos"))]
type Raw = libc::gid_t;

/// The groups of `user` (whose primary group is `gid`): `gid` and every group the
/// directory lists the user in, in the order `getgrouplist` gives them.
///
/// # Errors
/// The name holds NUL, or the list does not fit the kernel's list ([`MAX_GROUPS`]): it is
/// refused, by the user's name, not cut short.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // MAX_GROUPS is small
pub fn of_user(user: &str, gid: u32) -> Result<Vec<u32>, String> {
    let name = CString::new(user).map_err(|_| format!("the user name {user:?} holds NUL"))?;
    // One more than the kernel takes, so that a list that is too long is seen.
    let room = MAX_GROUPS + 1;
    let mut list: Vec<Raw> = vec![0; room];
    let mut count = room as libc::c_int;
    // SAFETY: `name` is NUL-terminated; `list` has room for `count` entries and `count`
    // is a writable int, which the call updates to the number it wrote. When the list is
    // too small the call fails and leaves `count` at the room given (macOS) or sets it to
    // the number needed (glibc); either is above `MAX_GROUPS`, so `fits` refuses it.
    unsafe {
        libc::getgrouplist(
            name.as_ptr(),
            Raw::from_ne_bytes(gid.to_ne_bytes()),
            list.as_mut_ptr(),
            &raw mut count,
        );
    }
    let found = count.unsigned_abs() as usize;
    let groups: Vec<u32> = list
        .iter()
        .take(found)
        .map(|g| u32::from_ne_bytes(g.to_ne_bytes()))
        .collect();
    fits(user, groups)
}

/// `groups`, when they fit the kernel's list.
///
/// # Errors
/// There are more than [`MAX_GROUPS`], and the error names `user`.
pub fn fits(user: &str, groups: Vec<u32>) -> Result<Vec<u32>, String> {
    match groups.len() > MAX_GROUPS {
        true => Err(format!(
            "{user} is in more than {MAX_GROUPS} groups, which a process cannot hold"
        )),
        false => Ok(groups),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name and primary group of the user running the tests, from the directory.
    fn me() -> (String, u32) {
        let uid = rustix::process::getuid().as_raw();
        // SAFETY: getpwuid returns a pointer to a static entry or null; the name is
        // copied before another call.
        let name = unsafe {
            let entry = libc::getpwuid(uid);
            assert!(!entry.is_null(), "the test user has no account");
            std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned()
        };
        (name, rustix::process::getgid().as_raw())
    }

    /// The list of a real account holds its primary group and fits the kernel's list.
    /// Catches a list that drops the primary group.
    #[test]
    fn a_real_user_has_its_primary_group_in_the_list() {
        let (name, gid) = me();
        let groups = of_user(&name, gid).unwrap();
        assert!(groups.contains(&gid), "{groups:?}");
        assert!(groups.len() <= MAX_GROUPS);
    }

    /// A name the directory does not have is listed with its primary group first, and
    /// never in group 0 (macOS adds an everyone-like entry of its own to such a list, so
    /// the list is not compared whole). Catches an unknown user given group 0 or no group.
    #[test]
    fn an_unknown_user_holds_its_primary_group_and_never_group_0() {
        let groups = of_user("kbf-no-such-user", 4242).unwrap();
        assert_eq!(groups.first(), Some(&4242), "{groups:?}");
        assert!(!groups.contains(&0), "{groups:?}");
    }

    /// A name with NUL is refused. Catches a lookup that reads a shorter name.
    #[test]
    fn a_name_with_nul_is_refused() {
        assert!(of_user("a\0b", 1).unwrap_err().contains("NUL"));
    }

    /// A list longer than the kernel's is refused by the user's name and never cut
    /// short, and one at the limit is kept whole. Catches a list truncated to fit.
    #[test]
    fn a_list_longer_than_the_kernels_is_refused_by_name() {
        let long: Vec<u32> = (1..=u32::try_from(MAX_GROUPS + 1).unwrap()).collect();
        assert_eq!(
            fits("kbf-lease-pool-4", long).unwrap_err(),
            format!(
                "kbf-lease-pool-4 is in more than {MAX_GROUPS} groups, which a process cannot hold"
            )
        );
        let full: Vec<u32> = (1..=u32::try_from(MAX_GROUPS).unwrap()).collect();
        assert_eq!(fits("u", full.clone()), Ok(full));
    }
}
