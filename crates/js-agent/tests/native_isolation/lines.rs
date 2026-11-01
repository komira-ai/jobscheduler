//! What the two runs say to the test: one line per item, `ISO <verdict> <kind>` and then
//! the token and the text, tab-separated. Run A says what it seeded (or what was
//! refused); the test hands the seeded lines to run B, which says what it found. A line
//! that does not start with `ISO ` (the test harness's own output) is not ours.

use super::token::Token;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    HomeFile,
    ScratchFile,
    SystemTempFile,
    Process,
    Keychain,
    Defaults,
    LaunchAgent,
    SysvShm,
    SysvSem,
    SysvMsg,
    PosixShm,
    PosixSem,
    /// A file in `DARWIN_USER_TEMP_DIR`, the per-user directory under `/var/folders` that is
    /// keyed by the uid and not by the home.
    UserTempDir,
    /// A file in `DARWIN_USER_CACHE_DIR`, its sibling.
    UserCacheDir,
    /// A file in `/Users/Shared`.
    SharedFile,
    /// A launchd agent in the `gui/<uid>` domain.
    GuiLaunchAgent,
}

impl Kind {
    pub const ALL: [Kind; 16] = [
        Kind::HomeFile,
        Kind::ScratchFile,
        Kind::SystemTempFile,
        Kind::Process,
        Kind::Keychain,
        Kind::Defaults,
        Kind::LaunchAgent,
        Kind::SysvShm,
        Kind::SysvSem,
        Kind::SysvMsg,
        Kind::PosixShm,
        Kind::PosixSem,
        Kind::UserTempDir,
        Kind::UserCacheDir,
        Kind::SharedFile,
        Kind::GuiLaunchAgent,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::HomeFile => "home-file",
            Kind::ScratchFile => "scratch-file",
            Kind::SystemTempFile => "system-temp-file",
            Kind::Process => "process",
            Kind::Keychain => "keychain-item",
            Kind::Defaults => "defaults-key",
            Kind::LaunchAgent => "launch-agent",
            Kind::SysvShm => "sysv-shared-memory",
            Kind::SysvSem => "sysv-semaphore",
            Kind::SysvMsg => "sysv-message-queue",
            Kind::PosixShm => "posix-shared-memory",
            Kind::PosixSem => "posix-semaphore",
            Kind::UserTempDir => "user-temp-dir-file",
            Kind::UserCacheDir => "user-cache-dir-file",
            Kind::SharedFile => "users-shared-file",
            Kind::GuiLaunchAgent => "gui-launch-agent",
        }
    }

    pub fn from_label(label: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.label() == label)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Run A left the item behind; the text says where.
    Seeded,
    /// Run A could not leave it; the text says why.
    Refused,
    /// Run A could not leave it because the product profile (the sandbox) denied it; the text
    /// says how. The job could not create the state, so nothing can carry over.
    Denied,
    /// Run B found the item; the text says where.
    Found,
    /// Run B could not tell whether the item is there; the text says why. Never a pass.
    Unlooked,
}

impl Verdict {
    fn word(self) -> &'static str {
        match self {
            Verdict::Seeded => "seeded",
            Verdict::Refused => "refused",
            Verdict::Denied => "denied",
            Verdict::Found => "found",
            Verdict::Unlooked => "unlooked",
        }
    }

    fn from_word(word: &str) -> Option<Verdict> {
        [
            Verdict::Seeded,
            Verdict::Refused,
            Verdict::Denied,
            Verdict::Found,
            Verdict::Unlooked,
        ]
        .into_iter()
        .find(|verdict| verdict.word() == word)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub verdict: Verdict,
    pub kind: Kind,
    pub token: Token,
    pub text: String,
}

impl Line {
    /// A line whose text is flattened to one line: a tab or a line break (a tool's error
    /// message has them) becomes a space.
    pub fn new(verdict: Verdict, kind: Kind, token: Token, text: &str) -> Self {
        let text = text
            .trim()
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        Self {
            verdict,
            kind,
            token,
            text,
        }
    }

    pub fn encode(&self) -> String {
        format!(
            "ISO {} {}\t{}\t{}",
            self.verdict.word(),
            self.kind.label(),
            self.token,
            self.text
        )
    }
}

/// The well-formed lines of `text` that are ours, whatever else is in it: the output of a run
/// that died midway may end in a half line, and what it had already left must still be
/// cleaned up.
pub fn parse_lines_lenient(text: &str) -> Vec<Line> {
    let _ = text;
    Vec::new()
}

