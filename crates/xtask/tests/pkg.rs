//! `pkg check-deb` against debs built in memory: the one package it accepts, and each way a
//! deb can differ from it. A deb here is what nfpm writes: an `ar` archive of `debian-binary`,
//! `control.tar.gz` and `data.tar.gz`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use xtask::pkg::{CheckArgs, Digests, Error, check, elf_problem, parse_deb, problems};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const VERSION: &str = "1.2.3";
const X86_64: u16 = 62;

/// An ELF64 file whose program headers have the types and bodies of `headers`.
fn elf_with(class: u8, data: u8, machine: u16, headers: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0u8; 64];
    out[..4].copy_from_slice(b"\x7fELF");
    out[4] = class;
    out[5] = data;
    out[6] = 1;
    out[18..20].copy_from_slice(&machine.to_le_bytes());
    out[32..40].copy_from_slice(&64u64.to_le_bytes());
    out[54..56].copy_from_slice(&56u16.to_le_bytes());
    out[56..58].copy_from_slice(&(headers.len() as u16).to_le_bytes());
    let mut offset = 64 + 56 * headers.len();
    let mut bodies = Vec::new();
    for (kind, body) in headers {
        let mut header = vec![0u8; 56];
        header[..4].copy_from_slice(&kind.to_le_bytes());
        header[8..16].copy_from_slice(&(offset as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(body.len() as u64).to_le_bytes());
        out.extend_from_slice(&header);
        offset += body.len();
        bodies.extend_from_slice(body);
    }
    out.extend_from_slice(&bodies);
    out
}

/// The entries `(tag, value)` of a dynamic section.
fn dynamic(entries: &[(u64, u64)]) -> Vec<u8> {
    entries
        .iter()
        .flat_map(|(tag, value)| [tag.to_le_bytes(), value.to_le_bytes()].concat())
        .collect()
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_FLAGS_1: u64 = 0x6fff_fffb;

/// What a static-pie musl executable looks like to the checker: a loadable segment and a
/// dynamic section (for its relocations) that needs no library.
fn static_elf() -> Vec<u8> {
    let section = dynamic(&[(DT_FLAGS_1, 8), (DT_NULL, 0)]);
    elf_with(
        2,
        1,
        X86_64,
        &[(PT_LOAD, vec![0; 8]), (PT_DYNAMIC, section)],
    )
}

/// What a glibc executable looks like: a program interpreter and a needed library.
fn dynamic_elf() -> Vec<u8> {
    let section = dynamic(&[(DT_NEEDED, 1), (DT_NULL, 0)]);
    let loader = b"/lib64/ld-linux-x86-64.so.2\0".to_vec();
    elf_with(2, 1, X86_64, &[(PT_INTERP, loader), (PT_DYNAMIC, section)])
}

/// One entry of the data archive.
#[derive(Clone)]
struct Ent {
    path: String,
    mode: u32,
    uid: u64,
    gid: u64,
    owner: &'static str,
    group: &'static str,
    ty: tar::EntryType,
    body: Vec<u8>,
}

fn file(path: &str, mode: u32, body: &[u8]) -> Ent {
    Ent {
        path: path.to_owned(),
        mode,
        uid: 0,
        gid: 0,
        owner: "",
        group: "",
        ty: tar::EntryType::Regular,
        body: body.to_vec(),
    }
}

fn dir(path: &str) -> Ent {
    Ent {
        ty: tar::EntryType::Directory,
        ..file(path, 0o755, b"")
    }
}

/// A deb as a test varies it.
#[derive(Clone)]
struct Spec {
    control: String,
    control_extra: Vec<&'static str>,
    data: Vec<Ent>,
}

fn good() -> Spec {
    Spec {
        control: format!(
            "Package: js-agent\nVersion: {VERSION}\nArchitecture: amd64\n\
             Description: the agent\n Depends: prose of the description, not a field\n"
        ),
        control_extra: Vec::new(),
        data: vec![
            dir("./"),
            dir("./usr/"),
            dir("./usr/bin/"),
            file("./usr/bin/js-agent", 0o755, &static_elf()),
            file("./usr/lib/systemd/system/js-agent.service", 0o644, b"unit"),
            file("./usr/lib/tmpfiles.d/js-agent.conf", 0o644, b"tmpfiles"),
            file(
                "./usr/share/js-agent/crun_host_setup.sh",
                0o755,
                b"host setup",
            ),
            file("./usr/share/doc/js-agent/copyright", 0o644, b"license"),
        ],
    }
}

/// The entry of `spec` at `path`.
fn at<'a>(spec: &'a mut Spec, path: &str) -> &'a mut Ent {
    spec.data
        .iter_mut()
        .find(|e| e.path.trim_start_matches("./").trim_end_matches('/') == path)
        .expect("an entry")
}

