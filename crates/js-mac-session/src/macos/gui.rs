//! Starting a lease's process in its user's GUI session (`run` with `session: gui`).
//!
//! A process belongs to a launchd bootstrap namespace: one the helper starts is in the
//! system's, as the helper is, and cannot reach the user's Aqua services (the window
//! server, Launch Services, the pasteboard) whatever its uid. `launchctl asuser <uid>
//! <program>` runs `<program>` in the namespace of the user's `gui/<uid>` domain, and
//! must be run as root; the program it runs is still root's. So the helper runs its own
//! binary there in the `as-user` mode ([`as_user`]), which drops to the lease user's
//! uid and gid and exactly the groups the helper resolved for it, which it is given on its
//! argument vector (`crate::groups`; resolved once, in the helper, so the two ways to
//! start a process hold the same list, and no directory lookup runs here), and only then executes the action's
//! argument vector, with its responsibility disclaimed (`crate::disclaim::exec`), so it
//! answers to the privacy controls (TCC) for itself; whether this path should keep the
//! responsibility is not decided. The working directory, the
//! environment and the descriptors `crate::spawn` set up pass through both executions
//! unchanged.

use std::ffi::{OsStr, OsString};
use std::io;
use std::process::ExitCode;

/// The program that switches bootstrap namespaces.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The first argument of the helper's `as-user` mode.
pub const AS_USER: &str = "as-user";

/// The program and arguments that start `program` with `args` as `uid`:`gid` holding
/// `groups` in `gui/<uid>`: `launchctl asuser <uid> <this binary> as-user <uid> <gid>
/// <groups, comma-separated> -- <program> <args>`, to be run as root.
///
/// # Errors
/// This binary's path cannot be found.
pub fn argv(
    uid: u32,
    gid: u32,
    groups: &[u32],
    program: &str,
    args: &[String],
) -> io::Result<(&'static str, Vec<OsString>)> {
    let groups: Vec<String> = groups.iter().map(ToString::to_string).collect();
    let helper = std::env::current_exe()?;
    let mut argv: Vec<OsString> = vec![
        "asuser".into(),
        uid.to_string().into(),
        helper.into(),
        AS_USER.into(),
        uid.to_string().into(),
        gid.to_string().into(),
        groups.join(",").into(),
        "--".into(),
        program.into(),
    ];
    argv.extend(args.iter().map(OsString::from));
    Ok((LAUNCHCTL, argv))
}

/// The `as-user` mode, given the arguments after `as-user`: `<uid> <gid> <groups> --
/// <program> [args]`, the groups comma-separated. Drops to `uid`:`gid` holding exactly
/// `groups`, checks that neither the real nor the effective ids are root's any more, and
/// executes the program; returns
/// only on failure (exit 126 for a refusal before the exec, 127 for a failed exec), with
/// the reason on stderr. Run by anyone but root, the drop fails and nothing executes.
#[must_use]
pub fn as_user(args: &[OsString]) -> ExitCode {
    match drop_and_exec(args) {
        Ok(never) => match never {},
        Err((code, why)) => {
            eprintln!("kbf-mac-session as-user: {why}");
            ExitCode::from(code)
        }
    }
}

/// [`as_user`]'s work: an error is its exit code and reason.
fn drop_and_exec(args: &[OsString]) -> Result<std::convert::Infallible, (u8, String)> {
    let refused = |why: String| (126, why);
    const USAGE: &str = "usage: as-user <uid> <gid> <groups> -- <program> [args]";
    let [uid, gid, groups, dashes, program, rest @ ..] = args else {
        return Err(refused(USAGE.to_owned()));
    };
    if dashes != "--" {
        return Err(refused(USAGE.to_owned()));
    }
    let id = |text: &OsStr| {
        text.to_str()
            .and_then(|t| t.parse::<u32>().ok())
            .filter(|&n| n != 0)
            .ok_or_else(|| refused(format!("{text:?} is not a non-root id")))
    };
    let (uid, gid) = (id(uid)?, id(gid)?);
    // Group 0 (`wheel`) is never a lease user's.
    let groups = groups
        .to_str()
        .unwrap_or_default()
        .split(',')
        .map(|group| id(OsStr::new(group)))
        .collect::<Result<Vec<u32>, _>>()?;
    drop_to(uid, gid, &groups).map_err(refused)?;
    let error = crate::disclaim::exec(program, rest);
    Err((
        127,
        format!("executing {}: {error}", program.to_string_lossy()),
    ))
}

