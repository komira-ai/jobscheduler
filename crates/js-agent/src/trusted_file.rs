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

/// What an error says of a file larger than its cap.
const TOO_LARGE: &str = "larger than the size cap";

/// What an error says of a file that is not UTF-8.
const NOT_TEXT: &str = "not UTF-8 text";

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
        return Err(io::Error::other(TOO_LARGE));
    }
    if !owned(
        &fs::metadata(directory_of(path))?,
        owner,
        NOT_WRITABLE_BY_OTHERS.forbidden,
    ) {
        return Err(io::Error::other(
            "its directory is not owned by the expected user, or is writable by group or others",
        ));
    }
    // The length was checked through the descriptor, but a file can grow after that (and
    // some report no length at all): one byte past the cap is read, and refused, so a
    // file is never taken for the prefix of it that fits.
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(io::Error::other(TOO_LARGE));
    }
    String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, NOT_TEXT))
}

/// The directory `path` is in: its parent, the current directory for a bare file name,
/// and `/` for `/` itself.
fn directory_of(path: &Path) -> &Path {
    let parent = path.parent().unwrap_or(Path::new("/"));
    match parent.as_os_str().is_empty() {
        true => Path::new("."),
        false => parent,
    }
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

    /// Proves every permission bit of group and of others is refused on its own for a
    /// secret, the write bits alone for a list, and that a file exactly at the cap is
    /// read. Catches a mask that misses one bit (a world-readable secret), and a cap that
    /// is off by one.
    #[test]
    fn each_bit_is_judged_alone_and_the_cap_is_inclusive() {
        let dir = TestDir::under(std::env::temp_dir(), "trusted-file-bits");
        let secret_rule =
            "not owned by the expected user, or readable or writable by group or others";
        let list_rule = "not owned by the expected user, or writable by group or others";
        for (mode, secret_ok, list_ok) in [
            (0o600, true, true),
            (0o400, true, true),
            (0o640, false, true),
            (0o620, false, false),
            (0o610, false, true),
            (0o604, false, true),
            (0o602, false, false),
            (0o601, false, true),
        ] {
            let path = file(
                &dir,
                &format!("m{mode:o}"),
                "line
",
                mode,
            );
            let as_secret = read(&path, me(), SECRET, 5).map_err(|e| e.to_string());
            let as_list = read(&path, me(), NOT_WRITABLE_BY_OTHERS, 5).map_err(|e| e.to_string());
            let want = |ok: bool, rule: &str| match ok {
                true => Ok("line\n".to_owned()),
                false => Err(rule.to_owned()),
            };
            assert_eq!(as_secret, want(secret_ok, secret_rule), "{mode:o}");
            assert_eq!(as_list, want(list_ok, list_rule), "{mode:o}");
        }
    }

    /// Proves a directory that its group alone, or others alone, can write is refused.
    /// Catches a parent check that looks at one of the two.
    #[test]
    fn a_directory_its_group_or_others_can_write_is_refused() {
        let want =
            "its directory is not owned by the expected user, or is writable by group or others";
        for mode in [0o720, 0o702] {
            let dir = TestDir::under(std::env::temp_dir(), "trusted-file-parent");
            let path = file(&dir, "inside", "line\n", 0o600);
            fs::set_permissions(dir.path(), Permissions::from_mode(mode)).expect("mode");
            assert_eq!(refusal(&path, me(), SECRET, 16), want, "{mode:o}");
        }
    }

    /// Proves the directory a path is in is found for a bare file name (the current
    /// directory), a relative and an absolute path, and `/`. Catches a bare file name
    /// whose empty parent fails the directory check, refusing a file that keeps every
    /// rule.
    #[test]
    fn the_directory_of_a_bare_file_name_is_the_current_one() {
        for (path, want) in [
            ("creds", "."),
            ("conf/creds", "conf"),
            ("/etc/js/creds", "/etc/js"),
            ("/creds", "/"),
            ("/", "/"),
        ] {
            assert_eq!(directory_of(Path::new(path)), Path::new(want), "{path}");
        }
    }

    /// Proves a file that holds more than its cap when it is read is refused, even when
    /// the descriptor said it was small: a `/proc` file reports a length of 0 (as a file
    /// that grew after the check would report a stale one). Catches a reader that takes
    /// the first `cap` bytes of a longer file for the whole of it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_longer_than_its_reported_length_is_refused() {
        let status = Path::new("/proc/self/status");
        let whole = read(status, me(), NOT_WRITABLE_BY_OTHERS, 1 << 20).expect("read");
        assert!(whole.len() > 8, "{whole}");
        assert_eq!(
            refusal(status, me(), NOT_WRITABLE_BY_OTHERS, 8),
            "larger than the size cap"
        );
    }
}
