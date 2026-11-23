//! The commit of a run's collected output to a put directory, on a scratch directory with a
//! `ready` that records what it saw.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use super::commit::commit_with;
use super::{Committed, PutTarget, commit};

/// The put directory `cas/b/r1/<leaf>` under `root`, not made yet.
fn target(root: &Path, leaf: &str) -> PutTarget {
    PutTarget {
        dir: root.join("cas/b/r1").join(leaf),
        key_prefix: format!("cas/b/r1/{leaf}"),
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

/// A collected output as the runtime leaves it: `outputs/.kbf-out/pack` holding `bytes`.
fn collected(dir: &Path, bytes: &[u8]) -> PathBuf {
    let out = dir.join("outputs/.kbf-out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("pack"), bytes).unwrap();
    out.join("pack")
}

fn none(_: &Path) -> io::Result<()> {
    Ok(())
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
    let from = collected(scratch.path(), b"the pack");
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
    let sha256 = hex::encode(Sha256::digest(b"the pack"));
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
        bytes: 8,
    };
    assert_eq!(done, want);
}

/// Proves the output is committed as it is, not in a tar or any other wrapping: `pack` holds
/// exactly its bytes, for an empty file and for one of a megabyte and a bit (more than one
/// buffer of the copy). Catches a wrapped or cut copy, which the build service would read as
/// a broken pack.
#[test]
fn the_output_is_committed_as_it_is() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-as-is");
    let root = scratch.path().join("put");
    let big: Vec<u8> = (0..(1 << 20) + 7).map(|n: u32| (n % 251) as u8).collect();
    for (leaf, bytes) in [("empty", Vec::new()), ("big", big)] {
        let from = collected(&scratch.path().join(leaf), &bytes);
        let to = target(&root, leaf);
        let done = commit(&from, &to, &none).unwrap();
        assert_eq!(fs::read(to.dir.join("pack")).unwrap(), bytes, "{leaf}");
        assert_eq!(done.sha256, hex::encode(Sha256::digest(&bytes)), "{leaf}");
        assert_eq!(done.bytes, bytes.len() as u64, "{leaf}");
    }
}

/// Proves an output that is a link (to a host file) is refused by its path and not followed,
/// one that is a directory is refused as not a regular file, and one that is a socket is
/// refused by its path: each commit fails and leaves the put directory holding nothing (its
/// `pack.part` removed). Catches a commit that carries a host file a link names, and a
/// half-written part left behind.
#[test]
fn an_output_that_is_not_a_regular_file_is_refused() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-not-file");
    let root = scratch.path().join("put");
    let host = scratch.path().join("host-secret");
    fs::write(&host, "secret").unwrap();
    let link = scratch.path().join("link");
    symlink(&host, &link).unwrap();
    let dir = scratch.path().join("a-dir");
    fs::create_dir(&dir).unwrap();
    let socket = scratch.path().join("socket");
    let _listening = UnixListener::bind(&socket).unwrap();

    let loop_said = io::Error::from(Errno::LOOP);
    let cases = [
        ("link", &link, format!("{}: {loop_said}", link.display())),
        (
            "dir",
            &dir,
            format!("{}: not a regular file", dir.display()),
        ),
    ];
    for (leaf, from, said) in cases {
        let to = target(&root, leaf);
        let why = commit(from, &to, &none).unwrap_err();
        assert_eq!(why.to_string(), said, "{leaf}");
        assert_eq!(names(&to.dir), Vec::<String>::new(), "{leaf}");
    }
    // A socket cannot be opened at all (the error the system gives differs by system).
    let to = target(&root, "socket");
    let why = commit(&socket, &to, &none).unwrap_err();
    let named = format!("{}: ", socket.display());
    assert!(why.to_string().starts_with(&named), "{why}");
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
    let from = collected(scratch.path(), b"the pack");
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
    let from = collected(scratch.path(), b"the pack");
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
    let from = collected(scratch.path(), b"first");
    let to = target(&scratch.path().join("put"), "n1");
    commit(&from, &to, &none).unwrap();
    fs::write(&from, "second").unwrap();
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
    assert_eq!(fs::read(to.dir.join("pack")).unwrap(), b"first");
}

/// Proves a collected output that is not there fails the commit naming it, with the put
/// directory left holding nothing. Catches a missing output turned into an empty object.
#[test]
fn a_missing_output_is_named_and_nothing_is_left() {
    let scratch = TestDir::under(std::env::temp_dir(), "put-missing");
    let from = scratch.path().join("outputs/.kbf-out/pack");
    let to = target(&scratch.path().join("put"), "n1");
    let why = commit(&from, &to, &none).unwrap_err();
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
    let said = format!("{}: {}", from.display(), io::Error::from(Errno::NOENT));
    assert_eq!(why.to_string(), said);
    assert_eq!(names(&to.dir), Vec::<String>::new());
}

/// Proves a named pipe at the collected path, with no writer, is refused as not a regular
/// file at once, and leaves nothing in the put directory: it is opened `O_NONBLOCK`, so the
/// open does not wait for a writer. Catches a commit that blocks for ever on a pipe a job left
/// where its output should be (Linux only: the test makes the pipe with `mknodat`).
#[cfg(target_os = "linux")]
#[test]
fn a_pipe_at_the_collected_path_is_refused_at_once() {
    use rustix::fs::{CWD, FileType, Mode, mknodat};

    let scratch = TestDir::under(std::env::temp_dir(), "put-pipe");
    let pipe = scratch.path().join("pipe");
    mknodat(CWD, &pipe, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
    let to = target(&scratch.path().join("put"), "n1");
    let why = commit(&pipe, &to, &none).unwrap_err();
    assert_eq!(
        why.to_string(),
        format!("{}: not a regular file", pipe.display())
    );
    assert_eq!(names(&to.dir), Vec::<String>::new());
}
