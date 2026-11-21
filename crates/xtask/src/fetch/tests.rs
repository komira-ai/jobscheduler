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

/// `bytes` as one zstd frame.
fn zst(bytes: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
}

/// The first half of a zstd frame of 4 KiB of xorshift output (which does not compress), so the
/// frame header is whole and the cut falls inside the block's data.
fn cut_zst() -> Vec<u8> {
    let mut state = 0x9e37_79b9u32;
    let data: Vec<u8> = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect();
    let full = zst(&data);
    full[..full.len() / 2].to_vec()
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

/// Asserts that a zstd download is installed as the file it decompresses to.
#[test]
fn a_zst_yields_the_tool() {
    let (result, out) = install_own("zst", &zst(TOOL), &Unpack::Zst);
    result.expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
}

/// Asserts that a download that is not zstd, an empty one and a cut frame are unpack errors
/// with nothing written, and that a cut frame (whose header is whole) is refused while its
/// data is read, not as a frame that is not zstd.
#[test]
fn a_zst_that_does_not_decode_is_an_unpack_error_with_nothing_written() {
    for (name, bytes) in [
        ("zst", b"not zstd".to_vec()),
        ("zst-empty", Vec::new()),
        ("zst-cut", cut_zst()),
    ] {
        let (result, out) = install_own(&format!("undecodable-{name}"), &bytes, &Unpack::Zst);
        assert!(
            matches!(result, Err(Error::Unpack(_))),
            "{name}: {result:?}"
        );
        assert!(!out.exists(), "{name}");
        assert!(!part_path(&out).exists(), "{name}");
    }
    let (result, _) = install_own("undecodable-zst-cut-why", &cut_zst(), &Unpack::Zst);
    let why = format!("{result:?}");
    assert!(why.starts_with("Err(Unpack("), "{why}");
    assert!(!why.contains("not a zstd frame"), "{why}");
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

/// Asserts that `--extract`, `--member` and `--tree` must agree: a tar format needs its
/// member (a file, or with `--tree` a directory), and a member or `--tree` needs a tar
/// format; each refusal is the whole message.
#[test]
fn extract_member_and_tree_must_agree() {
    assert_eq!(Unpack::new(None, None, false), Ok(Unpack::Raw));
    assert_eq!(
        Unpack::new(Some(Format::TarGz), Some(member("d/tool")), false),
        Ok(tar_gz("d/tool"))
    );
    assert_eq!(
        Unpack::new(Some(Format::TarXz), Some(member("d/tool")), false),
        Ok(tar_xz("d/tool"))
    );
    for format in [Format::TarGz, Format::TarXz] {
        assert_eq!(
            Unpack::new(Some(format), Some(member("d")), true),
            Ok(Unpack::Tree {
                format,
                top: member("d")
            })
        );
    }
    assert_eq!(Unpack::new(Some(Format::Zst), None, false), Ok(Unpack::Zst));
    let no_member = "--extract needs --member, the file to take (with --tree, the directory)";
    let no_extract = "--member and --tree name what to take out of a tar: they need --extract";
    let zst_alone = "--extract zst is one compressed file: it takes no --member or --tree";
    let refused = [
        (Some(Format::TarGz), None, false, no_member),
        (Some(Format::TarXz), None, true, no_member),
        (None, Some(member("d/tool")), false, no_extract),
        (None, Some(member("d")), true, no_extract),
        (None, None, true, no_extract),
        (Some(Format::Zst), Some(member("d/tool")), false, zst_alone),
        (Some(Format::Zst), Some(member("d")), true, zst_alone),
        (Some(Format::Zst), None, true, zst_alone),
    ];
    for (extract, path, tree, expected) in refused {
        assert_eq!(
            Unpack::new(extract, path.clone(), tree),
            Err(expected.to_owned()),
            "{extract:?} {path:?} {tree}"
        );
    }
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut &bytes[..], &mut out).expect("xz");
    out
}

fn tar_xz(path: &str) -> Unpack {
    Unpack::TarXz {
        member: member(path),
    }
}

fn tree(format: Format, top: &str) -> Unpack {
    Unpack::Tree {
        format,
        top: member(top),
    }
}

/// Appends a regular file of mode `mode` at `path`.
fn file(b: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32, data: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_mode(mode);
    h.set_size(data.len() as u64);
    b.append_data(&mut h, path, data).expect("file");
}

/// Appends a directory at `path`.
fn dir(b: &mut tar::Builder<Vec<u8>>, path: &str) {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_mode(0o755);
    h.set_size(0);
    b.append_data(&mut h, path, &[][..]).expect("dir");
}

/// A tar of a toolchain directory `cc-1/`: the executable `cc-1/cc`, the header
/// `cc-1/lib/std.h` (mode 0644) in a listed directory, `cc-1/lib/sub/deep.h` whose
/// directory is not listed, and an empty listed directory `cc-1/empty/`; around them the
/// decoys `cc-1.txt`, `other/cc` and `cc-1-extra/cc` (the last shares `cc-1` as a text
/// prefix, not as a directory).
fn toolchain() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    file(&mut b, "cc-1.txt", 0o644, b"decoy");
    dir(&mut b, "cc-1/");
    file(&mut b, "cc-1/cc", 0o755, b"compiler");
    dir(&mut b, "cc-1/lib/");
    file(&mut b, "cc-1/lib/std.h", 0o644, b"header");
    file(&mut b, "cc-1/lib/sub/deep.h", 0o640, b"deep");
    dir(&mut b, "cc-1/empty/");
    file(&mut b, "other/cc", 0o755, b"decoy");
    file(&mut b, "cc-1-extra/cc", 0o755, b"decoy");
    b.into_inner().expect("tar")
}

