//! The Linux js-agent archive gains one member, `updater.d/js-agent.toml`, and keeps its name,
//! its top directory and its other members. Read across versions both ways:
//!
//! - an archive of this change, read as a previous release is read (docs/release.md: unpack, take
//!   `<archive name>/js-agent` and `LICENSE` by name), gives the same binary and license, and
//!   the new member is one more file that reader passes over;
//! - an archive of a previous release (the binary and `LICENSE` only), read as this change reads
//!   it (the binary by name, and the declaration by `updater.d/js-agent.toml`), gives its binary,
//!   and no declaration: the absence is said as such, not taken for another file or an error.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use js_testkit::dir::TestDir;
use xtask::release::stage::{DECLARATION_MEMBER, DECLARATION_SOURCE, StageArgs, run};

const TARGET: &str = "x86_64-unknown-linux-musl";

/// A `.tar.gz` of the directory `top` under `parent`, its members named `top/...`.
fn archive(parent: &Path, top: &str) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    tar.append_dir_all(top, parent.join(top)).expect("archive");
    tar.into_inner().expect("tar").finish().expect("gzip")
}

/// Every regular file of an archive, by its name in the archive.
fn files(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut found = BTreeMap::new();
    for entry in tar.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        if entry.header().entry_type().is_file() {
            let name = entry
                .path()
                .expect("name")
                .to_str()
                .expect("utf-8")
                .to_owned();
            let mut body = Vec::new();
            entry.read_to_end(&mut body).expect("body");
            found.insert(name, body);
        }
    }
    found
}

/// What a previous release's reader takes: the binary and the license, by name; it fails
/// without either and passes over anything else.
fn read_as_before(bytes: &[u8], name: &str) -> (Vec<u8>, Vec<u8>) {
    let all = files(bytes);
    let take = |member: &str| {
        all.get(&format!("{name}/{member}"))
            .cloned()
            .unwrap_or_else(|| panic!("{name}/{member} is missing: {:?}", all.keys()))
    };
    (take("js-agent"), take("LICENSE"))
}

/// What this change reads: the binary by name, and the declaration when the archive has one.
fn read_now(bytes: &[u8], name: &str) -> (Vec<u8>, Option<Vec<u8>>) {
    let all = files(bytes);
    let binary = all
        .get(&format!("{name}/js-agent"))
        .cloned()
        .expect("the binary");
    (
        binary,
        all.get(&format!("{name}/{DECLARATION_MEMBER}")).cloned(),
    )
}

/// A checkout with the agent built and the files an archive takes.
fn checkout(tmp: &TestDir) -> &Path {
    let dir = tmp.path();
    let release = dir.join(format!("target/{TARGET}/release"));
    std::fs::create_dir_all(&release).expect("target");
    std::fs::write(release.join("js-agent"), "the agent").expect("binary");
    std::fs::write(dir.join("LICENSE"), "license").expect("license");
    let declaration = dir.join(DECLARATION_SOURCE);
    std::fs::create_dir_all(declaration.parent().expect("parent")).expect("packaging");
    std::fs::write(&declaration, "name = \"js-agent\"\n").expect("declaration");
    dir
}

/// A new archive read the previous way gives the binary and license it always did; the new
/// member is the only other file. Catches a layout change (a renamed top directory, a moved
/// binary) that a host following the documented steps would trip on.
#[test]
fn a_new_archive_read_as_a_previous_release_reads_it_gives_the_same_files() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "archive-new-read-old");
    let repo = checkout(&tmp);
    run(&StageArgs {
        bin: "js-agent".to_owned(),
        target: TARGET.to_owned(),
        version: "1.4.0".parse().expect("version"),
        out: repo.join("stage"),
        repo_dir: repo.to_owned(),
    })
    .expect("staged");
    let name = format!("js-agent-1.4.0-{TARGET}");
    let bytes = archive(&repo.join("stage"), &name);
    assert_eq!(
        read_as_before(&bytes, &name),
        (b"the agent".to_vec(), b"license".to_vec())
    );
    assert_eq!(
        files(&bytes).keys().cloned().collect::<Vec<_>>(),
        [
            format!("{name}/LICENSE"),
            format!("{name}/js-agent"),
            format!("{name}/updater.d/js-agent.toml"),
        ]
    );
    let (binary, declaration) = read_now(&bytes, &name);
    assert_eq!(binary, b"the agent");
    assert_eq!(declaration.as_deref(), Some(&b"name = \"js-agent\"\n"[..]));
}

/// A previous release's archive (laid down as the release workflow did before this change: the
/// binary and the license in the named directory) read the new way gives its binary and no
/// declaration. Catches a reader that would take another file for the declaration or refuse
/// an archive without one.
#[test]
fn a_previous_archive_read_the_new_way_has_its_binary_and_no_declaration() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "archive-old-read-new");
    let name = format!("js-agent-1.3.0-{TARGET}");
    let top = tmp.path().join(&name);
    std::fs::create_dir_all(&top).expect("top");
    std::fs::write(top.join("js-agent"), "the old agent").expect("binary");
    std::fs::write(top.join("LICENSE"), "license").expect("license");
    let bytes = archive(tmp.path(), &name);
    assert_eq!(read_now(&bytes, &name), (b"the old agent".to_vec(), None));
    assert_eq!(
        read_as_before(&bytes, &name),
        (b"the old agent".to_vec(), b"license".to_vec())
    );
}
