//! Files that are already there: a matching one is left alone, any other is refused before
//! the setup runs or anything is written.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use super::rig::{LINE, OTHER, Rig, TOKEN, sha, status};

/// Asserts that an instance file with the right line is left alone (the same inode, the
/// same bytes), whatever owner-only or world-readable mode it has, and reported `present`.
/// It catches a rewrite of a right file and a mode rule that is too strict.
#[test]
fn a_matching_instance_file_is_left_alone() {
    for mode in [0o644, 0o600, 0o640] {
        let rig = Rig::new("enroll_instance_same");
        rig.put("instance", LINE, mode);
        let inode = fs::metadata(rig.etc().join("instance")).unwrap().ino();
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(0), "{mode:o}: {ran:?}");
        assert!(
            ran.out.contains("enroll: instance status=present path="),
            "{mode:o}: {ran:?}"
        );
        assert_eq!(
            fs::metadata(rig.etc().join("instance")).unwrap().ino(),
            inode
        );
        assert_eq!(rig.mode("instance"), mode);
        assert_eq!(rig.read("instance"), LINE);
    }
}

/// Asserts that an instance file with another line, the right line without its line break,
/// or the line twice is refused (status 2) with the path and "remove it by hand", before the
/// setup runs, and is not changed. It catches a no-clobber that clobbers and a looser match
/// than "exactly".
#[test]
fn an_instance_file_that_differs_is_refused() {
    for text in [
        "burst:i-other\n",
        "burst:i-0abc",
        "burst:i-0abc\nburst:i-0abc\n",
        "",
    ] {
        let rig = Rig::new("enroll_instance_differs");
        rig.put("instance", text, 0o644);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(2), "{text:?}: {ran:?}");
        let path = rig.host.instance_file.display().to_string();
        assert!(
            ran.err.contains(&path) && ran.err.contains("remove it by hand"),
            "{text:?}: {ran:?}"
        );
        assert_eq!(ran.out, "");
        assert_eq!(rig.read("instance"), text);
        assert_eq!(rig.setup_args(), None, "the setup ran before the refusal");
        assert_eq!(rig.names(), ["instance"]);
    }
}

/// Asserts that an instance file others can write is refused whatever it holds. It catches a
/// file that another user could have changed being trusted for the line it holds.
#[test]
fn an_instance_file_others_can_write_is_refused() {
    for mode in [0o666, 0o664, 0o646] {
        let rig = Rig::new("enroll_instance_mode");
        rig.put("instance", LINE, mode);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(2), "{mode:o}: {ran:?}");
        assert!(
            ran.err.contains(&format!("mode {mode:04o}")),
            "{mode:o}: {ran:?}"
        );
        assert_eq!(rig.setup_args(), None);
    }
}

/// Asserts that something at the instance path that is not a regular file of a sane size (a
/// directory, a symbolic link to the right file, a file of more than 4096 bytes) is
/// refused. It catches a link followed to a file somewhere else, and a read of a huge file.
#[test]
fn an_instance_path_that_is_not_a_plain_small_file_is_refused() {
    let rig = Rig::new("enroll_instance_odd");
    let path = rig.host.instance_file.clone();
    fs::create_dir(&path).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("is not a regular file"), "{ran:?}");
    fs::remove_dir(&path).unwrap();

    rig.put("real", LINE, 0o644);
    symlink(rig.etc().join("real"), &path).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("symbolic links"), "{ran:?}");
    fs::remove_file(&path).unwrap();

    rig.put("instance", &"x".repeat(5000), 0o644);
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("longer than 4096 bytes"), "{ran:?}");
    assert_eq!(rig.setup_args(), None);
}

/// Asserts that a token file that holds the token on standard input (by its sha256, so a
/// missing line break does not matter), with mode 0600 or 0400, is left alone and reported
/// `present` with the sha256. It catches a rewrite, a mismatch on the line break, and a
/// mode rule that refuses the read-only file the agent itself accepts.
#[test]
fn a_matching_token_file_is_left_alone() {
    for (text, mode) in [
        (format!("{TOKEN}\n"), 0o600),
        (TOKEN.to_owned(), 0o600),
        (format!("{TOKEN}\n"), 0o400),
    ] {
        let rig = Rig::new("enroll_token_same");
        rig.put("token", &text, mode);
        let inode = fs::metadata(rig.etc().join("token")).unwrap().ino();
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(0), "{mode:o}: {ran:?}");
        assert!(
            ran.out.contains(&format!(
                "enroll: token status=present path={} sha256={}\n",
                rig.host.token_file.display(),
                sha(TOKEN)
            )),
            "{mode:o}: {ran:?}"
        );
        assert_eq!(fs::metadata(rig.etc().join("token")).unwrap().ino(), inode);
        assert_eq!(rig.read("token"), text);
        assert_eq!(rig.mode("token"), mode);
    }
}

/// Asserts that a token file with another token (or anything else) is refused with status 2,
/// "present but differs", both digests and "remove it by hand", before the setup runs; the
/// file is not changed, and neither token is printed. It catches a replaced token and a
/// refusal that does not say what to do.
#[test]
fn a_token_file_that_differs_is_refused() {
    for text in [
        format!("{OTHER}\n"),
        "pool-token\n".to_owned(),
        String::new(),
    ] {
        let rig = Rig::new("enroll_token_differs");
        rig.put("token", &text, 0o600);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(2), "{text:?}: {ran:?}");
        let path = rig.host.token_file.display().to_string();
        assert!(
            ran.err.contains(&path)
                && ran.err.contains("present but differs")
                && ran.err.contains("remove it by hand")
                && ran.err.contains(&sha(text.trim_end()))
                && ran.err.contains(&sha(TOKEN)),
            "{text:?}: {ran:?}"
        );
        assert_eq!(ran.out, "");
        assert_eq!(rig.read("token"), text);
        assert_eq!(rig.setup_args(), None);
    }
}