fn tgz(entries: &[Ent]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for e in entries {
        let mut h = tar::Header::new_ustar();
        if e.path == "./" {
            // `set_path` takes no path without a component.
            h.as_old_mut().name[..2].copy_from_slice(b"./");
        } else {
            h.set_path(&e.path).expect("path");
        }
        h.set_entry_type(e.ty);
        h.set_size(e.body.len() as u64);
        h.set_mode(e.mode);
        h.set_uid(e.uid);
        h.set_gid(e.gid);
        h.set_username(e.owner).expect("owner");
        h.set_groupname(e.group).expect("group");
        h.set_cksum();
        tar.append(&h, &e.body[..]).expect("append");
    }
    tar.into_inner().expect("tar").finish().expect("gzip")
}

fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, body) in members {
        let head = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            "100644",
            body.len()
        );
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

fn control_tgz(spec: &Spec) -> Vec<u8> {
    let mut entries = vec![file("./control", 0o644, spec.control.as_bytes())];
    for name in &spec.control_extra {
        entries.push(file(name, 0o755, b"#!/bin/sh\n"));
    }
    tgz(&entries)
}

/// A deb of the members nfpm writes, with these two archives.
fn deb_of(control: &[u8], data: &[u8]) -> Vec<u8> {
    ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", control),
        ("data.tar.gz", data),
    ])
}

fn deb(spec: &Spec) -> Vec<u8> {
    deb_of(&control_tgz(spec), &tgz(&spec.data))
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn digests() -> Digests {
    Digests {
        binary: sha(&static_elf()),
        service: sha(b"unit"),
        tmpfiles: sha(b"tmpfiles"),
        host_setup: sha(b"host setup"),
        license: sha(b"license"),
    }
}

/// What `problems` says of `spec`.
fn found(spec: &Spec) -> Vec<String> {
    problems(&parse_deb(&deb(spec)).expect("a deb"), VERSION, &digests())
}

fn only(spec: &Spec, expected: &str) {
    let all = found(spec);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].contains(expected), "{all:?} lacks {expected:?}");
}

/// The package nfpm is expected to write has nothing wrong with it, and its directories (which
/// the listing holds besides the files, the root `./` among them) are accepted.
#[test]
fn the_package_has_no_problem() {
    assert_eq!(found(&good()), Vec::<String>::new());
}

/// A control file for another version, package or architecture is refused: the deb would
/// install as something else than what the workflow attests.
#[test]
fn control_lines_must_name_the_package_version_and_architecture() {
    let version_line = format!("Version: {VERSION}");
    for (from, to) in [
        ("Package: js-agent", "Package: other"),
        (version_line.as_str(), "Version: 9.9.9"),
        ("Architecture: amd64", "Architecture: arm64"),
    ] {
        let mut spec = good();
        spec.control = spec.control.replace(from, to);
        only(&spec, from);
    }
}

/// dpkg reads field names in any letter case, and a second line of the same field is one it may
/// take instead of the first: a deb with two of them is refused, a deb with the names in other
/// letters is the same deb.
#[test]
fn control_field_names_are_read_in_any_case_and_each_must_be_there_once() {
    for name in ["Package", "Version", "Architecture"] {
        let mut spec = good();
        let line = spec
            .control
            .lines()
            .find(|l| l.starts_with(name))
            .expect("a line")
            .to_owned();
        spec.control.push_str(&format!("{line}\n"));
        only(&spec, &format!("exactly one line {name}"));
    }
    let mut spec = good();
    spec.control = format!("package: js-agent\nVERSION: {VERSION}\narchitecture: amd64\n");
    assert_eq!(found(&spec), Vec::<String>::new());
}