/// Every path under `root`, sorted, with `/` after a directory, and each file's mode and
/// contents.
fn listing(root: &Path) -> Vec<(String, u32, Vec<u8>)> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_owned()];
    while let Some(at) = todo.pop() {
        for entry in std::fs::read_dir(&at).expect("ls") {
            let path = entry.expect("entry").path();
            let name = path
                .strip_prefix(root)
                .expect("under")
                .display()
                .to_string();
            let meta = std::fs::metadata(&path).expect("stat");
            if meta.is_dir() {
                found.push((format!("{name}/"), 0, Vec::new()));
                todo.push(path);
            } else {
                let mode = meta.permissions().mode() & 0o777;
                found.push((name, mode, std::fs::read(&path).expect("read")));
            }
        }
    }
    found.sort();
    found
}

/// Asserts that an xz tar yields its named member, as a gzip tar does.
#[test]
fn a_tar_xz_yields_the_tool() {
    let (result, out) = install_own("tar-xz", &xz(&tar()), &tar_xz("tool-1.0/tool"));
    result.expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
}

/// Asserts that an xz stream that does not decode is an unpack error, for a file and for a
/// tree, with nothing written.
#[test]
fn an_xz_stream_that_does_not_decode_is_an_unpack_error_with_nothing_written() {
    for (name, unpack) in [("file", tar_xz("x")), ("tree", tree(Format::TarXz, "x"))] {
        let (result, out) = install_own(&format!("undecodable-xz-{name}"), b"not xz", &unpack);
        assert!(
            matches!(result, Err(Error::Unpack(_))),
            "{name}: {result:?}"
        );
        assert!(!out.exists(), "{name}");
        assert!(!part_path(&out).exists(), "{name}");
    }
}

/// Asserts that a tree install, from an xz and from a gzip tar, writes exactly the
/// directory taken: every directory (an empty one and one the archive does not list) and
/// every file under it, an executable file 0755 and any other 0644, and none of the
/// decoys beside it; and that no part directory is left.
#[test]
fn a_tree_is_installed_whole_with_its_modes_and_nothing_beside_it() {
    let expected = vec![
        ("cc".to_owned(), 0o755, b"compiler".to_vec()),
        ("empty/".to_owned(), 0, Vec::new()),
        ("lib/".to_owned(), 0, Vec::new()),
        ("lib/std.h".to_owned(), 0o644, b"header".to_vec()),
        ("lib/sub/".to_owned(), 0, Vec::new()),
        ("lib/sub/deep.h".to_owned(), 0o644, b"deep".to_vec()),
    ];
    for (name, bytes, format) in [
        ("tree-xz", xz(&toolchain()), Format::TarXz),
        ("tree-gz", gzip(&toolchain()), Format::TarGz),
    ] {
        let (result, out) = install_own(name, &bytes, &tree(format, "cc-1"));
        result.expect("install");
        assert_eq!(listing(&out), expected, "{name}");
        assert!(!part_path(&out).exists(), "{name}");
    }
}

