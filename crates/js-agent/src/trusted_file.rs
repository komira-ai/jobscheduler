//! Reading a file the host trusts: the bundle lists and manifests the store reads, and
//! the `--registry-credentials` files.
//!
//! The file is opened without following a link and without blocking, then looked at
//! through the descriptor (so the file looked at is the file read). It must be a regular
//! file of the expected owner, carry none of the permission bits its [`Rule`] forbids,
//! be no larger than its cap, and be in a directory of that owner that group and others
//! cannot write. A link, a pipe, a device, a file another user could change (or, for a
//! secret, read) and a file of unbounded size are all refused.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The permission bits of group and others a file may not carry, and how an error says
/// so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rule {
    forbidden: u32,
    says: &'static str,
}

/// A file no one but its owner may change: a list of digests, a manifest.
pub(crate) const NOT_WRITABLE_BY_OTHERS: Rule = Rule {
    forbidden: 0o022,
    says: "writable by group or others",
};

/// A secret: no one but its owner may read or change it.
pub(crate) const SECRET: Rule = Rule {
    forbidden: 0o077,
    says: "readable or writable by group or others",
};

/// Whether `meta` is of `owner` and has none of `forbidden`'s bits.
fn owned(meta: &fs::Metadata, owner: u32, forbidden: u32) -> bool {
    (meta.uid() == owner) & (meta.mode() & forbidden == 0)
}

/// The text of the file at `path`, read as the module says: owned by `owner`, without
/// the bits `rule` forbids, at most `cap` bytes.
///
/// # Errors
///
/// The file cannot be opened (a link is refused here), is not a regular file, breaks the
/// owner or mode rule, is larger than `cap`, sits in a directory that is not `owner`'s or
/// that group or others can write, or is not UTF-8. The text says which, and never holds
/// a byte of the file.
pub(crate) fn read(path: &Path, owner: u32, rule: Rule, cap: u64) -> io::Result<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    if !owned(&meta, owner, rule.forbidden) {
        return Err(io::Error::other(format!(
            "not owned by the expected user, or {}",
            rule.says
        )));
    }
    if meta.len() > cap {
        return Err(io::Error::other("larger than the size cap"));
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    if !owned(
        &fs::metadata(parent)?,
        owner,
        NOT_WRITABLE_BY_OTHERS.forbidden,
    ) {
        return Err(io::Error::other(
            "its directory is not owned by the expected user, or is writable by group or others",
        ));
    }
    let mut text = String::new();
    file.take(cap).read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    use js_testkit::dir::TestDir;

    use super::*;

    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// A private directory with the file `name` holding `text` at `mode`.
    fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
        fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).expect("dir mode");
        let path = dir.path().join(name);
        fs::write(&path, text).expect("write");
        fs::set_permissions(&path, Permissions::from_mode(mode)).expect("mode");
        path
    }

    fn refusal(path: &Path, owner: u32, rule: Rule, cap: u64) -> String {
        read(path, owner, rule, cap)
            .expect_err("refused")
            .to_string()
    }

    /// Proves each rule refuses on its own, by its own message, and the file is read when
    /// it keeps them all. Catches a secret that group or others may read, a link
    /// followed, a pipe or directory read, a cap not applied, another user's file or
    /// directory trusted.
    #[test]
    fn a_file_is_read_only_when_it_keeps_every_rule() {
        let dir = TestDir::under(std::env::temp_dir(), "trusted-file");
        let ok = file(&dir, "ok", "line\n", 0o600);
        assert_eq!(read(&ok, me(), SECRET, 16).expect("read"), "line\n");
        let shared = file(&dir, "shared", "line\n", 0o644);
        assert_eq!(
            read(&shared, me(), NOT_WRITABLE_BY_OTHERS, 16).expect("read"),
            "line\n"
        );
        assert_eq!(
            refusal(&shared, me(), SECRET, 16),
            "not owned by the expected user, or readable or writable by group or others"
        );
        let writable = file(&dir, "writable", "line\n", 0o620);
        assert_eq!(
            refusal(&writable, me(), NOT_WRITABLE_BY_OTHERS, 16),
            "not owned by the expected user, or writable by group or others"
        );
        assert_eq!(
            refusal(&ok, me().wrapping_add(1), SECRET, 16),
            "not owned by the expected user, or readable or writable by group or others"
        );
        assert_eq!(refusal(&ok, me(), SECRET, 4), "larger than the size cap");

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&ok, &link).expect("symlink");
        let followed = read(&link, me(), SECRET, 16).expect_err("a link");
        assert_eq!(followed.raw_os_error(), Some(libc::ELOOP), "{followed}");

        let sub = dir.path().join("sub");
        fs::create_dir(&sub).expect("sub");
        assert_eq!(refusal(&sub, me(), SECRET, 16), "not a regular file");

        let open_dir = TestDir::under(std::env::temp_dir(), "trusted-file-open");
        let inside = file(&open_dir, "inside", "line\n", 0o600);
        fs::set_permissions(open_dir.path(), Permissions::from_mode(0o777)).expect("mode");
        assert_eq!(
            refusal(&inside, me(), SECRET, 16),
            "its directory is not owned by the expected user, or is writable by group or others"
        );

        let binary = dir.path().join("binary");
        fs::write(&binary, [0xff, 0xfe]).expect("write");
        fs::set_permissions(&binary, Permissions::from_mode(0o600)).expect("mode");
        let not_text = read(&binary, me(), SECRET, 16).expect_err("not UTF-8");
        assert_eq!(not_text.kind(), io::ErrorKind::InvalidData, "{not_text}");
    }
}