/// A dependency makes a plain `dpkg --install` leave the package unconfigured, and a
/// replacement, a provision or an essential flag changes what dpkg does with other packages.
/// Field names have any letter case.
#[test]
fn a_dependency_replacement_or_conflict_is_refused_in_any_letter_case() {
    for field in [
        "Depends",
        "Pre-Depends",
        "Breaks",
        "Conflicts",
        "Replaces",
        "Provides",
        "Essential",
    ] {
        for name in [
            field.to_owned(),
            field.to_ascii_lowercase(),
            field.to_ascii_uppercase(),
        ] {
            let mut spec = good();
            spec.control.push_str(&format!("{name}: docker.io\n"));
            only(&spec, &format!("{name}: docker.io"));
        }
    }
}

/// Recommends and Suggests do not fail an install and are allowed (a deliberate decision: the
/// package may recommend an engine, which dpkg does not install on its own).
#[test]
fn a_recommendation_is_allowed() {
    let mut spec = good();
    spec.control
        .push_str("Recommends: docker.io\nSuggests: podman\n");
    assert_eq!(found(&spec), Vec::<String>::new());
}

/// A maintainer script would do at install what no test here reads, and a trigger, a conffile,
/// a config script or a template changes what dpkg does at install too: the control archive
/// holds `control` and `md5sums`.
#[test]
fn a_member_of_the_control_archive_other_than_control_and_md5sums_is_refused() {
    for script in [
        "preinst",
        "postinst",
        "prerm",
        "postrm",
        "triggers",
        "conffiles",
        "config",
        "templates",
        "shlibs",
    ] {
        let mut spec = good();
        spec.control_extra.push(script);
        only(&spec, &format!("{script}: in the control archive"));
    }
    let mut spec = good();
    spec.control_extra.push("md5sums");
    assert_eq!(found(&spec), Vec::<String>::new());
}

/// A file of the package that is not in the deb.
#[test]
fn a_missing_file_is_refused() {
    let mut spec = good();
    spec.data.retain(|e| !e.path.ends_with("js-agent.service"));
    only(&spec, "js-agent.service: missing");
}

/// A file the package does not list, however harmless it looks.
#[test]
fn an_extra_file_is_refused() {
    let mut spec = good();
    spec.data
        .push(file("./etc/js-agent/token", 0o600, b"secret"));
    only(&spec, "etc/js-agent/token: not a file of the package");
}

/// The parents of the package's files are its directories, and no other directory: an empty
/// `etc/cron.d` or `var/lib/js-agent` would be root's directory the package says it does not
/// make, and `usr/li` is the start of a name, not a parent of one.
#[test]
fn only_the_directories_the_files_lie_in_are_accepted() {
    let mut spec = good();
    for parent in [
        "./usr/lib/",
        "./usr/lib/systemd/",
        "./usr/lib/systemd/system/",
        "./usr/lib/tmpfiles.d/",
        "./usr/share/",
        "./usr/share/js-agent/",
        "./usr/share/doc/",
        "./usr/share/doc/js-agent/",
    ] {
        spec.data.push(dir(parent));
    }
    assert_eq!(found(&spec), Vec::<String>::new());
    for stranger in ["./etc/cron.d/", "./var/lib/js-agent/", "./usr/li/"] {
        let mut spec = good();
        spec.data.push(dir(stranger));
        let path = stranger.trim_start_matches("./").trim_end_matches('/');
        only(
            &spec,
            &format!("{path}: a directory no file of the package lies in"),
        );
    }
}

/// The binary with the mode of a data file, the unit executable: each mode is the listed one.
#[test]
fn a_wrong_mode_is_refused() {
    let mut spec = good();
    at(&mut spec, "usr/bin/js-agent").mode = 0o4755;
    only(&spec, "usr/bin/js-agent: mode 4755, not 755");
    let mut spec = good();
    at(&mut spec, "usr/lib/systemd/system/js-agent.service").mode = 0o664;
    only(&spec, "js-agent.service: mode 664, not 644");
    let mut spec = good();
    at(&mut spec, "usr/share/js-agent/crun_host_setup.sh").mode = 0o644;
    only(&spec, "crun_host_setup.sh: mode 644, not 755");
}

