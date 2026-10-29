//! `pkg check-deb` against debs built in memory: the one package it accepts, and each way a
//! deb can differ from it. A deb here is what nfpm writes: an `ar` archive of `debian-binary`,
//! `control.tar.gz` and `data.tar.gz`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use xtask::pkg::{CheckArgs, Error, check, parse_deb, problems};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const VERSION: &str = "1.2.3";

/// One entry of the data archive.
#[derive(Clone)]
struct Ent {
    path: String,
    mode: u32,
    uid: u64,
    gid: u64,
    ty: tar::EntryType,
    body: Vec<u8>,
}

fn file(path: &str, mode: u32, body: &[u8]) -> Ent {
    Ent {
        path: path.to_owned(),
        mode,
        uid: 0,
        gid: 0,
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
        control: format!("Package: js-agent\nVersion: {VERSION}\nArchitecture: amd64\n"),
        control_extra: Vec::new(),
        data: vec![
            dir("./usr/"),
            dir("./usr/bin/"),
            file("./usr/bin/js-agent", 0o755, b"binary"),
            file("./usr/lib/systemd/system/js-agent.service", 0o644, b"unit"),
            file("./usr/lib/tmpfiles.d/js-agent.conf", 0o644, b"tmpfiles"),
            file("./usr/share/doc/js-agent/copyright", 0o644, b"license"),
        ],
    }
}

fn tgz(entries: &[Ent]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for e in entries {
        let mut h = tar::Header::new_ustar();
        h.set_path(&e.path).expect("path");
        h.set_entry_type(e.ty);
        h.set_size(e.body.len() as u64);
        h.set_mode(e.mode);
        h.set_uid(e.uid);
        h.set_gid(e.gid);
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

fn deb(spec: &Spec) -> Vec<u8> {
    ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", &control_tgz(spec)),
        ("data.tar.gz", &tgz(&spec.data)),
    ])
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sources() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("binary".to_owned(), sha(b"binary")),
        ("js-agent.service".to_owned(), sha(b"unit")),
        ("tmpfiles.conf".to_owned(), sha(b"tmpfiles")),
    ])
}

/// What `problems` says of `spec`.
fn found(spec: &Spec) -> Vec<String> {
    problems(&parse_deb(&deb(spec)).expect("a deb"), VERSION, &sources())
}

fn only(spec: &Spec, expected: &str) {
    let all = found(spec);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].contains(expected), "{all:?} lacks {expected:?}");
}

/// The package nfpm is expected to write has nothing wrong with it, and its directories (which
/// the listing holds besides the files) are accepted.
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

/// A dependency makes a plain `dpkg --install` leave the package unconfigured.
#[test]
fn a_dependency_or_conflict_is_refused() {
    for field in ["Depends", "Pre-Depends", "Breaks", "Conflicts"] {
        let mut spec = good();
        spec.control.push_str(&format!("{field}: docker.io\n"));
        only(&spec, &format!("{field}: docker.io"));
    }
}

/// Recommends and Suggests do not fail an install and are allowed.
#[test]
fn a_recommendation_is_allowed() {
    let mut spec = good();
    spec.control.push_str("Recommends: docker.io\n");
    assert_eq!(found(&spec), Vec::<String>::new());
}

