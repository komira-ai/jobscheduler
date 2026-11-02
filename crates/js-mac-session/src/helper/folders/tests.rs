use super::*;

const FOLDERS: &str = "/private/var/folders";

fn of(dir: &str) -> Result<PathBuf, String> {
    entry(Path::new(dir), Path::new(FOLDERS)).map_err(|e| e.to_string())
}

/// Asserts that each spelling macOS answers `DARWIN_USER_DIR` with names the entry
/// under the configured folders, the `0` dropped. It catches an entry taken one level
/// too deep (only `0` removed, `C` and `T` left) or too shallow (every user's `<xx>`).
#[test]
fn the_answer_names_its_entry_under_the_folders() {
    let want = PathBuf::from("/private/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000");
    for dir in [
        "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/0/",
        "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/0",
        "/private/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/0/",
    ] {
        assert_eq!(of(dir), Ok(want.clone()), "{dir}");
    }
    assert_eq!(
        entry(
            Path::new("/var/folders/ab/c+d-e_f/0/"),
            Path::new("/scratch/folders")
        ),
        Ok(PathBuf::from("/scratch/folders/ab/c+d-e_f"))
    );
}

/// Asserts that every answer that is not exactly an entry's `0` is refused, with the
/// answer named in full. It catches a guessed or hostile path (a home, `..`, another
/// root, the folders themselves, a `T` or `C` answer) whose removal would reach
/// outside the uid's own entry.
#[test]
fn any_other_answer_is_refused() {
    for dir in [
        "",
        "var/folders/zz/abc/0/",
        "/Users/kbf-lease-1-1/0/",
        "/var/folders/",
        "/var/folders/zz/",
        "/var/folders/zz/abc/",
        "/var/folders/zz/abc/T/",
        "/var/folders/zz/abc/C/",
        "/var/folders/zz/abc/0/x",
        "/var/folders/zzz/abc/0/",
        "/var/folders/z/abc/0/",
        "/var/folders/zz/../0/",
        "/var/folders/../folders/zz/abc/0",
        "/var/folders/zz/./abc/0/",
        "/private/private/var/folders/zz/abc/0/",
        "/private/tmp/folders/zz/abc/0/",
        "/var/tmp/zz/abc/0/",
        "/var/folders/zz/a b/0/",
        "/var/folders/zz/a.b/0/",
        "/var/folders/zz/abc/1/",
        "/var/folders/../abc/0/",
        "/var//folders/zz/abc/0/",
        "/var/folders/zz/abc//0/",
        "/var/folders/zz/abc/0//",
        "/var/folders/zz//0/",
    ] {
        assert_eq!(
            of(dir),
            Err(format!(
                "the DARWIN_USER_DIR {dir} is not an entry of /var/folders; nothing is removed for it"
            )),
            "{dir}"
        );
    }
}

/// Asserts the resolver's program and argument on macOS. It catches a resolver that asks
/// for another directory (the temporary or cache one, whose parent is not the entry).
#[test]
fn the_resolver_asks_getconf_for_darwin_user_dir() {
    assert_eq!(GETCONF, ("/usr/bin/getconf", "DARWIN_USER_DIR"));
}

fn me() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

/// Asserts how the resolver runs, by running a shell as it (as the test's own uid and
/// group, which any user may become): with only the argument given, nothing of the
/// caller's environment, `/` as its working directory and an empty standard input; and
/// that its one line comes back as the path. It catches a resolver that leaks the
/// helper's environment or working directory into a process of the lease user, or one
/// that waits on input.
#[test]
fn the_resolver_runs_with_nothing_of_the_caller() {
    let (uid, gid) = me();
    let script = r#"[ "$1" = DARWIN_USER_DIR ] || exit 3
[ "$(pwd)" = / ] || exit 4
[ -z "$HOME$USER$CARGO_MANIFEST_DIR$CARGO_PKG_NAME" ] || exit 5
if read -r line; then exit 6; fi
printf '/var/folders/ab/cdef/0/\n'"#;
    assert_eq!(
        ask(
            "/bin/sh",
            &["-c", script, "sh", "DARWIN_USER_DIR"],
            uid,
            gid
        )
        .unwrap(),
        PathBuf::from("/var/folders/ab/cdef/0/")
    );
    let missing = ask("/nonexistent/getconf", &[], uid, gid).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

/// Asserts that only one non-empty line of text from a resolver that exited 0 is an
/// answer, and that every other outcome is refused naming it and what the resolver
/// said. It catches a failed, killed or garbled resolver taken as a path (and an
/// empty one taken as `/`).
#[test]
fn only_one_line_from_a_clean_exit_is_an_answer() {
    assert_eq!(
        answer(Some(0), b"/var/folders/ab/c/0/", b"").unwrap(),
        PathBuf::from("/var/folders/ab/c/0/")
    );
    for (code, stdout, stderr, want) in [
        (
            Some(1),
            &b""[..],
            &b"no such user\n"[..],
            "exited 1: no such user",
        ),
        (None, b"", b"", "ended by a signal: "),
        (
            Some(0),
            b"\xff\xfe",
            b"",
            "printed something that is not text: ",
        ),
        (Some(0), b"", b"", "printed \"\", not one path: "),
        (Some(0), b"\n", b"", "printed \"\\n\", not one path: "),
        (
            Some(0),
            b"/a\n/b\n",
            b"",
            "printed \"/a\\n/b\\n\", not one path: ",
        ),
    ] {
        assert_eq!(
            answer(code, stdout, stderr).unwrap_err().to_string(),
            want,
            "{stdout:?}"
        );
    }
}

/// Asserts that a resolver that fails is refused through [`ask`] as well, with its
/// exit and what it said. It catches a non-zero exit read as an answer.
#[test]
fn a_resolver_that_fails_is_refused() {
    let (uid, gid) = me();
    let got = ask("/bin/sh", &["-c", "echo gone >&2; exit 9"], uid, gid).unwrap_err();
    assert_eq!(got.to_string(), "exited 9: gone");
}
