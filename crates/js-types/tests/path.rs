//! The path rules the server and the agent share ([`js_types::path`]), each at both sides of
//! its bound, and the character class they refuse ([`js_types::text::hidden`]).

use js_types::path::{
    MAX_PATH_LEN, PathFault, RUNTIME_PLACES, absolute, disjoint, distinct, relative, target,
};
use js_types::text::hidden;

/// `/` and then `len - 1` bytes of the letter `a`: an absolute path of `len` bytes.
fn rooted(len: usize) -> String {
    format!("/{}", "a".repeat(len - 1))
}

/// Asserts each rule of a relative path at its edge: the empty path only when allowed, the
/// length bound, every refused character (non-ASCII, `:`, `,`, `\`, a control and a format
/// character), a leading `/` and a `..` part, and an empty or `.` part. It catches a rule
/// lost in the move from the agent wire, where these were a run directory's rules first.
#[test]
fn a_relative_path_keeps_the_run_directory_rules() {
    assert_eq!(relative("", true), Ok(()));
    assert_eq!(relative("", false), Err(PathFault::Empty));
    assert_eq!(relative("a/b.c/d-e_f", false), Ok(()));
    assert_eq!(relative(&"a".repeat(MAX_PATH_LEN), false), Ok(()));
    let long = "a".repeat(MAX_PATH_LEN + 1);
    assert_eq!(relative(&long, false), Err(PathFault::TooLong));
    for bad in [
        "caf\u{e9}",
        "a:b",
        "a,b",
        "a\\b",
        "a\nb",
        "a\u{202e}b",
        "a\0b",
    ] {
        assert_eq!(relative(bad, false), Err(PathFault::BadChars), "{bad:?}");
    }
    for bad in ["/a", "..", "a/../b", "a/.."] {
        assert_eq!(relative(bad, false), Err(PathFault::Escapes), "{bad}");
    }
    for bad in ["a//b", "a/", "./a", "a/./b", "."] {
        assert_eq!(relative(bad, false), Err(PathFault::Empty), "{bad}");
    }
}

/// Asserts each rule of an absolute path (a program, a host path) at its edge: `/` alone, an
/// empty path, a relative one, a `..` part (found before an empty part beside it), an empty
/// or `.` part and a `/` at the end; 4096 bytes accepted and 4097 refused; NUL, a control
/// and a format character refused, while `:`, `,`, a space and non-ASCII letters, which a
/// host directory may hold, pass. It catches a host path that is not in normal form, which
/// would let one directory be named in two ways, and a rule that refuses a real directory
/// name.
#[test]
fn an_absolute_path_is_in_normal_form() {
    for good in ["/a", "/srv/trees/ab:cd,ef", "/data/caf\u{e9}", "/a b/c"] {
        assert_eq!(absolute(good), Ok(()), "{good}");
    }
    assert_eq!(absolute(&rooted(MAX_PATH_LEN)), Ok(()));
    assert_eq!(absolute(&rooted(MAX_PATH_LEN + 1)), Err(PathFault::TooLong));
    for bad in ["", "/", "/a/", "/a//b", "/a/./b", "/."] {
        assert_eq!(absolute(bad), Err(PathFault::Empty), "{bad:?}");
    }
    for bad in ["a", "a/b", "./a", "/..", "/a/../b", "/a/../"] {
        assert_eq!(absolute(bad), Err(PathFault::Escapes), "{bad}");
    }
    for bad in ["/a\0b", "/a\nb", "/a\u{7f}", "/a\u{202e}b"] {
        assert_eq!(absolute(bad), Err(PathFault::BadChars), "{bad:?}");
    }
}

/// Asserts that the runtime's places are pinned, and that a target at each, below each, and
/// at each in upper case is refused, while a sibling that only shares a prefix (`/procfs`,
/// `/runner`, `/var`) passes. It catches a mount that would cover `/proc`, the run files or
/// the job's scratch, and a prefix match that would refuse an innocent directory.
#[test]
fn a_target_is_outside_the_runtime_places() {
    assert_eq!(
        RUNTIME_PLACES,
        [
            "/proc",
            "/dev",
            "/sys",
            "/run/js-files",
            "/scratch",
            "/root",
            "/tmp",
            "/var/tmp",
            "/run"
        ]
    );
    for place in RUNTIME_PLACES {
        let below = format!("{place}/x");
        let upper = place.to_ascii_uppercase();
        for bad in [place, below.as_str(), upper.as_str()] {
            assert_eq!(target(bad), Err(PathFault::RuntimePlace), "{bad}");
        }
    }
    for good in [
        "/work",
        "/work/sdk",
        "/procfs",
        "/runner",
        "/var",
        "/var/tmpx",
        "/tmp2",
    ] {
        assert_eq!(target(good), Ok(()), "{good}");
    }
}