/// A file or a directory owned by anyone but root (nfpm gives the builder's uid when
/// told to), by id or by name: dpkg looks the name up first and takes the id only when the
/// entry has none, so uid 0 with the name `nobody` installs as nobody.
#[test]
fn a_file_or_directory_not_owned_by_root_is_refused() {
    let mut spec = good();
    at(&mut spec, "usr/bin/js-agent").uid = 1000;
    only(&spec, "usr/bin/js-agent: owned by 1000:0");
    let mut spec = good();
    at(&mut spec, "usr/bin").gid = 1000;
    only(&spec, "usr/bin: owned by 0:1000");
    let mut spec = good();
    at(&mut spec, "usr/bin/js-agent").owner = "nobody";
    only(
        &spec,
        "usr/bin/js-agent: owned by the names \"nobody\":\"\"",
    );
    let mut spec = good();
    at(&mut spec, "usr/bin").group = "nogroup";
    only(&spec, "usr/bin: owned by the names \"\":\"nogroup\"");
}

/// Entries that name root as their owner and group are root's, as entries with no name are.
#[test]
fn entries_that_name_root_are_roots() {
    let mut spec = good();
    for ent in &mut spec.data {
        ent.owner = "root";
        ent.group = "root";
    }
    assert_eq!(found(&spec), Vec::<String>::new());
}

/// A directory open to writing by group or others.
#[test]
fn a_directory_that_is_not_0755_is_refused() {
    let mut spec = good();
    at(&mut spec, "usr/bin").mode = 0o775;
    only(&spec, "usr/bin: a directory of mode 775");
}

/// A link in the place of the binary is not the binary.
#[test]
fn a_link_in_place_of_a_file_is_refused() {
    let mut spec = good();
    let binary = at(&mut spec, "usr/bin/js-agent");
    binary.ty = tar::EntryType::Symlink;
    binary.body = Vec::new();
    let all = found(&spec);
    assert!(
        all.iter()
            .any(|p| p.contains("usr/bin/js-agent: not a regular file")),
        "{all:?}"
    );
}

/// Bytes other than the build's binary, unit, tmpfiles file or license: the tmpfiles file makes
/// the root-only directories, and the copyright file is the repository's license.
#[test]
fn bytes_other_than_the_sources_are_refused() {
    for (path, source) in [
        ("usr/bin/js-agent", "Binary"),
        ("usr/lib/systemd/system/js-agent.service", "Service"),
        ("usr/lib/tmpfiles.d/js-agent.conf", "Tmpfiles"),
        ("usr/share/js-agent/crun_host_setup.sh", "HostSetup"),
        ("usr/share/doc/js-agent/copyright", "License"),
    ] {
        let mut spec = good();
        at(&mut spec, path).body = b"other bytes".to_vec();
        only(&spec, &format!("{path}: not the bytes of {source}"));
    }
}

/// The same path twice in the data archive: dpkg takes the last, and a path that is allowed once
/// is not allowed with other bytes after it.
#[test]
fn a_path_twice_in_the_data_archive_is_malformed() {
    let mut spec = good();
    spec.data
        .push(file("./usr/bin/js-agent", 0o755, b"another binary"));
    match parse_deb(&deb(&spec)) {
        Err(e @ Error::Malformed(_)) => {
            assert!(
                e.to_string()
                    .contains("usr/bin/js-agent is in the data archive twice")
            );
        }
        _ => panic!("not refused as malformed"),
    }
}

/// The same path twice in the control archive is refused as the data archive's is: dpkg takes
/// the last, so a second `control` would hide the one that was checked.
#[test]
fn a_path_twice_in_the_control_archive_is_malformed() {
    let mut spec = good();
    spec.control_extra.push("md5sums");
    spec.control_extra.push("md5sums");
    match parse_deb(&deb(&spec)) {
        Err(e @ Error::Malformed(_)) => {
            assert!(
                e.to_string()
                    .contains("md5sums is in the control archive twice"),
                "{e}"
            );
        }
        _ => panic!("not refused as malformed"),
    }
}

