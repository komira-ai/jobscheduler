//! The js-agent .deb: its bytes, its control file and its refusals.

use std::io::Read;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::*;

const VERSION: Version = Version {
    major: 1,
    minor: 2,
    patch: 3,
};

/// A scratch directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// The repository root, two levels above this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A binary that holds the marker of [`VERSION`], and is long enough to be odd or even as asked.
fn binary(odd: bool) -> Vec<u8> {
    let mut bytes = b"\x7fELF....".to_vec();
    bytes.extend_from_slice(version_marker(VERSION).as_bytes());
    bytes.extend_from_slice(b" crun");
    if bytes.len() % 2 != usize::from(odd) {
        bytes.push(b'.');
    }
    bytes
}

/// The members of an `ar` archive: name, header fields after the name and content.
struct Member {
    name: String,
    fields: String,
    content: Vec<u8>,
}

/// Reads an `ar` archive as `dpkg-deb` does: the magic, then headers of 60 bytes each followed by
/// the content padded to an even length.
fn read_ar(bytes: &[u8]) -> Vec<Member> {
    assert_eq!(&bytes[..8], b"!<arch>\n");
    let mut at = 8;
    let mut members = Vec::new();
    while at < bytes.len() {
        let header = std::str::from_utf8(&bytes[at..at + 60]).expect("header");
        assert_eq!(&header[58..], "`\n", "the header's end");
        let len: usize = header[48..58].trim_end().parse().expect("length");
        at += 60;
        members.push(Member {
            name: header[..16].trim_end().to_owned(),
            fields: header[16..48].to_owned(),
            content: bytes[at..at + len].to_vec(),
        });
        at += len + len % 2;
    }
    members
}

/// One entry of a tar: path, type, mode, owner ids and names, time and content.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    directory: bool,
    mode: u32,
    ids: (u64, u64),
    names: (String, String),
    mtime: u64,
    content: Vec<u8>,
}

/// The entries of a gzip-compressed tar.
fn read_tar_gz(bytes: &[u8]) -> Vec<Entry> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    tar.entries()
        .expect("entries")
        .map(|entry| {
            let mut entry = entry.expect("entry");
            let header = entry.header().clone();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).expect("content");
            Entry {
                path: String::from_utf8(entry.path_bytes().to_vec()).expect("path"),
                directory: header.entry_type().is_dir(),
                mode: header.mode().expect("mode"),
                ids: (header.uid().expect("uid"), header.gid().expect("gid")),
                names: (
                    header
                        .username()
                        .expect("user")
                        .unwrap_or_default()
                        .to_owned(),
                    header
                        .groupname()
                        .expect("group")
                        .unwrap_or_default()
                        .to_owned(),
                ),
                mtime: header.mtime().expect("mtime"),
                content,
            }
        })
        .collect()
}

/// The package of `binary` for `platform`, built.
fn package(binary: &[u8], platform: Platform) -> Vec<u8> {
    build(VERSION, platform, &files(binary)).expect("the package builds")
}

/// Asserts that the same inputs give the same bytes, and different inputs different ones: the
/// package carries no time, no name and no host detail of its own.
#[test]
fn the_package_is_the_same_bytes_for_the_same_inputs() {
    let bin = binary(false);
    let first = package(&bin, Platform::LinuxX86_64);
    assert_eq!(first, package(&bin, Platform::LinuxX86_64));
    assert_ne!(first, package(&bin, Platform::LinuxArm64));
    assert_ne!(first, package(&binary(true), Platform::LinuxX86_64));
}

/// Asserts the `ar` layer as `dpkg-deb` reads it: `debian-binary` `2.0`, `control.tar.gz` and
/// `data.tar.gz` in that order, each owned by root 0:0 with mode 100644 and the fixed time, no
/// other member.
#[test]
fn the_ar_archive_has_the_three_members_in_order_owned_by_root() {
    for odd in [false, true] {
        let members = read_ar(&package(&binary(odd), Platform::LinuxX86_64));
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["debian-binary", "control.tar.gz", "data.tar.gz"]);
        assert_eq!(members[0].content, b"2.0\n");
        for member in &members {
            assert_eq!(
                member.fields,
                format!("{MTIME:<12}{:<6}{:<6}{:<8}", 0, 0, "100644"),
                "{}",
                member.name
            );
        }
    }
}

