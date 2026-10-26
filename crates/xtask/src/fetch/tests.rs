use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;

const TOOL: &[u8] = b"\x7fELF a pinned tool's bytes";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-fetch-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A tar of a directory `tool-1.0`, a decoy file, `tool-1.0/tool` holding `TOOL` and a
/// symlink `tool-1.0/link`.
fn tar() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    b.append_data(&mut dir, "tool-1.0/", &[][..]).expect("dir");
    for (path, data) in [("tool-1.0/README", &b"decoy"[..]), ("tool-1.0/tool", TOOL)] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).expect("file");
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    b.append_link(&mut link, "tool-1.0/link", "tool")
        .expect("link");
    b.into_inner().expect("tar")
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).expect("gzip");
    e.finish().expect("gzip")
}

fn member(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn tar_gz(path: &str) -> Unpack {
    Unpack::TarGz {
        member: member(path),
    }
}

/// `install` of `bytes` (pinned at their own digest) into a fresh `out`.
fn install_own(name: &str, bytes: &[u8], unpack: &Unpack) -> (Result<(), Error>, PathBuf) {
    let out = scratch(name).join("bin").join("tool");
    (install(bytes, &Sha256::of(bytes), unpack, &out), out)
}

/// Asserts that a pin parses only as 64 lowercase hex digits (a short, upper-case or
/// non-hex pin is refused) and that the digest prints as it was written.
#[test]
fn a_sha256_is_64_lowercase_hex_digits() {
    let hex = "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f";
    let digest: Sha256 = hex.parse().expect("parse");
    assert_eq!(digest.to_string(), hex);
    assert_eq!(format!("{digest:?}"), hex);
    for bad in [
        &hex[..62],
        &hex.to_uppercase(),
        &format!("{}g", &hex[..63]),
        "",
    ] {
        assert_eq!(
            bad.parse::<Sha256>(),
            Err(NotASha256(bad.to_owned())),
            "{bad}"
        );
    }
    assert_eq!(
        NotASha256("x".to_owned()).to_string(),
        "not a SHA-256: \"x\" (expected 64 lowercase hex digits)"
    );
}

/// Asserts that a wrong hash is refused before anything is unpacked or written: the
/// first archive would unpack fine, and the second is no archive at all, so a late
/// compare would report an unpack error instead of the mismatch.
#[test]
fn a_wrong_hash_is_refused_before_anything_is_unpacked_or_written() {
    let dir = scratch("wrong-hash");
    let out = dir.join("tool");
    let archive = gzip(&tar());
    let pinned = Sha256::of(b"the bytes the pin was made from");
    for (bytes, unpack) in [
        (&archive[..], tar_gz("tool-1.0/tool")),
        (b"not gzip", tar_gz("x")),
    ] {
        assert_eq!(
            install(bytes, &pinned, &unpack, &out),
            Err(Error::Mismatch {
                expected: pinned,
                actual: Sha256::of(bytes)
            })
        );
        assert!(!out.exists(), "nothing at --out");
        assert!(!part_path(&out).exists(), "no part file");
    }
    assert_eq!(
        std::fs::read_dir(&dir).expect("ls").count(),
        0,
        "nothing written"
    );
}

/// Asserts that a mismatch message names both digests, the one a maintainer needs to
/// update the pin and the one pinned.
#[test]
fn a_mismatch_names_both_digests() {
    let e = Error::Mismatch {
        expected: Sha256([0; 32]),
        actual: Sha256([0xff; 32]),
    };
    let text = e.to_string();
    assert!(text.contains(&"ff".repeat(32)), "{text}");
    assert!(text.contains(&"00".repeat(32)), "{text}");
}

/// Asserts that a matching raw download is written unchanged, with the execute bits
/// set and no part file left beside it.
#[test]
fn a_matching_raw_download_is_installed_executable() {
    let (result, out) = install_own("raw", TOOL, &Unpack::Raw);
    result.expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    let mode = std::fs::metadata(&out).expect("stat").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!part_path(&out).exists());
}