/// What cannot be read as a deb says what is wrong with it.
#[test]
fn a_file_that_is_not_a_deb_is_malformed() {
    let members = |names: &[&str]| {
        ar(&names
            .iter()
            .map(|n| (*n, &b"x"[..]))
            .collect::<Vec<(&str, &[u8])>>())
    };
    let bad = |bytes: &[u8], expected: &str| match parse_deb(bytes) {
        Err(e @ Error::Malformed(_)) => {
            let why = e.to_string();
            assert!(why.starts_with("not a deb: "), "{why}");
            assert!(why.contains(expected), "{why}");
        }
        _ => panic!("not refused as malformed"),
    };
    bad(b"not an archive", "no ar magic");
    bad(b"!<arch>\nshort", "a short ar header");
    let mut size = ar(&[("x", b"abc")]);
    size[8 + 48..8 + 58].copy_from_slice(b"12x       ");
    bad(&size, "an ar size that is not a number");
    let mut cut = ar(&[("x", b"abc")]);
    cut.truncate(cut.len() - 2);
    bad(&cut, "an ar member cut short");
    bad(
        &members(&["debian-binary", "data.tar.gz"]),
        "the ar members are",
    );
    // A last member of odd size whose padding byte is missing ends the archive.
    let mut unpadded = ar(&[("debian-binary", b"2.0")]);
    unpadded.pop();
    bad(&unpadded, "the ar members are");
    bad(
        &members(&["debian-binary", "control.tar.gz", "data.tar.gz"]),
        "debian-binary is not 2.0",
    );
    let control = control_tgz(&good());
    let data = tgz(&good().data);
    bad(&deb_of(b"garbage", &data), "not gzip");
    bad(&deb_of(&control, b"garbage"), "not gzip");
    let empty = tgz(&[]);
    bad(&deb_of(&empty, &empty), "no control file");
    let mut not_tar = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    not_tar.write_all(&[7u8; 1024]).expect("write");
    let not_tar = not_tar.finish().expect("gzip");
    bad(&deb_of(&not_tar, &data), "not a tar");
    bad(&deb_of(&control, &not_tar), "not a tar");
}

/// The members of a deb are exactly `debian-binary`, `control.tar.gz` and `data.tar.gz`, in that
/// order: an xz or zstd deb (the checker reads gzip only, and nfpm.yaml asks for it), a second
/// data archive that dpkg and the checker could read differently, and other orders are refused
/// for what they are.
#[test]
fn a_deb_of_other_members_is_refused_by_name() {
    let control = control_tgz(&good());
    let data = tgz(&good().data);
    let bad = |bytes: &[u8], expected: &str| match parse_deb(bytes) {
        Err(Error::Malformed(why)) => {
            assert!(why.contains(expected), "{why}");
            assert!(why.contains("only gzip archives are read"), "{why}");
        }
        _ => panic!("not refused as malformed"),
    };
    bad(
        &ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.xz", &control),
            ("data.tar.xz", &data),
        ]),
        "control.tar.xz",
    );
    bad(
        &ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", &control),
            ("data.tar.gz", &data),
            ("data.tar.zst", &data),
        ]),
        "data.tar.zst",
    );
    bad(
        &ar(&[
            ("control.tar.gz", &control),
            ("debian-binary", b"2.0\n"),
            ("data.tar.gz", &data),
        ]),
        "\"control.tar.gz\", \"debian-binary\"",
    );
}

/// An entry whose body is shorter than its header says (a deb cut short).
#[test]
fn a_tar_entry_cut_short_is_malformed() {
    let mut h = tar::Header::new_ustar();
    h.set_path("usr/bin/js-agent").expect("path");
    h.set_size(4096);
    h.set_mode(0o755);
    h.set_cksum();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(h.as_bytes()).expect("write");
    gz.write_all(&[1u8; 100]).expect("write");
    let cut = gz.finish().expect("gzip");
    let control = control_tgz(&good());
    assert!(matches!(
        parse_deb(&deb_of(&control, &cut)),
        Err(Error::Malformed(_))
    ));
}