/// Drops this process to `uid`:`gid` holding exactly `groups` (never `setgroups(0)`,
/// which leaves group 0 on macOS), and checks that the real and effective ids are the new
/// ones.
///
/// # Errors
/// `groups` is empty or longer than the kernel's list (`crate::groups::MAX_GROUPS`), a
/// step failed (each does for anyone but root), or the ids are not the new ones.
pub fn drop_to(uid: u32, gid: u32, groups: &[u32]) -> Result<(), String> {
    if groups.is_empty() || groups.len() > crate::groups::MAX_GROUPS {
        return Err(format!("{} supplementary groups to set", groups.len()));
    }
    // SAFETY: `groups` is a readable list of `groups.len()` ids for the call to copy, at
    // most `MAX_GROUPS`, which the count's type (`int` on macOS) holds.
    if unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) } != 0 {
        let why = io::Error::last_os_error();
        return Err(format!("setting the supplementary groups: {why}"));
    }
    // SAFETY: setgid and setuid take plain ids.
    if unsafe { libc::setgid(gid) } != 0 {
        let why = io::Error::last_os_error();
        return Err(format!("setgid {gid}: {why}"));
    }
    // SAFETY: as above.
    if unsafe { libc::setuid(uid) } != 0 {
        let why = io::Error::last_os_error();
        return Err(format!("setuid {uid}: {why}"));
    }
    // SAFETY: these read the process's own ids.
    let now = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if now != (uid, uid, gid, gid) {
        return Err(format!("the ids are {now:?} after the drop to {uid}:{gid}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// Catches an `as-user` that executes anything with arguments it cannot read, with
    /// root's id (0) as the target, or without dropping its privileges first: run here,
    /// not as root, the drop fails and the program is never executed.
    #[test]
    fn as_user_refuses_what_it_cannot_drop_to() {
        for bad in [
            &["501"][..],
            &["501", "20", "20", "/usr/bin/true"],
            &["501", "20", "20", "-", "/usr/bin/true"],
            &["0", "20", "20", "--", "/usr/bin/true"],
            &["501", "staff", "20", "--", "/usr/bin/true"],
            &["501", "20", "20,0", "--", "/usr/bin/true"],
            &["501", "20", "", "--", "/usr/bin/true"],
            &["501", "20", "20,x", "--", "/usr/bin/true"],
        ] {
            let error = drop_and_exec(&args(bad)).expect_err("executed");
            assert_eq!(error.0, 126, "{bad:?}: {}", error.1);
        }
        assert!(
            !rustix::process::geteuid().is_root(),
            "run this test as a user"
        );
        // The drop itself, not through drop_and_exec: a drop that succeeded would
        // replace this test process with the program, and the test would end without a
        // verdict (as it did with the drop of the groups taken out).
        let me = rustix::process::getuid().as_raw();
        let my_gid = rustix::process::getgid().as_raw();
        let error = drop_to(me, my_gid, &[my_gid]).expect_err("the groups were set without root");
        assert!(error.contains("supplementary groups"), "{error}");
        // An empty list is refused before `setgroups` is called, as root as much as not.
        assert_eq!(
            drop_to(me, my_gid, &[]),
            Err("0 supplementary groups to set".to_owned())
        );
    }

    /// Catches a launch that does not go through `launchctl asuser` for the lease's
    /// uid, or that passes the ids, the groups or the argument vector out of order.
    #[test]
    fn the_command_runs_this_binary_as_user_under_launchctl_asuser() {
        let (program, args) = argv(
            650,
            20,
            &[20, 12, 701],
            "/usr/bin/id",
            &["-u".to_owned(), "-r".to_owned()],
        )
        .expect("the command");
        assert_eq!(program, LAUNCHCTL);
        let got: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let me = std::env::current_exe().unwrap();
        assert_eq!(
            got,
            [
                "asuser",
                "650",
                &me.to_string_lossy(),
                AS_USER,
                "650",
                "20",
                "20,12,701",
                "--",
                "/usr/bin/id",
                "-u",
                "-r"
            ]
        );
    }
}