/// Asserts the rest of a target's rules at their edges: `/` alone and an empty target, a
/// relative one and a `..` part, an empty part, `:`, `,`, `\` and non-ASCII refused, and the
/// bound counted with the leading `/` (4096 bytes accepted, 4097 refused). It catches a
/// target that could add a field to a mount option, two spellings of one target, and a
/// bound off by the leading `/`. It also pins the order of the faults the docs give: a
/// refused character before a `..` part, and a `..` part before an empty one.
#[test]
fn a_target_is_an_absolute_ascii_path_in_normal_form() {
    assert_eq!(target(&rooted(MAX_PATH_LEN)), Ok(()));
    assert_eq!(target(&rooted(MAX_PATH_LEN + 1)), Err(PathFault::TooLong));
    for bad in ["", "/", "/work/", "/work//sdk", "/work/./sdk"] {
        assert_eq!(target(bad), Err(PathFault::Empty), "{bad:?}");
    }
    for bad in ["work", "work/sdk", "/work/../proc", "/.."] {
        assert_eq!(target(bad), Err(PathFault::Escapes), "{bad}");
    }
    for bad in ["/a:b", "/a,b", "/a\\b", "/caf\u{e9}", "/a\0b"] {
        assert_eq!(target(bad), Err(PathFault::BadChars), "{bad:?}");
    }
    assert_eq!(target("/caf\u{e9}/.."), Err(PathFault::BadChars));
    assert_eq!(target("/a/..//b"), Err(PathFault::Escapes));
    assert_eq!(relative("a/../b//", false), Err(PathFault::Escapes));
    assert_eq!(absolute("/a/..//b"), Err(PathFault::Escapes));
}

/// Asserts that `disjoint` refuses two equal paths (in any case) as a repeat and one below
/// another (either order, in any case) as nested, while siblings that share a name prefix,
/// and an empty path beside another, pass; and that `distinct` refuses only the repeat, so
/// a mount may lie below another. It catches the two checks swapped, a case-sensitive
/// comparison that macOS would see as one path, and a prefix compare that treats `a/bc` as
/// below `a/b`.
#[test]
fn lists_of_paths_refuse_repeats_and_disjoint_ones_nesting() {
    assert_eq!(disjoint(&["a", "A"]), Err(PathFault::Duplicate));
    assert_eq!(disjoint(&["a/b", "a"]), Err(PathFault::Nested));
    assert_eq!(disjoint(&["a", "A/b"]), Err(PathFault::Nested));
    assert_eq!(disjoint(&["", ""]), Err(PathFault::Duplicate));
    assert_eq!(disjoint(&["a/b", "a/bc", "", "c"]), Ok(()));
    assert_eq!(disjoint(&[]), Ok(()));
    assert_eq!(distinct(&["/work", "/work/sdk", "/WORK/lib"]), Ok(()));
    assert_eq!(
        distinct(&["/work/sdk", "/a", "/Work/SDK"]),
        Err(PathFault::Duplicate)
    );
    assert_eq!(distinct(&[]), Ok(()));
}

/// Asserts the fixed text of each fault: built from the rule alone, never from the path. It
/// catches a fault whose text would repeat a peer's bytes into a log.
#[test]
fn a_fault_names_its_rule_alone() {
    let texts = [
        (PathFault::Empty, "is empty or has an empty or `.` part"),
        (PathFault::TooLong, "is longer than the cap"),
        (PathFault::BadChars, "holds a character that is not allowed"),
        (PathFault::Escapes, "leaves its root"),
        (PathFault::Duplicate, "repeats another path"),
        (PathFault::Nested, "lies below another path"),
        (
            PathFault::RuntimePlace,
            "names a place the runtime keeps for itself",
        ),
    ];
    for (fault, text) in texts {
        assert_eq!(fault.to_string(), text);
    }
}

/// Asserts that `hidden` is true for a control character (NUL, a line feed, DEL), for a
/// format character at each end of a range (U+00AD, U+202E, U+E007F) and for U+2028, and
/// false for a letter, a space and the code points just outside a range (U+00AC, U+00AE).
/// It catches the predicate's two halves (control, format) losing either one in the move.
#[test]
fn hidden_is_a_control_or_a_format_character() {
    for c in [
        '\0',
        '\n',
        '\u{7f}',
        '\u{ad}',
        '\u{202e}',
        '\u{2028}',
        '\u{e007f}',
    ] {
        assert!(hidden(c), "{:x}", u32::from(c));
    }
    for c in ['a', ' ', '\u{ac}', '\u{ae}', '\u{e9}'] {
        assert!(!hidden(c), "{:x}", u32::from(c));
    }
}