/// A header whose mode, owner or group is not an octal number.
#[test]
fn a_header_with_a_field_that_is_not_a_number_is_malformed() {
    for field in 0..3 {
        let mut h = tar::Header::new_old();
        h.set_path("usr/bin/js-agent").expect("path");
        h.set_size(0);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        let old = h.as_old_mut();
        let target = match field {
            0 => &mut old.mode,
            1 => &mut old.uid,
            _ => &mut old.gid,
        };
        target.copy_from_slice(b"zzzzzzz\0");
        h.set_cksum();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(h.as_bytes()).expect("write");
        gz.write_all(&[0u8; 1024]).expect("write");
        let data = gz.finish().expect("gzip");
        let control = control_tgz(&good());
        assert!(
            matches!(
                parse_deb(&deb_of(&control, &data)),
                Err(Error::Malformed(_))
            ),
            "{field}"
        );
    }
}

/// A static-pie executable (a dynamic section with no needed library) is static; a file that is
/// not an ELF file, not 64-bit, not little-endian or not for x86-64 is not the binary of an
/// `amd64` package; a program interpreter or a needed library makes it dynamic; and headers
/// that end too soon are named.
#[test]
fn the_binary_must_be_a_static_x86_64_elf_executable() {
    assert_eq!(elf_problem(&static_elf()), None);
    assert_eq!(elf_problem(&elf_with(2, 1, X86_64, &[])), None);
    let why = |bytes: &[u8]| elf_problem(bytes).expect("a problem");
    assert!(why(b"").contains("not an ELF file"));
    assert!(why(b"#!/bin/sh\n").contains("not an ELF file"));
    assert!(why(&elf_with(1, 1, X86_64, &[])).contains("64-bit little-endian"));
    assert!(why(&elf_with(2, 2, X86_64, &[])).contains("64-bit little-endian"));
    assert!(why(&elf_with(2, 1, 183, &[])).contains("not an x86-64 ELF file"));
    assert!(why(&dynamic_elf()).contains("program interpreter"));
    let needed = elf_with(
        2,
        1,
        X86_64,
        &[(PT_DYNAMIC, dynamic(&[(DT_NEEDED, 1), (DT_NULL, 0)]))],
    );
    assert!(why(&needed).contains("needs a shared library"));
    // Entries after DT_NULL are not read.
    let after_null = elf_with(
        2,
        1,
        X86_64,
        &[(PT_DYNAMIC, dynamic(&[(DT_NULL, 0), (DT_NEEDED, 1)]))],
    );
    assert_eq!(elf_problem(&after_null), None);
    let whole = static_elf();
    for (len, expected) in [
        (40, "cut short"),
        (120, "cut short"),
        (whole.len() - 8, "cut short"),
    ] {
        assert!(why(&whole[..len]).contains(expected), "{len}");
    }
}

/// The directory a test writes its files in, with the files `check` reads and a deb of them.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-pkg")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(dir.join("packaging")).expect("scratch");
    std::fs::write(dir.join("js-agent"), static_elf()).expect("binary");
    std::fs::write(dir.join("packaging/js-agent.service"), b"unit").expect("unit");
    std::fs::write(dir.join("packaging/tmpfiles.conf"), b"tmpfiles").expect("tmpfiles");
    std::fs::write(dir.join("packaging/crun_host_setup.sh"), b"host setup").expect("script");
    std::fs::write(dir.join("LICENSE"), b"license").expect("license");
    std::fs::write(dir.join("js-agent.deb"), deb(&good())).expect("deb");
    dir
}

fn args(dir: &Path) -> CheckArgs {
    CheckArgs {
        deb: dir.join("js-agent.deb"),
        version: VERSION.parse().expect("a version"),
        binary: dir.join("js-agent"),
        packaging: dir.join("packaging"),
        license: dir.join("LICENSE"),
    }
}

/// The reasons `check` refuses the package in `dir`.
fn refused(dir: &Path) -> Vec<String> {
    match check(&args(dir)) {
        Err(Error::Refused(all)) => all,
        other => panic!("{other:?}"),
    }
}