/// A maintainer script would do at install what no test here reads.
#[test]
fn a_maintainer_script_is_refused() {
    for script in ["preinst", "postinst", "prerm", "postrm"] {
        let mut spec = good();
        spec.control_extra.push(script);
        only(&spec, &format!("maintainer script {script}"));
    }
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

/// The binary with the mode of a data file, the unit executable: each mode is the listed one.
#[test]
fn a_wrong_mode_is_refused() {
    let mut spec = good();
    spec.data[2].mode = 0o4755;
    only(&spec, "usr/bin/js-agent: mode 4755, not 755");
    let mut spec = good();
    spec.data[3].mode = 0o664;
    only(&spec, "js-agent.service: mode 664, not 644");
}

/// A file or a directory owned by anyone but root (nfpm gives the builder's uid when
/// told to).
#[test]
fn a_file_or_directory_not_owned_by_root_is_refused() {
    let mut spec = good();
    spec.data[2].uid = 1000;
    only(&spec, "usr/bin/js-agent: owned by 1000:0");
    let mut spec = good();
    spec.data[1].gid = 1000;
    only(&spec, "usr/bin: owned by 0:1000");
}

/// A directory open to writing by group or others.
#[test]
fn a_directory_that_is_not_0755_is_refused() {
    let mut spec = good();
    spec.data[1].mode = 0o775;
    only(&spec, "usr/bin: a directory of mode 775");
}

/// A link in the place of the binary is not the binary.
#[test]
fn a_link_in_place_of_a_file_is_refused() {
    let mut spec = good();
    spec.data[2].ty = tar::EntryType::Symlink;
    spec.data[2].body = Vec::new();
    let all = found(&spec);
    assert!(
        all.iter()
            .any(|p| p.contains("usr/bin/js-agent: not a regular file")),
        "{all:?}"
    );
}

/// Bytes other than the build's binary, unit or tmpfiles file.
#[test]
fn bytes_other_than_the_sources_are_refused() {
    let mut spec = good();
    spec.data[2].body = b"another binary".to_vec();
    only(&spec, "usr/bin/js-agent: not the bytes of Binary");
    let mut spec = good();
    spec.data[3].body = b"another unit".to_vec();
    only(
        &spec,
        "js-agent.service: not the bytes of Packaging(\"js-agent.service\")",
    );
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
        "no control.tar.gz",
    );
    // A last member of odd size whose padding byte is missing ends the archive.
    let mut unpadded = ar(&[("debian-binary", b"2.0")]);
    unpadded.pop();
    bad(&unpadded, "no control.tar.gz");
    bad(&members(&["debian-binary", "control.tar.gz"]), "not gzip");
    let control = control_tgz(&good());
    bad(
        &ar(&[("control.tar.gz", &control)]),
        "no data.tar.gz member",
    );
    let empty = tgz(&[]);
    bad(
        &ar(&[("control.tar.gz", &empty), ("data.tar.gz", &empty)]),
        "no control file",
    );
    let data = tgz(&good().data);
    bad(
        &ar(&[("control.tar.gz", b"garbage"), ("data.tar.gz", &data)]),
        "not gzip",
    );
    let mut not_tar = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    not_tar.write_all(&[7u8; 1024]).expect("write");
    let not_tar = not_tar.finish().expect("gzip");
    bad(
        &ar(&[("control.tar.gz", &not_tar), ("data.tar.gz", &data)]),
        "not a tar",
    );
    bad(
        &ar(&[("control.tar.gz", &control), ("data.tar.gz", &not_tar)]),
        "not a tar",
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
    let bytes = ar(&[("control.tar.gz", &control), ("data.tar.gz", &cut)]);
    assert!(matches!(parse_deb(&bytes), Err(Error::Malformed(_))));
}

/// A header whose mode, owner or group is not an octal number.
#[test]
fn a_header_with_a_field_that_is_not_a_number_is_malformed() {
    for field in 0..3 {
        let mut h = tar::Header::new_old();
        h.set_path("usr/bin/js-agent").expect("path");
        h.set_size(0);
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
        let bytes = ar(&[("control.tar.gz", &control), ("data.tar.gz", &data)]);
        assert!(
            matches!(parse_deb(&bytes), Err(Error::Malformed(_))),
            "{field}"
        );
    }
}

/// The directory a test writes its files in.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-pkg")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(dir.join("packaging")).expect("scratch");
    std::fs::write(dir.join("js-agent"), b"binary").expect("binary");
    std::fs::write(dir.join("packaging/js-agent.service"), b"unit").expect("unit");
    std::fs::write(dir.join("packaging/tmpfiles.conf"), b"tmpfiles").expect("tmpfiles");
    std::fs::write(dir.join("js-agent.deb"), deb(&good())).expect("deb");
    dir
}

fn args(dir: &Path) -> CheckArgs {
    CheckArgs {
        deb: dir.join("js-agent.deb"),
        version: VERSION.to_owned(),
        binary: dir.join("js-agent"),
        packaging: dir.join("packaging"),
    }
}

/// The check passes for the package built from these files, and refuses it when the binary
/// that went into it is not the one given.
#[test]
fn check_compares_the_deb_with_the_files_it_was_built_from() {
    let dir = scratch("check");
    let line = check(&args(&dir)).expect("the package");
    assert!(line.ends_with("js-agent 1.2.3 with 4 files"), "{line}");
    std::fs::write(dir.join("js-agent"), b"rebuilt").expect("binary");
    match check(&args(&dir)) {
        Err(Error::Refused(all)) => assert_eq!(all.len(), 1, "{all:?}"),
        other => panic!("{other:?}"),
    }
}

/// A file that is not there is named.
#[test]
fn check_names_the_file_it_cannot_read() {
    for gone in ["js-agent.deb", "js-agent", "packaging/js-agent.service"] {
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

/// The verb's exit status: 0 and the line for the package, 1 and the reason for another.
#[test]
fn the_verb_exits_0_for_the_package_and_1_for_another_deb() {
    let dir = scratch("verb");
    let run = |dir: &Path| {
        Command::new(XTASK)
            .args(["pkg", "check-deb", "--deb"])
            .arg(dir.join("js-agent.deb"))
            .args(["--version", VERSION, "--binary"])
            .arg(dir.join("js-agent"))
            .arg("--packaging")
            .arg(dir.join("packaging"))
            .output()
            .expect("run xtask")
    };
    let ok = run(&dir);
    assert_eq!(ok.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("js-agent 1.2.3"));
    std::fs::write(dir.join("js-agent"), b"rebuilt").expect("binary");
    let refused = run(&dir);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not the js-agent package"));
}
