//! The commit of a run's collected outputs to a put directory, on a scratch directory with a
//! `ready` that records what it saw.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use js_testkit::dir::TestDir;
use rustix::fs::{CWD, FileType, Mode, mknodat};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use super::commit::commit_with;
use super::{Committed, PutTarget, commit};

/// The put directory `cas/b/r1/<nonce>` under `root`, not made yet.
fn target(root: &Path, nonce: &str) -> PutTarget {
    PutTarget {
        dir: root.join("cas/b/r1").join(nonce),
        key_prefix: format!("cas/b/r1/{nonce}"),
    }
}

/// The names `dir` holds, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A collected tree as the runtime leaves it: a file at the top, a directory holding a file,
/// an empty directory.
fn collected(dir: &Path) -> PathBuf {
    let into = dir.join("outputs");
    fs::create_dir_all(into.join("sub")).unwrap();
    fs::create_dir_all(into.join("empty")).unwrap();
    fs::write(into.join("a.txt"), "alpha").unwrap();
    fs::write(into.join("sub/b.txt"), "beta").unwrap();
    into
}

fn none(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Each entry of the tar `bytes`: its path, whether it is a directory, and what it holds.
fn entries(bytes: &[u8]) -> Vec<(String, bool, String)> {
    let mut archive = tar::Archive::new(bytes);
    let mut seen = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().display().to_string();
        let dir = entry.header().entry_type().is_dir();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        seen.push((path, dir, text));
    }
    seen
}

/// Proves the three names come in the order tierstore's put ingest needs, each alone in the
/// put directory: `ready` is given the directory once, made and still empty; then
/// `pack.part` only, then `pack.sha256-<hex>.part` only, then `pack` only, where `<hex>` is
/// the SHA-256 of what `pack` holds, and the commit answers that key, digest and length.
/// Catches a `pack` published before its digest was declared (the node refuses it after a
/// minute), a digest that is not of the bytes written, and a directory handed over after a
/// write.
#[test]
fn commit_renames_in_the_order_the_ingest_needs() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-order");
    let from = collected(scratch.path());
    let to = target(&scratch.path().join("put"), "n1");
    let handed = RefCell::new(Vec::new());
    let ready = |dir: &Path| {
        handed.borrow_mut().push((dir.to_owned(), names(dir)));
        Ok(())
    };
    let mut steps = Vec::new();
    let done = commit_with(&from, &to, &ready, &mut |dir| steps.push(names(dir))).unwrap();
    assert_eq!(
        handed.into_inner(),
        [(to.dir.clone(), Vec::<String>::new())]
    );
    let bytes = fs::read(to.dir.join("pack")).unwrap();
    let sha256 = hex::encode(Sha256::digest(&bytes));
    assert_eq!(
        steps,
        [
            vec!["pack.part".to_owned()],
            vec![format!("pack.sha256-{sha256}.part")],
            vec!["pack".to_owned()],
        ]
    );
    let want = Committed {
        key: "cas/b/r1/n1/pack".to_owned(),
        sha256,
        bytes: bytes.len() as u64,
    };
    assert_eq!(done, want);
}

/// Proves the tar holds exactly the collected tree, each entry at its path below the
/// collected directory, in name order: the empty directory, the file at the top, the
/// directory and its file, with what each file holds. Catches a missing, renamed or extra
/// entry (the collected directory itself among them).
#[test]
fn the_tar_holds_the_collected_files_at_their_paths() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-tar");
    let from = collected(scratch.path());
    let to = target(&scratch.path().join("put"), "n1");
    commit(&from, &to, &none).unwrap();
    let bytes = fs::read(to.dir.join("pack")).unwrap();
    let entry = |path: &str, dir: bool, text: &str| (path.to_owned(), dir, text.to_owned());
    assert_eq!(
        entries(&bytes),
        [
            entry("a.txt", false, "alpha"),
            entry("empty", true, ""),
            entry("sub", true, ""),
            entry("sub/b.txt", false, "beta"),
        ]
    );
}

