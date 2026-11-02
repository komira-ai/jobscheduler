use super::*;

const FOLDERS: &str = "/private/var/folders";

fn of(dir: &str) -> Result<PathBuf, String> {
    entry(Path::new(dir), Path::new(FOLDERS))
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