/// The check passes for the package built from these files, and refuses it when a file that
/// went into it is not the one given: the binary, the unit, the tmpfiles file and the license
/// each, one reason apiece.
#[test]
fn check_compares_the_deb_with_the_files_it_was_built_from() {
    let dir = scratch("check");
    let line = check(&args(&dir)).expect("the package");
    assert!(line.ends_with("js-agent 1.2.3 with 5 files"), "{line}");
    let other = elf_with(2, 1, X86_64, &[(PT_LOAD, vec![1; 8])]);
    for (file, bytes, source) in [
        ("js-agent", other.as_slice(), "Binary"),
        (
            "packaging/js-agent.service",
            &b"another unit"[..],
            "Service",
        ),
        ("packaging/tmpfiles.conf", &b"another file"[..], "Tmpfiles"),
        (
            "packaging/crun_host_setup.sh",
            &b"another script"[..],
            "HostSetup",
        ),
        ("LICENSE", &b"another license"[..], "License"),
    ] {
        let dir = scratch(&format!("changed-{}", file.replace('/', "-")));
        std::fs::write(dir.join(file), bytes).expect("rewrite");
        let all = refused(&dir);
        assert_eq!(all.len(), 1, "{all:?}");
        assert!(
            all[0].contains(&format!("not the bytes of {source}")),
            "{all:?}"
        );
    }
}

/// A binary that is not a static executable is refused even when it is the one in the deb, and
/// the reason names the file.
#[test]
fn check_refuses_a_binary_that_is_not_static() {
    let dir = scratch("dynamic");
    let mut spec = good();
    at(&mut spec, "usr/bin/js-agent").body = dynamic_elf();
    std::fs::write(dir.join("js-agent"), dynamic_elf()).expect("binary");
    std::fs::write(dir.join("js-agent.deb"), deb(&spec)).expect("deb");
    let all = refused(&dir);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].contains("--binary "), "{all:?}");
    assert!(all[0].contains("program interpreter"), "{all:?}");
}

/// A file that is not there is named.
#[test]
fn check_names_the_file_it_cannot_read() {
    for gone in [
        "js-agent.deb",
        "js-agent",
        "packaging/js-agent.service",
        "packaging/tmpfiles.conf",
        "packaging/crun_host_setup.sh",
        "LICENSE",
    ] {
        let dir = scratch(&format!("unreadable-{}", gone.replace('/', "-")));
        std::fs::remove_file(dir.join(gone)).expect("remove");
        match check(&args(&dir)) {
            Err(e @ Error::Read(_)) => {
                let why = e.to_string();
                assert!(why.contains(gone), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }
}

/// The errors compare and print: two refusals with the same reasons are equal, and the
/// reasons are in the text.
#[test]
fn errors_compare_and_print() {
    let refused = Error::Refused(vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(
        refused,
        Error::Refused(vec!["a".to_owned(), "b".to_owned()])
    );
    assert_ne!(refused, Error::Read("a".to_owned()));
    assert!(refused.to_string().ends_with("a; b"));
    assert!(format!("{refused:?}").contains("Refused"));
}

/// The verb's exit status: 0 and the line for the package, 1 and the reason for another, and 2
/// for a `--version` that is not `MAJOR.MINOR.PATCH` (the verb is safe without the workflow's
/// own check of the version).
#[test]
fn the_verb_exits_0_for_the_package_1_for_another_deb_and_2_for_a_bad_version() {
    let dir = scratch("verb");
    let run = |dir: &Path, version: &str| {
        Command::new(XTASK)
            .args(["pkg", "check-deb", "--deb"])
            .arg(dir.join("js-agent.deb"))
            .args(["--version", version, "--binary"])
            .arg(dir.join("js-agent"))
            .arg("--packaging")
            .arg(dir.join("packaging"))
            .arg("--license")
            .arg(dir.join("LICENSE"))
            .output()
            .expect("run xtask")
    };
    let ok = run(&dir, VERSION);
    assert_eq!(ok.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("js-agent 1.2.3"));
    for version in ["1.2", "01.2.3", "v1.2.3", "1.2.3-rc1"] {
        let bad = run(&dir, version);
        assert_eq!(bad.status.code(), Some(2), "{version}");
    }
    std::fs::write(dir.join("LICENSE"), b"another license").expect("license");
    let refused = run(&dir, VERSION);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not the js-agent package"));
}
