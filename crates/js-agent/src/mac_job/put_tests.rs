//! The commit of a job's outputs to a put directory, on a scratch directory.

use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::symlink;
use std::path::Path;

use js_testkit::dir::TestDir;
use sha2::{Digest as _, Sha256};

use super::{Committed, commit};

/// A work folder with a file, a folder holding a file and a link, and a link at its top.
fn work(dir: &Path) -> std::path::PathBuf {
    let work = dir.join("work");
    fs::create_dir_all(work.join("out/sub")).unwrap();
    fs::write(work.join("out/a.txt"), "alpha").unwrap();
    fs::write(work.join("out/sub/b.txt"), "beta").unwrap();
    symlink("../a.txt", work.join("out/sub/to-a")).unwrap();
    fs::write(work.join("log.txt"), "log").unwrap();
    symlink("out", work.join("link-out")).unwrap();
    work
}

fn none(_: &Path) -> io::Result<()> {
    Ok(())
}

#[test]
fn the_outputs_are_one_committed_tar_whose_digest_the_name_declared() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-commit");
    let work = work(scratch.path());
    let root = scratch.path().join("put");
    let outputs = ["out".to_owned(), "log.txt".to_owned()];
    let done = commit(&work, &outputs, &root, "93-tree", "n1", &none).expect("committed");
    let dir = root.join("cas/b/93-tree/n1");
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["pack"]);
    let bytes = fs::read(dir.join("pack")).unwrap();
    assert_eq!(
        done,
        Committed {
            key: "cas/b/93-tree/n1/pack".to_owned(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
        }
    );
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut seen = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().display().to_string();
        let link = entry.link_name().unwrap().map(|l| l.display().to_string());
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        seen.push((path, link, text));
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            ("log.txt".to_owned(), None, "log".to_owned()),
            ("out/".to_owned(), None, String::new()),
            ("out/a.txt".to_owned(), None, "alpha".to_owned()),
            ("out/sub".to_owned(), None, String::new()),
            ("out/sub/b.txt".to_owned(), None, "beta".to_owned()),
            (
                "out/sub/to-a".to_owned(),
                Some("../a.txt".to_owned()),
                String::new()
            ),
        ]
    );
}

#[test]
fn the_put_directory_is_handed_over_before_anything_is_written_in_it() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-ready");
    let work = work(scratch.path());
    let root = scratch.path().join("put");
    let ready = |dir: &Path| {
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
        Err(io::Error::other("not the node's"))
    };
    let why = commit(&work, &["log.txt".to_owned()], &root, "b", "n", &ready).expect_err("refused");
    assert_eq!(why.to_string(), "not the node's");
    assert!(!root.join("cas/b/b/n/pack.part").exists());
}

#[test]
fn an_output_through_a_link_or_a_missing_one_is_refused_before_anything_is_made() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-refused");
    let work = work(scratch.path());
    let root = scratch.path().join("put");
    let why = commit(
        &work,
        &["link-out/a.txt".to_owned()],
        &root,
        "b",
        "n",
        &none,
    )
    .expect_err("link");
    assert_eq!(
        why.to_string(),
        format!("{} is a link", work.join("link-out").display())
    );
    let why = commit(&work, &["gone".to_owned()], &root, "b", "n", &none).expect_err("missing");
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
    assert!(!root.exists());
}