/// Asserts that a gzip tar yields its named member (and not the decoy file that comes
/// before it).
#[test]
fn a_tar_gz_yields_the_tool() {
    let (result, out) = install_own("tar-gz", &gzip(&tar()), &tar_gz("tool-1.0/tool"));
    result.expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
}

/// Asserts that a gzip stream that does not decode, and a tar whose header is not a
/// tar's, are unpack errors with nothing written.
#[test]
fn an_archive_that_does_not_decode_is_an_unpack_error_with_nothing_written() {
    let not_tar = [1u8; 1024];
    for (name, bytes) in [("gz", b"not gzip".to_vec()), ("tar", gzip(&not_tar))] {
        let (result, out) = install_own(&format!("undecodable-{name}"), &bytes, &tar_gz("x"));
        assert!(
            matches!(result, Err(Error::Unpack(_))),
            "{name}: {result:?}"
        );
        assert!(!out.exists(), "{name}");
        assert!(!part_path(&out).exists(), "{name}");
    }
}

/// Asserts that a member the archive lacks, and a member that is a symlink or a
/// directory, are refused with nothing written.
#[test]
fn a_missing_member_or_one_that_is_not_a_regular_file_is_refused() {
    let archive = gzip(&tar());
    let cases = [
        (
            "tool-1.0/cargo-deny",
            Error::NoMember(member("tool-1.0/cargo-deny")),
        ),
        ("tool-1.0/link", Error::NotAFile(member("tool-1.0/link"))),
        ("tool-1.0", Error::NotAFile(member("tool-1.0"))),
    ];
    for (path, expected) in cases {
        let (result, out) = install_own(path, &archive, &tar_gz(path));
        assert_eq!(result, Err(expected));
        assert!(!out.exists(), "{path}");
        assert!(!part_path(&out).exists(), "{path}");
    }
    assert_eq!(
        Error::NoMember(member("a/b")).to_string(),
        "the archive has no \"a/b\""
    );
}

/// Asserts that an install whose last step fails leaves no part file and keeps the old
/// `--out`: `--out` is a directory, so the rename over it fails after the part file was
/// written.
#[test]
fn an_install_that_cannot_rename_leaves_no_part_file() {
    let out = scratch("rename-fails").join("tool");
    std::fs::create_dir_all(out.join("inside")).expect("dir");
    let result = install(TOOL, &Sha256::of(TOOL), &Unpack::Raw, &out);
    let Err(Error::Install { path, why }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(path, out);
    assert!(!why.is_empty());
    assert!(!part_path(&out).exists(), "part file removed");
    assert!(out.join("inside").is_dir(), "the old --out is untouched");
}

/// Asserts that an install replaces an existing `--out` and a part file a killed run
/// left behind.
#[test]
fn an_install_replaces_an_old_tool_and_a_stale_part_file() {
    let out = scratch("replace").join("tool");
    std::fs::write(&out, b"old").expect("old");
    std::fs::write(part_path(&out), b"half").expect("stale part");
    install(TOOL, &Sha256::of(TOOL), &Unpack::Raw, &out).expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    assert!(!part_path(&out).exists());
}

/// Asserts that `--extract` and `--member` must agree: a tar format needs its member
/// and a member needs a tar format.
#[test]
fn extract_and_member_must_agree() {
    assert_eq!(Unpack::new(None, None), Ok(Unpack::Raw));
    assert_eq!(
        Unpack::new(Some(Format::TarGz), Some(member("d/tool"))),
        Ok(tar_gz("d/tool"))
    );
    let e = Unpack::new(Some(Format::TarGz), None).expect_err("no member");
    assert!(e.contains("needs --member"), "{e}");
    let e = Unpack::new(None, Some(member("d/tool"))).expect_err("member out of place");
    assert!(e.contains("needs --extract tar.gz"), "{e}");
}

/// Asserts that the part file sits in the same directory as `--out`, so the rename
/// does not cross file systems.
#[test]
fn the_part_file_sits_beside_out() {
    assert_eq!(
        part_path(Path::new("/a/b/tool")),
        Path::new("/a/b/tool.part")
    );
}
