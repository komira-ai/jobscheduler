//! The two pieces of making an account that are not a call to the directory service: the
//! attributes of its record and its home folder.

use std::io;
use std::path::Path;

use rustix::fs::{CWD, Mode, OFlags};
use rustix::process::{Gid, Uid};

use super::host::NewUser;

/// The login shell of every account: none can log in.
pub const SHELL: &str = "/usr/bin/false";

/// The attributes of the directory record an account gets, as OpenDirectory names them,
/// each with its value: the uid, the primary group (the only group), the home, the login
/// shell ([`SHELL`]), the real name (the account's name) and the hidden flag (kept off the
/// login window's list of users). No password and no authentication authority is among
/// them, so the account has no `ShadowHash`: nothing can log in as it and it has no secure
/// token. It is in no group but its primary group, and never in `admin`.
#[must_use]
pub fn account_attributes(user: &NewUser) -> [(&'static str, String); 6] {
    [
        ("dsAttrTypeStandard:UniqueID", user.uid.to_string()),
        ("dsAttrTypeStandard:PrimaryGroupID", user.gid.to_string()),
        (
            "dsAttrTypeStandard:NFSHomeDirectory",
            user.home.to_string_lossy().into_owned(),
        ),
        ("dsAttrTypeStandard:UserShell", SHELL.to_owned()),
        ("dsAttrTypeStandard:RealName", user.name.clone()),
        ("dsAttrTypeNative:IsHidden", "1".to_owned()),
    ]
}

/// Makes the new home folder `name` in `homes`, owned by `uid` and `gid`, mode 0700. It must
/// not exist: a folder that does is an error (the account is new, and takes nothing of what
/// was there). Never through a link. `homes` is root's to write, so the directory just made
/// is still the one opened.
///
/// # Errors
/// `homes` cannot be opened, the folder exists or could not be made, or it could not be
/// given away.
pub fn make_home(homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent = rustix::fs::openat(CWD, homes, flags, Mode::empty())?;
    match rustix::fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("the home {} exists", homes.join(name).display()),
            ));
        }
        Err(why) => return Err(why.into()),
    }
    let home = rustix::fs::openat(&parent, name, flags, Mode::empty())?;
    rustix::fs::fchown(&home, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
    Ok(rustix::fs::fchmod(&home, Mode::from_raw_mode(0o700))?)
}

#[cfg(test)]
mod tests;