/// Asserts that a token file that others can read or write, or that is a symbolic link, is
/// refused even when it holds the right token. It catches a token file that was readable by
/// others being kept as the host's secret.
#[test]
fn a_token_file_others_can_read_is_refused() {
    for mode in [0o644, 0o640, 0o660, 0o700] {
        let rig = Rig::new("enroll_token_mode");
        rig.put("token", &format!("{TOKEN}\n"), mode);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(2), "{mode:o}: {ran:?}");
        assert!(
            ran.err.contains(&format!("mode {mode:04o}")),
            "{mode:o}: {ran:?}"
        );
        assert_eq!(rig.setup_args(), None);
    }
    let rig = Rig::new("enroll_token_link");
    rig.put("real", &format!("{TOKEN}\n"), 0o600);
    symlink(rig.etc().join("real"), rig.etc().join("token")).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("symbolic links"), "{ran:?}");
}

/// Asserts that a file at the name an older version staged under (`.token.new`) neither
/// blocks the run nor is touched: the staging name is not fixed. It catches a staging name
/// another user can plant a file or a link at.
#[test]
fn a_file_at_a_guessable_staging_name_does_not_matter() {
    let rig = Rig::new("enroll_guessable_stage");
    rig.put(".token.new", "planted", 0o644);
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(rig.names(), [".token.new", "instance", "token"]);
    assert_eq!(rig.read(".token.new"), "planted");
    assert_eq!(rig.read("token"), format!("{TOKEN}\n"));
}

/// Asserts that a directory that group or others can write, a link in its place, and one that
/// is not the user's are refused (status 2) before the setup runs, naming the directory: a
/// secret is staged there and a writer could swap it. A directory that is not there yet is
/// fine (the setup makes it). It catches a staged token a stranger can replace, and a refusal
/// that comes after the setup has run.
#[test]
fn a_directory_others_can_write_is_refused() {
    let rig = Rig::new("enroll_dir_modes");
    fs::set_permissions(rig.etc(), fs::Permissions::from_mode(0o775)).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    let etc = rig.etc().display().to_string();
    assert!(
        ran.err.contains(&etc) && ran.err.contains("writable by group or others"),
        "{ran:?}"
    );
    assert_eq!(rig.setup_args(), None);
    fs::set_permissions(rig.etc(), fs::Permissions::from_mode(0o755)).unwrap();

    let mut other = Rig::new("enroll_dir_owner");
    other.host.owner += 1;
    let ran = other.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("is not owned by uid"), "{ran:?}");
    assert_eq!(other.setup_args(), None);

    let mut linked = Rig::new("enroll_dir_link");
    let real = linked.dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&real, linked.dir.path().join("etc-link")).unwrap();
    linked.host.instance_file = linked.dir.path().join("etc-link").join("instance");
    let ran = linked.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("is not a directory"), "{ran:?}");

    let mut under_file = Rig::new("enroll_dir_file");
    under_file.put("plain", "x", 0o644);
    under_file.host.token_file = under_file.etc().join("plain").join("sub").join("token");
    let ran = under_file.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert_eq!(under_file.setup_args(), None);
}

/// Asserts that a dangling link at the token path is refused like any link (status 2). It
/// catches a link that is followed to wherever it points, or taken for an absent file.
#[test]
fn a_dangling_link_at_the_token_path_is_refused() {
    let rig = Rig::new("enroll_token_dangling");
    symlink(rig.etc().join("nowhere"), rig.etc().join("token")).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("symbolic links"), "{ran:?}");
    assert_eq!(rig.setup_args(), None);
}

/// Asserts that a file that appears between the look and the write (the setup script makes
/// one) is not replaced and is reported as a failure of a step (status 1, the setup having
/// run), not as a refusal that changed nothing. It catches a race that overwrites, and a
/// failure that claims nothing was done.
#[test]
fn a_file_that_appears_before_the_write_is_kept_and_reported_as_a_failure() {
    let rig = Rig::new("enroll_raced");
    let theirs = rig.etc().join("instance");
    rig.script_with(&format!(
        "echo 'burst:other' > '{}'\nexit 0",
        theirs.display()
    ));
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(1), "{ran:?}");
    assert!(
        ran.err.contains("cannot write") && ran.err.contains("exists"),
        "{ran:?}"
    );
    assert_eq!(rig.read("instance"), "burst:other\n");
    assert_eq!(rig.names(), ["instance"]);
}

/// Asserts that the staged copy of the token is gone when the link fails (a directory is at
/// the token path by then), so a failed run leaves no secret under a name of its own. It
/// catches a staged file left behind on the error path.
#[test]
fn a_failed_write_leaves_no_staged_secret() {
    let rig = Rig::new("enroll_staged_gone");
    let theirs = rig.etc().join("token");
    rig.script_with(&format!("mkdir '{}'\nexit 0", theirs.display()));
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(1), "{ran:?}");
    assert_eq!(rig.names(), ["instance", "token"]);
    assert!(rig.etc().join("token").is_dir());
}