/// Asserts the control file of the package exactly: the fields in order, no `Depends`, the
/// installed size rounded up to the kilobyte of the files' bytes, the description with ` .`
/// for its empty line.
#[test]
fn the_control_file_is_exactly_this() {
    let files = vec![
        ("usr/bin/js-agent".to_owned(), 0o755, vec![0; 1025]),
        ("usr/share/x".to_owned(), 0o644, vec![0; 10]),
    ];
    assert_eq!(
        control(VERSION, "arm64", &files),
        "Package: js-agent\nVersion: 1.2.3\nArchitecture: arm64\n\
         Maintainer: komira-ai <noreply@komira.com>\nInstalled-Size: 2\n\
         Section: admin\nPriority: optional\nHomepage: https://github.com/komira-ai/jobscheduler\n\
         Description: job scheduler agent: runs the jobs a lease server leases to this host\n \
         js-agent joins a js-lease-server pool and runs the jobs it is leased in containers.\n .\n \
         The package installs the program, the systemd units of a host that runs jobs with crun and\n \
         the host setup script. It starts and enables nothing: place the host's files, then start a unit.\n"
    );
}

/// Asserts the control archive holds the control file of the platform's architecture and the
/// md5sums of every file, and no maintainer script and no conffiles: nothing runs at install
/// and no file is kept at removal.
#[test]
fn the_control_archive_holds_control_and_md5sums_only() {
    let bin = binary(false);
    let members = read_ar(&package(&bin, Platform::LinuxArm64));
    let entries = read_tar_gz(&members[1].content);
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["./", "./control", "./md5sums"]);
    let control_text = String::from_utf8(entries[1].content.clone()).expect("text");
    assert_eq!(control_text, control(VERSION, "arm64", &files(&bin)));
    assert!(!control_text.contains("Depends"));
    for entry in &entries {
        assert_eq!(
            (entry.ids, entry.names.clone(), entry.mtime),
            (
                (0, 0),
                ("root".to_owned(), "root".to_owned()),
                u64::from(MTIME)
            ),
            "{}",
            entry.path
        );
    }
    assert!(entries[0].directory && entries[0].mode == 0o755);
    assert!(entries[1..].iter().all(|e| !e.directory && e.mode == 0o644));
}

/// Asserts the md5sums file: one line per file, in path order, `md5sum`'s `<hex>  <path>`.
#[test]
fn md5sums_lists_each_file_in_path_order() {
    let files = vec![
        ("b".to_owned(), 0o644, b"two".to_vec()),
        ("a".to_owned(), 0o644, b"one".to_vec()),
    ];
    assert_eq!(
        md5sums(&files),
        "f97c5d29941bfb1b2fdab0874906ab82  a\nb8a9f715dbb64fd5c56e7783c6820a61  b\n"
    );
}

/// Asserts the data archive: every directory above a file (each once, mode 0755) before what is
/// in it, then the eight files with their modes, all owned by root:root at the fixed time, the
/// binary's bytes at `usr/bin/js-agent` and every packaged text as the repository has it.
#[test]
fn the_data_archive_holds_the_eight_files_and_their_directories() {
    let bin = binary(true);
    let members = read_ar(&package(&bin, Platform::LinuxX86_64));
    let entries = read_tar_gz(&members[2].content);
    let listing: Vec<(&str, u32)> = entries.iter().map(|e| (e.path.as_str(), e.mode)).collect();
    assert_eq!(
        listing,
        [
            ("./", 0o755),
            ("./usr/", 0o755),
            ("./usr/bin/", 0o755),
            ("./usr/bin/js-agent", 0o755),
            ("./usr/lib/", 0o755),
            ("./usr/lib/systemd/", 0o755),
            ("./usr/lib/systemd/system/", 0o755),
            ("./usr/lib/systemd/system/js-agent-crun-gpu.service", 0o644),
            ("./usr/lib/systemd/system/js-agent-crun.service", 0o644),
            ("./usr/lib/systemd/system/js-agent.service", 0o644),
            ("./usr/lib/tmpfiles.d/", 0o755),
            ("./usr/lib/tmpfiles.d/js-agent-crun.conf", 0o644),
            ("./usr/lib/tmpfiles.d/js-agent.conf", 0o644),
            ("./usr/share/", 0o755),
            ("./usr/share/doc/", 0o755),
            ("./usr/share/doc/js-agent/", 0o755),
            ("./usr/share/doc/js-agent/copyright", 0o644),
            ("./usr/share/js-agent/", 0o755),
            ("./usr/share/js-agent/crun_host_setup.sh", 0o755),
        ]
    );
    for entry in &entries {
        assert_eq!(
            (entry.ids, entry.names.clone(), entry.mtime),
            (
                (0, 0),
                ("root".to_owned(), "root".to_owned()),
                u64::from(MTIME)
            ),
            "{}",
            entry.path
        );
        assert_eq!(entry.directory, entry.path.ends_with('/'), "{}", entry.path);
    }
    let content = |path: &str| {
        entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.content.clone())
    };
    assert_eq!(content("./usr/bin/js-agent"), Some(bin));
    assert_eq!(
        content("./usr/share/doc/js-agent/copyright"),
        std::fs::read(repo_root().join("LICENSE")).ok()
    );
}

