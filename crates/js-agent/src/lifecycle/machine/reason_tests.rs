//! The detail of a start a provider refused: the code, then the provider's reason on one
//! line, without host paths, within its bound.

use super::{CUT, MAX_REASON_LEN, detail};

/// The detail of a `provider-refused` start whose provider said `reason`.
fn refused(reason: &str) -> String {
    detail("provider-refused", reason)
}

/// Asserts that a reason follows the code after `: ` as the provider wrote it, for the texts a
/// client reads today: the refused tree list, a list the store does not have, a lost manifest
/// and a lost source. Catches a reason dropped, or changed where nothing in it needed to be.
#[test]
fn a_reason_follows_the_code_as_written() {
    let manifest = format!("cannot fetch manifest {}: upstream 404", "0a".repeat(32));
    let reasons = [
        "tree list not supported: missing_sidecar",
        "no tree list at cas/l/0123abcd",
        manifest.as_str(),
        "404 source missing: cas/s/ab12/cd34.bin",
        "the ref and/or its 1/2 halves are wrong, see ./notes and ../more",
        "caf\u{e9}/x is a name, not a path",
    ];
    for reason in reasons {
        assert_eq!(refused(reason), format!("provider-refused: {reason}"));
    }
}

/// Asserts that a reason with nothing in it but white space and hidden characters, or with
/// nothing at all, gives the code alone. Catches a detail of `provider-refused: ` with no
/// reason, which no client matches as the plain code.
#[test]
fn an_empty_reason_gives_the_code_alone() {
    for reason in ["", " ", "\n", " \t\r\n\u{200b}\u{7} "] {
        assert_eq!(refused(reason), "provider-refused", "{reason:?}");
    }
}

/// Asserts that line breaks, tabs, control and format characters, and runs of them, each
/// become one space, with none at either end. Catches a detail with a line break, which the
/// wire refuses with the whole heartbeat, and a reason that keeps a character that hides text.
#[test]
fn a_reason_is_put_on_one_line() {
    assert_eq!(
        refused("\n first line\r\n  second\tline\u{7}end\u{200b}x\u{2028}\n"),
        "provider-refused: first line second line end x"
    );
}

/// Asserts that each path on the host in a reason becomes `<path>`: one at the start, after a
/// space, after `=` or `,`, inside quotes, brackets and back quotes, before a `:` or a `.` that
/// ends a sentence, and the host and path of a URL; and that a path of a home directory keeps
/// only its `~`. Catches a host path given to a client, and a path cut so that a part of it is
/// left in the text.
#[test]
fn a_path_on_the_host_becomes_a_mark() {
    let cases = [
        (
            "cannot open /var/lib/tier/cell-3/list.bin: No such file or directory (os error 2)",
            "cannot open <path>: No such file or directory (os error 2)",
        ),
        ("/srv/trees/k1 is gone.", "<path> is gone."),
        ("the tree is at /srv/trees/k1.", "the tree is at <path>."),
        ("root=/srv/x;mode=ro", "root=<path>;mode=ro"),
        ("roots /a/b, /c/d", "roots <path>, <path>"),
        (
            "in '/run/tier/sock' (\"/run/x\") [/a] {/b} </c> `/d`",
            "in '<path>' (\"<path>\") [<path>] {<path>} <<path>> `<path>`",
        ),
        ("a,/b", "a,<path>"),
        ("see http://192.0.2.1/v1/trees/k1.", "see http:<path>."),
        ("under ~/trees", "under ~<path>"),
        ("/", "<path>"),
    ];
    for (reason, stripped) in cases {
        assert_eq!(
            refused(reason),
            format!("provider-refused: {stripped}"),
            "{reason:?}"
        );
    }
}

/// Asserts that a reason of [`MAX_REASON_LEN`] bytes is kept whole, and that a longer one is
/// cut at the last space before the bound, so no word is given in part, and ends in the mark,
/// in at most [`MAX_REASON_LEN`] bytes. Catches a detail that grows with the provider's text,
/// and a key or a digest given in part, which a client would read as another one.
#[test]
fn a_long_reason_is_cut_at_a_space() {
    let whole = "a".repeat(MAX_REASON_LEN);
    assert_eq!(refused(&whole), format!("provider-refused: {whole}"));
    let words = ["ab"; 100].join(" ");
    let kept = ["ab"; 84].join(" ");
    assert_eq!(refused(&words), format!("provider-refused: {kept}{CUT}"));
    let at_space = format!("x {} {}", "a".repeat(251), "b".repeat(10));
    let kept = format!("x {}{CUT}", "a".repeat(251));
    assert_eq!(kept.len(), MAX_REASON_LEN);
    assert_eq!(refused(&at_space), format!("provider-refused: {kept}"));
}

/// Asserts that a long reason with no space is cut at the last character boundary before the
/// bound: within a word of one-byte characters right at it, within one of two-byte
/// characters a byte below it. Catches a cut inside a character, which panics.
#[test]
fn a_long_word_is_cut_at_a_character() {
    let bytes = MAX_REASON_LEN - CUT.len_utf8();
    let cases = [
        ("a".repeat(MAX_REASON_LEN + 1), "a".repeat(bytes)),
        ("\u{e9}".repeat(200), "\u{e9}".repeat(bytes / 2)),
    ];
    for (reason, kept) in cases {
        let cut = refused(&reason);
        assert_eq!(cut, format!("provider-refused: {kept}{CUT}"));
        assert!(cut.len() <= "provider-refused: ".len() + MAX_REASON_LEN);
    }
}

/// Asserts that the bound is applied after the paths are replaced: a reason over the bound
/// only by a long path is kept whole. Catches a cut that drops what the provider said after a
/// path that is no longer there.
#[test]
fn the_bound_counts_the_reason_without_its_paths() {
    let reason = format!("cannot read /{}: denied", "d".repeat(MAX_REASON_LEN));
    assert_eq!(
        refused(&reason),
        "provider-refused: cannot read <path>: denied"
    );
}
