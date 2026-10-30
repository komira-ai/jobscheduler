//! The mount a path lives on, read from `/proc/self/mountinfo`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// One line of `/proc/self/mountinfo`, as far as the quota needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mount {
    /// The device number (`st_dev` of a file on it), in the kernel's `dev_t` encoding.
    pub dev: u64,
    /// Where the file system is mounted.
    pub point: PathBuf,
    /// The file system type (`xfs`).
    pub fstype: String,
    /// What was mounted: for a block device the node under `/dev` (a loop device for a
    /// loop-backed volume).
    pub source: String,
}

/// The mount that holds `path`: of the mounts whose mount point is a prefix of the path,
/// the deepest, and of two at the same place the one listed last (it is on top).
pub(super) fn find(text: &str, path: &Path) -> Option<Mount> {
    text.lines()
        .filter_map(parse)
        .filter(|mount| path.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.components().count())
}

fn parse(line: &str) -> Option<Mount> {
    // id parent major:minor root point options [optional fields] - fstype source options
    let mut fields = line.split(' ');
    let (major, minor) = fields.nth(2)?.split_once(':')?;
    let dev = makedev(major.parse().ok()?, minor.parse().ok()?);
    let point = unescape(fields.nth(1)?);
    let mut rest = fields.skip_while(|field| *field != "-").skip(1);
    let fstype = rest.next()?.to_owned();
    let source = String::from_utf8_lossy(&unescape(rest.next()?)).into_owned();
    Some(Mount {
        dev,
        point: PathBuf::from(OsStr::from_bytes(&point)),
        fstype,
        source,
    })
}

/// The kernel's (and glibc's) `dev_t` for a major and a minor number.
pub(super) fn makedev(major: u64, minor: u64) -> u64 {
    ((major & 0xffff_f000) << 32)
        | ((major & 0xfff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0xff)
}

/// `\040`-style escapes of a mountinfo field (a space, tab, newline or backslash in a path)
/// turned back into the bytes.
fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match octal(&bytes[at..]) {
            Some(byte) => {
                out.push(byte);
                at += 4;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    out
}

fn octal(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [b'\\', a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', ..] => {
            Some(((a - b'0') << 6) | ((b - b'0') << 3) | (c - b'0'))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 259:3 / /scratch rw,relatime shared:9 - xfs /dev/nvme0n1p3 rw,prjquota
41 40 7:2 / /scratch/inner\\040dir rw - xfs /dev/loop2 rw,prjquota
42 22 0:30 / /proc rw - proc proc rw
";

    /// The deepest mount that holds the path wins, whatever the order of the lines, and a
    /// mount point's `\040` is a space.
    #[test]
    fn the_deepest_mount_wins() {
        let at = |path: &str| find(TEXT, Path::new(path)).map(|m| (m.fstype, m.source));
        let pair = |a: &str, b: &str| Some((a.to_owned(), b.to_owned()));
        assert_eq!(at("/scratch/run1"), pair("xfs", "/dev/nvme0n1p3"));
        assert_eq!(at("/scratch/inner dir/x"), pair("xfs", "/dev/loop2"));
        assert_eq!(at("/scratch/inner"), pair("xfs", "/dev/nvme0n1p3"));
        assert_eq!(at("/home/me"), pair("ext4", "/dev/sda1"));
        assert_eq!(find("", Path::new("/x")), None);
        let m = find(TEXT, Path::new("/scratch")).unwrap();
        assert_eq!(m.dev, makedev(259, 3));
    }

    /// Of two mounts at one place the later is the one on top.
    #[test]
    fn the_last_of_two_mounts_at_one_place_wins() {
        let text = "1 0 8:1 / /m rw - ext4 /dev/a rw\n2 1 8:2 / /m rw - xfs /dev/b rw\n";
        assert_eq!(find(text, Path::new("/m/x")).unwrap().source, "/dev/b");
    }

    /// A line that is cut short in any field is skipped, not guessed at.
    #[test]
    fn malformed_lines_are_skipped() {
        for line in [
            "",
            "1 2 nocolon / /m rw - xfs /dev/x rw",
            "1 2 a:4 / /m rw - xfs /dev/x rw",
            "1 2 3:b / /m rw - xfs /dev/x rw",
            "1 2 3:4 /",
            "1 2 3:4 / /m rw",
            "1 2 3:4 / /m rw - xfs",
        ] {
            assert_eq!(parse(line), None, "{line:?}");
        }
    }

    /// The `dev_t` encoding of the kernel: a loop device is major 7, and a high minor
    /// number is spread over two fields.
    #[test]
    fn dev_t_encoding() {
        assert_eq!(makedev(7, 0), 0x700);
        assert_eq!(makedev(259, 3), (0x103 << 8) | 3);
        assert_eq!(makedev(8, 0x1_0003), (0x1_0000 << 12) | (8 << 8) | 3);
    }

    /// An escape is three octal digits, the first at most 3; anything else stays as it is.
    #[test]
    fn escapes() {
        assert_eq!(unescape("a\\040b\\134c"), b"a b\\c");
        assert_eq!(unescape("a\\4b\\777"), b"a\\4b\\777");
        assert_eq!(unescape("tail\\04"), b"tail\\04");
    }
}