/// A collected tree holding `names` made in the order given (a name ending in `/` is a
/// directory holding a file `f`) and a 1 MiB file of zeros; when `other` is set, every file
/// and directory has another time, the files another mode, and the zeros are a hole.
fn tree_in(dir: &Path, names: &[&str], other: bool) -> PathBuf {
    let into = dir.join("outputs");
    fs::create_dir_all(&into).unwrap();
    let mut made = Vec::new();
    for name in names {
        match name.strip_suffix('/') {
            Some(sub) => {
                fs::create_dir(into.join(sub)).unwrap();
                fs::write(into.join(sub).join("f"), *name).unwrap();
                made.push(into.join(sub).join("f"));
                made.push(into.join(sub));
            }
            None => {
                fs::write(into.join(name), *name).unwrap();
                made.push(into.join(name));
            }
        }
    }
    let zeros = into.join("zeros");
    match other {
        true => File::create(&zeros).unwrap().set_len(1 << 20).unwrap(),
        false => fs::write(&zeros, vec![0u8; 1 << 20]).unwrap(),
    }
    made.push(zeros);
    if other {
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        for path in &made {
            File::open(path).unwrap().set_modified(later).unwrap();
            if path.is_file() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
    }
    into
}

/// Proves the same collected outputs make the same bytes, and so the same digest, however
/// they were made: names made in other orders (the tar lists them in name order), files and
/// directories with other times, files with other modes, and a file of zeros written out or
/// left as a hole (every file is a plain entry, never a sparse one). Catches a tar whose
/// digest changes from run to run of one build, with the file system's listing order, or
/// with how a file was written.
#[test]
fn the_same_outputs_make_the_same_digest() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-same");
    let first = tree_in(&scratch.path().join("one"), &["b", "d/", "a", "c/"], false);
    let second = tree_in(&scratch.path().join("two"), &["c/", "a", "d/", "b"], true);
    let root = scratch.path().join("put");
    let one = commit(&first, &target(&root, "n1"), &none).unwrap();
    let two = commit(&second, &target(&root, "n2"), &none).unwrap();
    assert_eq!(one.sha256, two.sha256);
    assert_eq!(one.bytes, two.bytes);
    let bytes = fs::read(target(&root, "n2").dir.join("pack")).unwrap();
    let mut archive = tar::Archive::new(bytes.as_slice());
    let listed: Vec<(String, tar::EntryType)> = archive
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            (path, entry.header().entry_type())
        })
        .collect();
    let file = tar::EntryType::Regular;
    let dir = tar::EntryType::Directory;
    let want = [
        ("a", file),
        ("b", file),
        ("c", dir),
        ("c/f", file),
        ("d", dir),
        ("d/f", file),
        ("zeros", file),
    ]
    .map(|(path, kind)| (path.to_owned(), kind));
    assert_eq!(listed, want);
}

/// Proves an empty collected directory (a declared directory the job left empty) is still
/// committed, as the empty tar: 1024 zero bytes, declared and published like any other.
/// Catches a commit that refuses, or publishes nothing, for a run that made no files.
#[test]
fn an_empty_collected_directory_is_the_empty_tar() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-empty");
    let from = scratch.path().join("outputs");
    fs::create_dir_all(&from).unwrap();
    let to = target(&scratch.path().join("put"), "n1");
    let done = commit(&from, &to, &none).unwrap();
    let bytes = fs::read(to.dir.join("pack")).unwrap();
    assert_eq!(bytes, vec![0u8; 1024]);
    assert_eq!(done.sha256, hex::encode(Sha256::digest(&bytes)));
    assert_eq!(done.bytes, 1024);
}

/// Proves a path longer than a tar header's 100-byte name field is kept whole, the directory
/// and the file in it, when the tar is read back. Catches a long output path cut short.
#[test]
fn a_long_path_is_kept_whole() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-long");
    let from = scratch.path().join("outputs");
    let long = "d".repeat(120);
    fs::create_dir_all(from.join(&long)).unwrap();
    fs::write(from.join(&long).join("file"), "deep").unwrap();
    let to = target(&scratch.path().join("put"), "n1");
    commit(&from, &to, &none).unwrap();
    let bytes = fs::read(to.dir.join("pack")).unwrap();
    let want = [
        (long.clone(), true, String::new()),
        (format!("{long}/file"), false, "deep".to_owned()),
    ];
    assert_eq!(entries(&bytes), want);
}

/// Proves a collected path that is not there fails the commit naming it, with the put
/// directory left holding nothing. Catches a missing source turned into an empty object.
#[test]
fn a_missing_collected_path_is_named_and_nothing_is_left() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-missing");
    let from = scratch.path().join("outputs");
    let to = target(&scratch.path().join("put"), "n1");
    let why = commit(&from, &to, &none).unwrap_err();
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
    let said = format!("{}: {}", from.display(), io::Error::from(Errno::NOENT));
    assert_eq!(why.to_string(), said);
    assert_eq!(names(&to.dir), Vec::<String>::new());
}

/// Proves one collected file is committed as it is, not in a tar: `pack` holds exactly its
/// bytes and the digest is theirs. Catches a tar around a single output, which the build
/// service would read as a broken pack.
#[test]
fn a_single_file_is_committed_as_it_is() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-file");
    let from = collected(scratch.path()).join("sub/b.txt");
    let to = target(&scratch.path().join("put"), "n1");
    let done = commit(&from, &to, &none).unwrap();
    assert_eq!(fs::read(to.dir.join("pack")).unwrap(), b"beta");
    assert_eq!(done.sha256, hex::encode(Sha256::digest(b"beta")));
    assert_eq!(done.bytes, 4);
}