/// Asserts that the control archive's md5sums are the MD5 of the data archive's files, so
/// `dpkg --verify` finds them intact.
#[test]
fn the_md5sums_match_the_data_archive() {
    let members = read_ar(&package(&binary(false), Platform::LinuxX86_64));
    let sums =
        String::from_utf8(read_tar_gz(&members[1].content)[2].content.clone()).expect("text");
    let mut from_data: Vec<String> = read_tar_gz(&members[2].content)
        .iter()
        .filter(|e| !e.directory)
        .map(|e| {
            format!(
                "{}  {}\n",
                hex::encode(Md5::digest(&e.content)),
                e.path.trim_start_matches("./")
            )
        })
        .collect();
    from_data.sort_by(|a, b| a[34..].cmp(&b[34..]));
    assert_eq!(sums, from_data.concat());
}

/// Asserts the table of files: eight absolute paths, none twice, the program the only one with
/// no text, the texts the repository's files, and the modes the units' and scripts' (only the
/// program and the setup script execute).
#[test]
fn the_files_table_names_the_eight_files_and_their_sources() {
    assert_eq!(FILES.len(), 8);
    let mut paths: Vec<&str> = FILES.iter().map(|f| f.path).collect();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(paths.len(), 8);
    assert!(FILES.iter().all(|f| f.path.starts_with('/')));
    let sources = [
        (
            "/usr/lib/systemd/system/js-agent-crun.service",
            "packaging/js-agent/deb/js-agent-crun.service",
        ),
        (
            "/usr/lib/systemd/system/js-agent-crun-gpu.service",
            "packaging/js-agent/deb/js-agent-crun-gpu.service",
        ),
        (
            "/usr/lib/tmpfiles.d/js-agent-crun.conf",
            "packaging/js-agent/deb/tmpfiles.conf",
        ),
        (
            "/usr/lib/systemd/system/js-agent.service",
            "packaging/js-agent/js-agent.service",
        ),
        (
            "/usr/lib/tmpfiles.d/js-agent.conf",
            "packaging/js-agent/tmpfiles.conf",
        ),
        (
            "/usr/share/js-agent/crun_host_setup.sh",
            "packaging/js-agent/crun_host_setup.sh",
        ),
        ("/usr/share/doc/js-agent/copyright", "LICENSE"),
    ];
    for (path, source) in sources {
        let file = FILES.iter().find(|f| f.path == path).expect(path);
        assert_eq!(
            file.text,
            Some(
                std::fs::read_to_string(repo_root().join(source))
                    .expect(source)
                    .as_str()
            ),
            "{path}"
        );
    }
    let program = FILES
        .iter()
        .filter(|f| f.text.is_none())
        .collect::<Vec<_>>();
    assert_eq!(program.len(), 1);
    assert_eq!(
        (program[0].path, program[0].mode),
        ("/usr/bin/js-agent", 0o755)
    );
    let executable: Vec<&str> = FILES
        .iter()
        .filter(|f| f.mode == 0o755)
        .map(|f| f.path)
        .collect();
    assert_eq!(
        executable,
        [
            "/usr/bin/js-agent",
            "/usr/share/js-agent/crun_host_setup.sh"
        ]
    );
}

/// Asserts the marker: the version, a newline and the runtimes line's start; a binary of
/// version 1.2.30 does not pass for 1.2.3.
#[test]
fn the_marker_keeps_a_longer_patch_number_from_passing() {
    assert_eq!(version_marker(VERSION), "1.2.3\nruntimes: docker");
    assert!(holds(
        b"..1.2.3\nruntimes: docker crun",
        version_marker(VERSION).as_bytes()
    ));
    assert!(!holds(
        b"..1.2.30\nruntimes: docker",
        version_marker(VERSION).as_bytes()
    ));
    assert!(!holds(b"short", version_marker(VERSION).as_bytes()));
}

/// Asserts the platforms' spellings and Debian architectures.
#[test]
fn a_platform_has_its_spelling_and_its_debian_architecture() {
    let all: Vec<(&str, &str)> = Platform::ALL
        .iter()
        .map(|p| (p.name(), p.architecture()))
        .collect();
    assert_eq!(all, [("linux-arm64", "arm64"), ("linux-x86_64", "amd64")]);
    assert_eq!(
        file_name(VERSION, Platform::LinuxX86_64),
        "js-agent_1.2.3_amd64.deb"
    );
    assert_eq!(
        file_name(VERSION, Platform::LinuxArm64),
        "js-agent_1.2.3_arm64.deb"
    );
}