/// Asserts that a tree install replaces the part directory a killed run left.
#[test]
fn a_tree_install_replaces_a_stale_part_directory() {
    let out = scratch("tree-stale-part").join("cc");
    std::fs::create_dir_all(part_path(&out).join("half")).expect("stale part");
    let bytes = gzip(&toolchain());
    install(
        &bytes,
        &Sha256::of(&bytes),
        &tree(Format::TarGz, "cc-1"),
        &out,
    )
    .expect("install");
    assert!(out.join("cc").is_file());
    assert!(!out.join("half").exists(), "nothing of the stale part");
    assert!(!part_path(&out).exists());
}

/// Asserts that a tree is refused, with nothing written, when the directory is not in the
/// archive, when an entry under it is a link, and when an entry's path climbs out of it.
#[test]
fn a_tree_with_no_top_a_link_or_a_climbing_path_is_refused() {
    let mut climbing = tar::Builder::new(Vec::new());
    dir(&mut climbing, "cc-1/");
    // The builder refuses `..` in a path it is given, so the name is set in the header.
    let mut h = tar::Header::new_gnu();
    let name = b"cc-1/../evil";
    h.as_old_mut().name[..name.len()].copy_from_slice(name);
    h.set_mode(0o644);
    h.set_size(1);
    h.set_cksum();
    climbing.append(&h, &b"x"[..]).expect("climbing entry");
    let climbing = climbing.into_inner().expect("tar");
    let cases = [
        (
            "no-top",
            gzip(&toolchain()),
            "cc-2",
            Error::NoMember(member("cc-2")),
        ),
        (
            "link",
            gzip(&tar()),
            "tool-1.0",
            Error::NotAFile(member("tool-1.0/link")),
        ),
        (
            "climbs",
            gzip(&climbing),
            "cc-1",
            Error::Climbs(member("cc-1/../evil")),
        ),
    ];
    for (name, bytes, top, expected) in cases {
        let (result, out) = install_own(&format!("tree-{name}"), &bytes, &tree(Format::TarGz, top));
        assert_eq!(result, Err(expected), "{name}");
        assert!(!out.exists(), "{name}");
        assert!(!part_path(&out).exists(), "{name}");
    }
    assert_eq!(
        Error::Climbs(member("a/../b")).to_string(),
        "\"a/../b\" in the archive climbs out of the directory taken"
    );
}

/// Asserts that a tree install whose writing or renaming fails leaves no part directory
/// and keeps the old `--out`: once because `--out` is a directory that holds a file (the
/// rename fails after the tree was written), once because the "directory" taken is a
/// regular file (writing it where the part directory is fails).
#[test]
fn a_tree_install_that_fails_leaves_no_part_directory() {
    let out = scratch("tree-rename-fails").join("cc");
    std::fs::create_dir_all(&out).expect("dir");
    std::fs::write(out.join("old"), b"old").expect("old");
    let bytes = gzip(&toolchain());
    let result = install(
        &bytes,
        &Sha256::of(&bytes),
        &tree(Format::TarGz, "cc-1"),
        &out,
    );
    let Err(Error::Install { path, why }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(path, out);
    assert!(!why.is_empty());
    assert!(!part_path(&out).exists(), "part directory removed");
    assert_eq!(
        std::fs::read(out.join("old")).expect("read"),
        b"old",
        "old --out kept"
    );

    let (result, out) = install_own(
        "tree-top-is-a-file",
        &bytes,
        &tree(Format::TarGz, "cc-1/cc"),
    );
    assert!(matches!(result, Err(Error::Install { .. })), "{result:?}");
    assert!(!out.exists());
    assert!(!part_path(&out).exists(), "part directory removed");
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