/// Proves a link in the collected tree to a file outside it is refused, by its path, and not
/// followed: the commit fails and the put directory holds nothing (no `pack`, no `.part`
/// name). A collected single file that is a link is refused the same way. Catches a tar that
/// carries a host file the link names.
#[test]
fn a_link_in_the_collected_tree_is_not_followed_and_is_refused() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-link");
    let from = collected(scratch.path());
    let host = scratch.path().join("host-secret");
    fs::write(&host, "secret").unwrap();
    let link = from.join("sub/to-host");
    symlink(&host, &link).unwrap();
    let to = target(&scratch.path().join("put"), "n1");
    let why = commit(&from, &to, &none).unwrap_err();
    assert_eq!(
        why.to_string(),
        format!("{}: a link, not followed", link.display())
    );
    assert_eq!(names(&to.dir), Vec::<String>::new());

    let to = target(&scratch.path().join("put"), "n2");
    let why = commit(&link, &to, &none).unwrap_err();
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", link.display())),
        "{why}"
    );
    assert_eq!(names(&to.dir), Vec::<String>::new());
}

/// Proves something in the collected tree that is neither a regular file nor a directory (a
/// named pipe) is refused by its path once `pack.part` was begun, and the part is removed:
/// the put directory holds nothing. Catches a half-written part left behind, and a commit
/// that waits on a pipe.
#[test]
fn a_pipe_in_the_collected_tree_is_refused_and_its_part_removed() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-pipe");
    let from = collected(scratch.path());
    let pipe = from.join("sub/pipe");
    mknodat(CWD, &pipe, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
    let to = target(&scratch.path().join("put"), "n1");
    let why = commit(&from, &to, &none).unwrap_err();
    assert_eq!(
        why.to_string(),
        format!("{}: not a regular file or a directory", pipe.display())
    );
    assert_eq!(names(&to.dir), Vec::<String>::new());
}

/// Proves a commit that fails leaves nothing the node could take: when `ready` fails nothing
/// is written and its error is the commit's; when the last rename fails (a directory named
/// `pack` is in the way once the digest was declared) the declared name and `pack.part` are
/// removed, and what is at `pack` is left as it was. Catches a half object the node could
/// store, and a cleanup that removes `pack`.
#[test]
fn a_failed_write_leaves_no_pack_and_no_declared_name() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-failed");
    let from = collected(scratch.path());
    let root = scratch.path().join("put");
    let to = target(&root, "n1");
    let refuse = |_: &Path| Err(io::Error::other("not the node's"));
    let why = commit(&from, &to, &refuse).unwrap_err();
    assert_eq!(why.to_string(), "not the node's");
    assert_eq!(names(&to.dir), Vec::<String>::new());

    let to = target(&root, "n2");
    let mut step = 0;
    let mut in_the_way = |dir: &Path| {
        step += 1;
        if step == 2 {
            fs::create_dir_all(dir.join("pack/held")).unwrap();
        }
    };
    commit_with(&from, &to, &none, &mut in_the_way).unwrap_err();
    assert_eq!(names(&to.dir), ["pack"]);
    assert_eq!(names(&to.dir.join("pack")), ["held"]);
}

/// Proves `pack` is mode 0640 (the agent's to write, the node's group's to read): the part
/// is opened `0600` and its mode set on the open file, so the umask has no say, and the mode
/// is read back after the commit (the umask, which the whole test binary shares, is never
/// changed). Catches the 0600 file the node cannot read, which it leaves and tries again
/// forever.
#[test]
fn the_pack_is_readable_by_its_group() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-mode");
    let from = collected(scratch.path());
    let to = target(&scratch.path().join("put"), "n1");
    commit(&from, &to, &none).unwrap();
    let mode = fs::metadata(to.dir.join("pack"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o640);
}

/// Proves a second commit to a put directory that still holds the first one's `pack` fails
/// before it hands the directory over or writes anything, and the first object is unchanged.
/// Catches an overwrite of a committed key.
#[test]
fn commit_twice_to_the_same_dir_fails_before_it_writes() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-twice");
    let from = collected(scratch.path());
    let to = target(&scratch.path().join("put"), "n1");
    commit(&from, &to, &none).unwrap();
    let first = fs::read(to.dir.join("pack")).unwrap();
    fs::write(from.join("a.txt"), "changed").unwrap();
    let handed = RefCell::new(0);
    let ready = |_: &Path| {
        *handed.borrow_mut() += 1;
        Ok(())
    };
    let why = commit(&from, &to, &ready).unwrap_err();
    assert_eq!(why.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        why.to_string(),
        format!("{} already holds pack", to.dir.display())
    );
    assert_eq!(handed.into_inner(), 0);
    assert_eq!(names(&to.dir), ["pack"]);
    assert_eq!(fs::read(to.dir.join("pack")).unwrap(), first);
}
