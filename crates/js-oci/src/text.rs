//! Text from a registry or a caller, made safe to put in an error: cut to a bounded
//! length, with control characters escaped, and URLs without anything that is a secret.

/// The most bytes of server-supplied text an error carries.
const CLIP: usize = 128;

/// `text` cut to at most [`CLIP`] bytes, at a character boundary, with an ellipsis when
/// something was cut. Nothing is escaped: for a value an error prints with `{:?}`.
pub(crate) fn clip(text: &str) -> String {
    if text.len() <= CLIP {
        return text.to_owned();
    }
    let end = (0..=CLIP)
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or_default();
    format!("{}\u{2026}", &text[..end])
}

/// [`clip`]ped `text` with control characters, quotes and backslashes escaped, so it
/// stays on one line and cannot move a terminal's cursor or colour its output.
pub(crate) fn escape(text: &str) -> String {
    clip(text).escape_debug().to_string()
}

/// [`escape`]d `text` in double quotes.
pub(crate) fn shown(text: &str) -> String {
    format!("\"{}\"", escape(text))
}

/// `url` as an error may show it: scheme, host, port and path. The userinfo, the query
/// and the fragment are dropped, since a registry's redirect to a CDN carries a
/// signature in its query, and the result is [`escape`]d like any server text.
pub(crate) fn url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) if scheme.bytes().all(scheme_byte) => (format!("{scheme}://"), rest),
        _ => (String::new(), url),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit('@').next().unwrap_or_default();
    let path = &tail[..tail.find(['?', '#']).unwrap_or(tail.len())];
    escape(&format!("{scheme}{host}{path}"))
}

/// Whether `byte` may be in a URI scheme after its first letter.
pub(crate) fn scheme_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a cut in the middle of a character, a cut where none was needed, and a
    /// missing mark that something was left out.
    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(clip("short"), "short");
        assert_eq!(clip(&"a".repeat(128)), "a".repeat(128));
        assert_eq!(
            clip(&"a".repeat(129)),
            format!("{}\u{2026}", "a".repeat(128))
        );
        // 127 bytes, then a two-byte character straddling the limit.
        let straddle = format!("{}\u{e9}tail", "a".repeat(127));
        assert_eq!(clip(&straddle), format!("{}\u{2026}", "a".repeat(127)));
        assert!(clip(&"z".repeat(64 << 10)).len() < 140);
    }

    /// Catches control characters surviving into an error line: escape, CR, LF, tab,
    /// quotes and backslashes are all spelled out, and the result is one line.
    #[test]
    fn control_characters_and_quotes_are_escaped() {
        let hostile = "a\u{1b}[31m\r\nb\t\"c\" \\ d";
        let out = escape(hostile);
        assert!(!out.contains(['\u{1b}', '\r', '\n', '\t']), "{out:?}");
        assert!(out.contains("\\u{1b}"), "{out}");
        assert!(out.contains("\\r\\n"), "{out}");
        assert_eq!(shown("x\ny"), "\"x\\ny\"");
        assert!(shown(&"\u{1b}".repeat(10_000)).len() < 1024);
    }

    /// Catches a secret surviving in a URL: userinfo, query and fragment are dropped in
    /// every spelling, the rest is kept, and what is no URL is shown without its query too.
    #[test]
    fn urls_lose_their_userinfo_query_and_fragment() {
        for (given, want) in [
            ("https://cdn.test/b?sig=SECRET", "https://cdn.test/b"),
            ("https://cdn.test/b#SECRET", "https://cdn.test/b"),
            ("https://cdn.test/b?x=1#SECRET", "https://cdn.test/b"),
            (
                "https://u:SECRET@cdn.test:8443/b/c",
                "https://cdn.test:8443/b/c",
            ),
            ("https://u:SECRET@cdn.test?q=1", "https://cdn.test"),
            ("https://u@v@cdn.test/x", "https://cdn.test/x"),
            ("https://cdn.test#SECRET", "https://cdn.test"),
            ("https://[::1]:5000/x?SECRET", "https://[::1]:5000/x"),
            ("//u:SECRET@h/x", "//h/x"),
            ("//u:SECRET@h:8443?q=SECRET", "//h:8443"),
            ("/relative?SECRET", "/relative"),
            ("/r?u=https://x", "/r"),
            ("not a url", "not a url"),
            ("sch eme://h/p?SECRET", "sch eme://h/p"),
            ("", ""),
        ] {
            assert_eq!(url(given), want, "{given}");
        }
        assert!(url(&format!("https://h/{}", "p".repeat(64 << 10))).len() < 140);
        assert!(!url("https://h/a\u{1b}[0m\r\n").contains(['\u{1b}', '\r', '\n']));
    }
}