/// The arguments for a run in `dir`.
fn args(dir: &TestDir, bin: &str, platform: Platform) -> DebArgs {
    DebArgs {
        binary: dir.join(bin),
        version: VERSION,
        platform,
        out: dir.join("out"),
    }
}

/// Asserts a run: the package is written under its name, the printed line gives its SHA-256 and
/// size, and a second run into the same directory is refused and leaves the first's bytes.
#[test]
fn a_run_writes_the_package_and_refuses_to_replace_it() {
    let dir = scratch("deb-run");
    std::fs::write(dir.join("js-agent"), binary(false)).expect("binary");
    let args = args(&dir, "js-agent", Platform::LinuxX86_64);
    let line = run(&args).expect("the run passes");
    let written = std::fs::read(dir.join("out/js-agent_1.2.3_amd64.deb")).expect("the package");
    assert_eq!(written, package(&binary(false), Platform::LinuxX86_64));
    assert_eq!(
        line,
        format!(
            "js-agent_1.2.3_amd64.deb: sha256 {}, {} bytes",
            hex::encode(Sha256::digest(&written)),
            written.len()
        )
    );
    let Err(Error::Io { path, why }) = run(&args) else {
        panic!("a second run must fail");
    };
    assert!(path.ends_with("js-agent_1.2.3_amd64.deb"), "{path}");
    assert!(why.contains("exists"), "{why}");
    assert_eq!(
        std::fs::read(dir.join("out/js-agent_1.2.3_amd64.deb")).expect("the package"),
        written
    );
}

/// Asserts that a binary that does not hold the version's marker (built as another version) is
/// refused by name, before the output directory is made.
#[test]
fn a_binary_of_another_version_is_refused_and_nothing_is_written() {
    let dir = scratch("deb-marker");
    std::fs::write(dir.join("js-agent"), b"1.2.30\nruntimes: docker").expect("binary");
    let result = run(&args(&dir, "js-agent", Platform::LinuxArm64));
    assert_eq!(
        result,
        Err(Error::VersionNotInBinary {
            path: dir.join("js-agent").display().to_string(),
            version: "1.2.3".to_owned(),
        })
    );
    assert!(!dir.join("out").exists());
}

/// Asserts the I/O failures: a missing binary and an output directory that cannot be made are
/// [`Error::Io`] naming the path.
#[test]
fn a_missing_binary_and_an_unmakeable_directory_are_io_errors() {
    let dir = scratch("deb-io");
    let Err(Error::Io { path, .. }) = run(&args(&dir, "absent", Platform::LinuxArm64)) else {
        panic!("a missing binary must fail");
    };
    assert_eq!(path, dir.join("absent").display().to_string());
    std::fs::write(dir.join("js-agent"), binary(false)).expect("binary");
    std::fs::write(dir.join("out"), "a file").expect("file");
    let Err(Error::Io { path, .. }) = run(&args(&dir, "js-agent", Platform::LinuxArm64)) else {
        panic!("an output that is a file must fail");
    };
    assert_eq!(path, dir.join("out").display().to_string());
}

/// A sink that takes `room` bytes and then refuses every write, as a full disk does.
struct Full {
    room: usize,
}

impl Write for Full {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.room == 0 {
            return Err(std::io::Error::other("the sink is full"));
        }
        let taken = bytes.len().min(self.room);
        self.room -= taken;
        Ok(taken)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that a sink that refuses the stream is an error naming what was being written: at
/// the first entry when the sink takes nothing, and at the end of the archive when it takes the
/// gzip header and no more (the compressed bytes are only flushed there).
#[test]
fn a_sink_that_fails_is_an_error_naming_what_was_written() {
    let entries: Vec<super::Entry> = vec![("./a".to_owned(), 0o644, Some(b"text".to_vec()))];
    let Err(Error::Io { path, why }) = tar_gz_to(Full { room: 0 }, &entries) else {
        panic!("a full sink must fail");
    };
    assert_eq!((path.as_str(), why.as_str()), ("./a", "the sink is full"));
    let Err(Error::Io { path, why }) = tar_gz_to(Full { room: 10 }, &entries) else {
        panic!("a sink that fills at the end must fail");
    };
    assert_eq!(
        (path.as_str(), why.as_str()),
        ("the tar archive", "the sink is full")
    );
}
