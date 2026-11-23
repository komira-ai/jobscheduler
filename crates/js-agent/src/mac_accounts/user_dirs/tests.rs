//! The check of an account's per-user folders as `getconf` names them.

use std::fs;

use js_testkit::dir::TestDir;

use super::*;

/// The account the fake runner is asked for.
fn slot() -> Account {
    Account {
        name: "js-slot-3".to_owned(),
        uid: me(),
        gid: 20,
        generated_uid: String::new(),
    }
}

/// What a run of `getconf` ended with.
fn ran(code: Option<i32>, stdout: &str, stderr: &str) -> Ran {
    Ran {
        code,
        signal: code.is_none().then_some(9),
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

/// This process's own uid: the owner of every directory a test makes.
fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A per-user folders root with one slot's folder in it, `T` and `C` made.
fn folders(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-user-dirs-{name}"));
    let root = dir.join("folders");
    fs::create_dir_all(root.join("ab/slot/T")).unwrap();
    fs::create_dir_all(root.join("ab/slot/C")).unwrap();
    let root = fs::canonicalize(root).unwrap();
    (dir, root)
}

/// Asserts that a folder of the account below the root is taken as the kernel resolves it,
/// the trailing slash and line `getconf` prints dropped, through a link too.
///
/// Catches a grant spelled as `getconf` printed it, which a profile does not match.
#[test]
fn a_folder_of_the_account_below_the_root_is_taken_resolved() {
    let (dir, root) = folders("ok");
    let temp = root.join("ab/slot/T");
    let printed = format!("{}/\n", temp.display());
    assert_eq!(checked(&printed, &root, me()).unwrap(), temp);
    std::os::unix::fs::symlink(root.join("ab/slot/C"), dir.join("cache")).unwrap();
    let through = format!("{}\n", dir.join("cache").display());
    assert_eq!(
        checked(&through, &root, me()).unwrap(),
        root.join("ab/slot/C")
    );
}

/// Asserts each refusal, by its text: nothing printed, a path that does not resolve, the root
/// itself, a path outside the root, a file, and a folder of another uid.
///
/// Catches a profile that grants writes to whatever a lookup answered, such as all of
/// `/var/folders` or another account's folder.
#[test]
fn what_is_not_the_accounts_own_folder_is_refused() {
    let (dir, root) = folders("refused");
    assert_eq!(
        checked(" \n", &root, me()).unwrap_err().to_string(),
        "getconf printed no folder"
    );
    let missing = root.join("ab/slot/X");
    let why = checked(&missing.display().to_string(), &root, me()).unwrap_err();
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", missing.display())),
        "{why}"
    );
    let at_root = root.display().to_string();
    assert_eq!(
        checked(&at_root, &root, me()).unwrap_err().to_string(),
        format!("{at_root}: not below {at_root}")
    );
    let outside = fs::canonicalize(dir.path()).unwrap().display().to_string();
    assert_eq!(
        checked(&outside, &root, me()).unwrap_err().to_string(),
        format!("{outside}: not below {at_root}")
    );
    let file = root.join("ab/slot/file");
    fs::write(&file, "x").unwrap();
    let file = file.display().to_string();
    assert_eq!(
        checked(&file, &root, me()).unwrap_err().to_string(),
        format!("{file}: not a directory")
    );
    let temp = root.join("ab/slot/T").display().to_string();
    let other = me() + 1;
    assert_eq!(
        checked(&temp, &root, other).unwrap_err().to_string(),
        format!("{temp}: owned by uid {}, not uid {other}", me())
    );
}

/// Asserts that a `getconf` that exits non-zero, or is ended by a signal, fails the lookup
/// with the exact text naming the variable asked first, the account and the trimmed error
/// output, and that the runner is not asked again; the signalled run printed a valid folder,
/// which must not be taken.
///
/// Catches a lookup that takes the printed folder of a failed `getconf`, an error text that
/// drops the variable, the account or the trim, and a loop that goes on after a failure.
#[test]
fn a_getconf_that_fails_fails_the_lookup_with_its_error_text() {
    let (_dir, root) = folders("fails");
    let valid = format!("{}\n", root.join("ab/slot/T").display());
    for (code, stdout) in [(Some(1), ""), (None, valid.as_str())] {
        let mut asked = 0;
        let why = folders_via(&slot(), &root, |_| {
            asked += 1;
            Ok(ran(code, stdout, " no slot\n"))
        })
        .unwrap_err();
        assert_eq!(
            why.to_string(),
            "getconf DARWIN_USER_TEMP_DIR as js-slot-3: no slot"
        );
        assert_eq!(asked, 1);
    }
}

/// Asserts that a runner that cannot run the program fails the lookup with its own error,
/// and that a good run of both variables returns the two folders in order, having been
/// asked `getconf DARWIN_USER_TEMP_DIR` and then `getconf DARWIN_USER_CACHE_DIR`.
///
/// Catches a lookup that swallows the runner's error, and one that asks the wrong names or
/// in the wrong order.
#[test]
fn the_folders_are_asked_of_getconf_in_order_and_returned() {
    let (_dir, root) = folders("asked");
    let why = folders_via(&slot(), &root, |_| Err(io::Error::other("no spawn"))).unwrap_err();
    assert_eq!(why.to_string(), "no spawn");
    let mut commands = Vec::new();
    let got = folders_via(&slot(), &root, |command| {
        commands.push(command.to_vec());
        let folder = if command[1] == "DARWIN_USER_TEMP_DIR" {
            "T"
        } else {
            "C"
        };
        let printed = format!("{}\n", root.join("ab/slot").join(folder).display());
        Ok(ran(Some(0), &printed, ""))
    })
    .unwrap();
    assert_eq!(got, [root.join("ab/slot/T"), root.join("ab/slot/C")]);
    assert_eq!(
        commands,
        [
            ["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"],
            ["/usr/bin/getconf", "DARWIN_USER_CACHE_DIR"]
        ]
    );
}