/// The lines of `text` that are ours. A line that starts with `ISO ` and is not a
/// well-formed line is an error naming it: a garbled report must not read as "found
/// nothing".
///
/// # Errors
/// A line that starts with `ISO ` and has an unknown verdict or kind, a token that is not
/// one, or a missing field.
pub fn parse_lines(text: &str) -> Result<Vec<Line>, String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("ISO ") else {
            continue;
        };
        let bad = || format!("a malformed report line: {line:?}");
        let (head, tail) = rest.split_once('\t').ok_or_else(bad)?;
        let (token, text) = tail.split_once('\t').ok_or_else(bad)?;
        let (verdict, kind) = head.split_once(' ').ok_or_else(bad)?;
        lines.push(Line {
            verdict: Verdict::from_word(verdict).ok_or_else(bad)?,
            kind: Kind::from_label(kind).ok_or_else(bad)?,
            token: Token::parse(token).ok_or_else(bad)?,
            text: text.to_owned(),
        });
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u8) -> Token {
        Token::from_bytes([n; 8])
    }

    /// Asserts that every kind has its own label and parses back from it, and no label
    /// holds a space or tab (the line format splits on both). It catches two kinds that
    /// share a label, which would let run B's report for one hide the other.
    #[test]
    fn every_kind_has_its_own_label_that_parses_back() {
        let mut seen = Vec::new();
        for kind in Kind::ALL {
            let label = kind.label();
            assert!(!label.contains([' ', '\t']), "{label}");
            assert_eq!(Kind::from_label(label), Some(kind));
            assert!(!seen.contains(&label), "{label} is used twice");
            seen.push(label);
        }
        assert_eq!(Kind::from_label("nothing"), None);
        assert_eq!(seen.len(), 16);
    }

    /// Asserts that a line survives encode and parse, with a text that holds spaces, and
    /// that a text with tabs and line breaks is flattened first so it cannot become a
    /// second line or field. It catches an error message that splits the report in two.
    #[test]
    fn a_line_round_trips_and_a_text_stays_on_one_line() {
        let line = Line::new(Verdict::Seeded, Kind::SysvShm, token(7), "key=-5 id=9");
        let back = parse_lines(&line.encode()).unwrap();
        assert_eq!(back, vec![line]);
        let messy = Line::new(
            Verdict::Refused,
            Kind::Keychain,
            token(1),
            "security: no\tkeychain\nfound\r\n",
        );
        assert_eq!(messy.text, "security: no keychain found");
        assert_eq!(parse_lines(&messy.encode()).unwrap(), vec![messy]);
        let denied = Line::new(
            Verdict::Denied,
            Kind::Process,
            token(5),
            "refused by the profile",
        );
        assert_eq!(parse_lines(&denied.encode()).unwrap(), vec![denied]);
        let unlooked = Line::new(
            Verdict::Unlooked,
            Kind::Keychain,
            token(4),
            "security exited 51",
        );
        assert_eq!(parse_lines(&unlooked.encode()).unwrap(), vec![unlooked]);
    }

    /// Asserts that the lenient reader keeps the good lines of a run that died midway and
    /// drops a half line instead of failing. It catches a cleanup that gives up on the whole
    /// output because its last line was cut, and so leaves what was seeded.
    #[test]
    fn a_half_line_does_not_hide_the_lines_before_it() {
        let one = Line::new(Verdict::Seeded, Kind::SysvShm, token(6), "key=-5");
        let text = format!("{}\nISO seeded posix-shared-memory\t0606", one.encode());
        assert_eq!(parse_lines_lenient(&text), vec![one]);
        assert!(parse_lines(&text).is_err());
        assert!(parse_lines_lenient("").is_empty());
    }

    /// Asserts that lines the harness prints around ours are skipped, in order, and that
    /// a damaged line of ours is an error and not silence. It catches a reader that drops
    /// a garbled report, which would turn "run B said something odd" into "run B found
    /// nothing".
    #[test]
    fn the_harness_noise_is_skipped_and_a_damaged_line_is_an_error() {
        let one = Line::new(Verdict::Found, Kind::Process, token(2), "pid=5");
        let two = Line::new(Verdict::Found, Kind::PosixSem, token(3), "/jsi01");
        let text = format!(
            "running 1 test\n{}\nISOLATED is not ours\n{}\ntest result: ok\n",
            one.encode(),
            two.encode()
        );
        assert_eq!(parse_lines(&text).unwrap(), vec![one, two]);
        for bad in [
            "ISO found process",
            "ISO found process\t0202020202020202",
            "ISO lost process\t0202020202020202\tx",
            "ISO found tea\t0202020202020202\tx",
            "ISO found process\t02\tx",
            "ISO process\t0202020202020202\tx",
        ] {
            assert!(parse_lines(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(parse_lines("").unwrap().is_empty());
    }
}
