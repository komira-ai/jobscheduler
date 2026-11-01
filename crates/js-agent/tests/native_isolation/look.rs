//! What a look at the machine can answer, and the classifiers that decide it.
//!
//! A look has three outcomes, never two: the item is there (`Found`), the tool or system
//! call said, by its documented not-found answer, that it is not (`Absent`), or the look
//! failed for any other reason (`CouldNotLook`). Only `Absent` may read as clean; a
//! `CouldNotLook` fails the test with its reason in the table. A look that cannot tell
//! "not there" from "not allowed to see" must not pass.
//!
//! The not-found answers relied on, per tool:
//!
//! - a path (`stat`): errno `ENOENT`. `EACCES`, `ENOTDIR`, `EPERM` and the rest are not
//!   absence;
//! - `shmget`, `semget`, `msgget`, `shm_open`, `sem_open`: errno `ENOENT`;
//! - `security find-generic-password`: exit status 44 with "could not be found" on
//!   standard error (`errSecItemNotFound`), or exit status 37 with "default keychain could not be
//!   found" (`errSecNoDefaultKeychain`; the fresh user has no keychain at all, so it has no item). The tool exits with the negated error code modulo 256: -25300 gives 44, -25307 gives 37;
//! - `defaults read`: exit status 1 with "does not exist" on standard error (the domain or
//!   the domain/key pair is missing). Exit status 1 alone is not absence;
//! - `launchctl print <domain>`: exit status 113 with "Could not find domain" on standard
//!   error;
//! - `ps`, `ipcs`, `find`: no not-found answer; any status but 0 is a failed look.
//!
//! The same care applies to run A's seeds. A kind counts as COVERED when it was seeded, or
//! when the product profile refused the seed with a SANDBOX denial (the job could not create
//! the state, so nothing can carry over): `EPERM` or `EACCES` from the call, or the sandbox's
//! own words from a tool ("Operation not permitted", "deny(1)", sandbox-violation text).
//! That row is recorded as refused by the profile. Any other seed failure (`ENOSPC`, a
//! missing tool, a crash) proves nothing. The minimum set (a file, a process, a keychain item,
//! a launchd agent, shared memory) is met when each kind is covered.

use std::io;
use std::path::Path;

use super::lines::{Kind, Line, Verdict};
use super::token::Token;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Found(String),
    Absent,
    CouldNotLook(String),
}

/// A `stat` of `path` (links not followed): there, or `ENOENT`, or a failed look.
pub fn classify_path(path: &Path, stat: Result<(), io::Error>) -> Outcome {
    match stat {
        Ok(()) => Outcome::Found(path.display().to_string()),
        Err(why) if why.raw_os_error() == Some(libc::ENOENT) => Outcome::Absent,
        Err(why) => Outcome::CouldNotLook(format!("{}: {why}", path.display())),
    }
}

/// A System V or POSIX call that returned an error with `errno`.
pub fn classify_errno(what: &str, errno: Option<i32>) -> Outcome {
    match errno {
        Some(code) if code == libc::ENOENT => Outcome::Absent,
        Some(code) => {
            Outcome::CouldNotLook(format!("{what}: {}", io::Error::from_raw_os_error(code)))
        }
        None => Outcome::CouldNotLook(format!("{what}: failed with no errno")),
    }
}

/// `security find-generic-password -s <token>` (exit status `code`, `None` when a signal
/// ended it).
pub fn classify_security(code: Option<i32>, stderr: &str) -> Outcome {
    if code == Some(0) {
        return Outcome::Found(
            "a generic password with the token is in the keychain search list".to_owned(),
        );
    }
    if code == Some(37) && stderr.contains("default keychain could not be found") {
        return Outcome::Absent;
    }
    if code == Some(44) && stderr.contains("could not be found") {
        return Outcome::Absent;
    }
    Outcome::CouldNotLook(format!("security exited {code:?}: {}", stderr.trim()))
}

/// `defaults read <domain> <key>`.
pub fn classify_defaults(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
    match code {
        Some(0) => Outcome::Found(format!("the key reads back as {:?}", stdout.trim())),
        Some(1) if stderr.contains("does not exist") => Outcome::Absent,
        _ => Outcome::CouldNotLook(format!("defaults exited {code:?}: {}", stderr.trim())),
    }
}

/// `launchctl print <domain>`: the text of the domain, or its documented absence.
pub fn classify_launchctl_print(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
    match code {
        Some(0) => Outcome::Found(stdout.to_owned()),
        Some(113) if stderr.contains("Could not find domain") => Outcome::Absent,
        _ => Outcome::CouldNotLook(format!("launchctl exited {code:?}: {}", stderr.trim())),
    }
}

/// The standard output of a tool that has no not-found answer (`ps`, `ipcs`, `find`), or
/// why the look failed.
///
/// # Errors
/// Any exit status but 0.
pub fn view_output(
    program: &str,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<String, String> {
    match code {
        Some(0) => Ok(stdout.to_owned()),
        _ => Err(format!("{program} exited {code:?}: {}", stderr.trim())),
    }
}

/// The paths a `find -uid` printed: those with a token in them, which are left behind by the
/// run, and the others, which are only reported (the uid's own caches get files of the
/// machine's making). EVERY path is examined for a token; only the notes are cut at `limit`,
/// and the third value says whether they were.
pub fn split_found_files(
    output: &str,
    tokens: &[Token],
    limit: usize,
) -> (Vec<String>, Vec<String>, bool) {
    let mut with = Vec::new();
    let mut notes = Vec::new();
    let mut cut = false;
    for path in output.lines().filter(|line| !line.is_empty()) {
        if tokens.iter().any(|token| path.contains(token.as_str())) {
            with.push(path.to_owned());
        } else if notes.len() < limit {
            notes.push(path.to_owned());
        } else {
            cut = true;
        }
    }
    (with, notes, cut)
}

/// Records `handle` in `runs`, then finishes the run. The handle is in `runs` whatever
/// `finish` does, so a run that fails midway is still there for the cleanup to end.
pub fn record_then<H, R>(
    runs: &mut Vec<H>,
    handle: H,
    finish: impl FnOnce(&H) -> Result<R, String>,
) -> Result<R, String> {
    runs.push(handle);
    let handle = runs.last().expect("just pushed");
    finish(handle)
}

/// A value read back after a seed must be the one written.
///
/// # Errors
/// The two differ (compared trimmed).
pub fn readback_matches(expected: &str, actual: &str) -> Result<(), String> {
    match actual.trim() == expected.trim() {
        true => Ok(()),
        false => Err(format!(
            "wrote {expected:?} and read back {:?}",
            actual.trim()
        )),
    }
}

/// Root's `launchctl print` of `domain` proves an agent with `token` is loaded.
///
/// # Errors
/// The domain is absent, could not be read, or does not hold the token.
pub fn domain_holds(domain: &str, seen: &Outcome, token: &Token) -> Result<(), String> {
    match seen {
        Outcome::Found(text) if text.contains(token.as_str()) => Ok(()),
        Outcome::Found(_) => Err(format!("launchctl print {domain} does not hold the agent")),
        Outcome::Absent => Err(format!("launchctl print {domain}: there is no such domain")),
        Outcome::CouldNotLook(why) => Err(format!("launchctl print {domain}: {why}")),
    }
}

/// Whether `bytes` hold any of `tokens`.
pub fn content_with_token(bytes: &[u8], tokens: &[Token]) -> bool {
    tokens.iter().any(|token| {
        let needle = token.as_str().as_bytes();
        bytes.windows(needle.len()).any(|window| window == needle)
    })
}

/// Why run A could not seed an item: the sandbox of the product profile said no (so the job
/// could not create the state and nothing can carry over), or anything else (a full disk, a
/// missing tool, a crash), which proves nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedFailure {
    ProfileRefused,
    Other,
}

/// Tells the two apart from the failure text of a seed: `EPERM` or `EACCES` from the call
/// (as `io::Error` prints them), or the sandbox's own words from a tool.
pub fn classify_seed_failure(text: &str) -> SeedFailure {
    let text = text.to_lowercase();
    let sandbox = [
        "operation not permitted",
        "permission denied",
        "(os error 1)",
        "(os error 13)",
        "deny(1)",
        "sandbox:",
        "sandbox violation",
    ];
    match sandbox.iter().any(|words| text.contains(words)) {
        true => SeedFailure::ProfileRefused,
        false => SeedFailure::Other,
    }
}

/// The kinds a run A must have covered for the result to mean anything: a file, a process,
/// a keychain item, a launchd agent and some shared memory. A kind is covered when it was
/// seeded or denied by the profile. What is missing, by name; a run where these were refused
/// for another reason proves nothing.
pub fn missing_minimum(lines: &[Line]) -> Vec<&'static str> {
    let seeded = |kind: Kind| {
        lines.iter().any(|line| {
            line.kind == kind && matches!(line.verdict, Verdict::Seeded | Verdict::Denied)
        })
    };
    let mut missing = Vec::new();
    for (kind, name) in [
        (Kind::HomeFile, "home-file"),
        (Kind::Process, "process"),
        (Kind::Keychain, "keychain-item"),
        (Kind::LaunchAgent, "launch-agent"),
    ] {
        if !seeded(kind) {
            missing.push(name);
        }
    }
    if !seeded(Kind::SysvShm) && !seeded(Kind::PosixShm) {
        missing.push("shared memory (System V or POSIX)");
    }
    missing
}

/// The line a run prints for its own `getuid()`.
pub fn uid_line(uid: u32) -> String {
    format!("ISOUID {uid}")
}

/// The uid a run printed with [`uid_line`], if it printed one.
pub fn parse_uid(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix("ISOUID ")?.trim().parse().ok())
}

/// Run A, run B and the configuration agree on the uid.
///
/// # Errors
/// A run printed no uid, or the three differ.
pub fn uids_agree(a: Option<u32>, b: Option<u32>, configured: u32) -> Result<(), String> {
    match (a, b) {
        (Some(a), Some(b)) if a == configured && b == configured => Ok(()),
        (Some(a), Some(b)) => Err(format!(
            "run A ran as uid {a}, run B as uid {b}, and {configured} was asked for: the uid was not reused, so this run proves nothing"
        )),
        _ => Err("a run did not print the uid it ran as".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u8) -> Token {
        Token::from_bytes([n; 8])
    }

    /// Asserts that a path is absent only for `ENOENT`: found for a stat that worked, and a
    /// failed look for permission, not-a-directory and other errors. It catches
    /// `Path::exists`, which turns every error into "not there".
    #[test]
    fn a_path_is_absent_only_on_enoent() {
        let path = Path::new("/x/y");
        assert_eq!(
            classify_path(path, Ok(())),
            Outcome::Found("/x/y".to_owned())
        );
        let err = |code| Err(io::Error::from_raw_os_error(code));
        assert_eq!(classify_path(path, err(libc::ENOENT)), Outcome::Absent);
        for code in [libc::EACCES, libc::EPERM, libc::ENOTDIR, libc::EIO] {
            assert!(
                matches!(classify_path(path, err(code)), Outcome::CouldNotLook(_)),
                "errno {code} was not a failed look"
            );
        }
    }

    /// Asserts the same for the System V and POSIX calls: `ENOENT` is absent, any other
    /// errno (`EACCES` is the one a permission-denied lookup gives) or none is a failed
    /// look that names the call.
    #[test]
    fn a_shared_object_is_absent_only_on_enoent() {
        assert_eq!(
            classify_errno("shmget", Some(libc::ENOENT)),
            Outcome::Absent
        );
        for code in [
            Some(libc::EACCES),
            Some(libc::EINVAL),
            Some(libc::ENOSYS),
            None,
        ] {
            match classify_errno("semget", code) {
                Outcome::CouldNotLook(why) => assert!(why.starts_with("semget"), "{why}"),
                other => panic!("{code:?} gave {other:?}"),
            }
        }
    }

    /// Asserts `security`'s answers: exit 0 is found; exit 44 with "could not be found" and
    /// the missing default keychain are absent; exit 44 without the words, a denied keychain
    /// and a signal are failed looks. It catches `.ok()`, which read all of these as absent.
    #[test]
    fn security_is_absent_only_on_its_not_found_answers() {
        assert!(matches!(classify_security(Some(0), ""), Outcome::Found(_)));
        let not_found = "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.";
        assert_eq!(classify_security(Some(44), not_found), Outcome::Absent);
        assert_eq!(
            classify_security(Some(37), "security: A default keychain could not be found."),
            Outcome::Absent
        );
        for (code, text) in [
            (Some(44), "unexpected"),
            (Some(1), not_found),
            (Some(36), "User interaction is not allowed."),
            (Some(51), "security: A default keychain could not be found."),
            (None, "security: A default keychain could not be found."),
            (None, ""),
        ] {
            assert!(
                matches!(classify_security(code, text), Outcome::CouldNotLook(_)),
                "{code:?} {text:?}"
            );
        }
    }

    /// Asserts `defaults read`'s answers: 0 is found whatever it reads; exit 1 with "does
    /// not exist" (either wording) is absent; exit 1 with another message, other codes and a
    /// signal are failed looks.
    #[test]
    fn defaults_is_absent_only_on_does_not_exist() {
        assert!(matches!(
            classify_defaults(Some(0), "tok\n", ""),
            Outcome::Found(_)
        ));
        for text in [
            "Domain com.x.y does not exist",
            "The domain/default pair of (com.x.y, isolation) does not exist",
        ] {
            assert_eq!(
                classify_defaults(Some(1), "", text),
                Outcome::Absent,
                "{text}"
            );
        }
        for (code, text) in [
            (Some(1), "cfprefsd: connection invalid"),
            (Some(2), "does not exist"),
            (None, "does not exist"),
        ] {
            assert!(
                matches!(classify_defaults(code, "", text), Outcome::CouldNotLook(_)),
                "{code:?} {text:?}"
            );
        }
    }

    /// Asserts `launchctl print`'s answers: 0 returns the text; exit 113 with "Could not find
    /// domain" is absent; the same code without the words, other codes and a signal are
    /// failed looks. It catches a view that drops the exit status and so reads a permission
    /// failure as an empty domain.
    #[test]
    fn launchctl_print_is_absent_only_on_its_not_found_answer() {
        assert_eq!(
            classify_launchctl_print(Some(0), "user/700 = {\n}\n", ""),
            Outcome::Found("user/700 = {\n}\n".to_owned())
        );
        assert_eq!(
            classify_launchctl_print(Some(113), "", "Could not find domain for user/700"),
            Outcome::Absent
        );
        for (code, text) in [
            (Some(113), "Operation not permitted"),
            (Some(1), "Could not find domain for user/700"),
            (Some(150), "Domain does not support specified action"),
            (None, ""),
        ] {
            assert!(
                matches!(
                    classify_launchctl_print(code, "", text),
                    Outcome::CouldNotLook(_)
                ),
                "{code:?} {text:?}"
            );
        }
    }

    /// Asserts that `ps`, `ipcs` and `find` have no not-found answer: their output counts
    /// only on exit 0, and a failure carries the program and its message.
    #[test]
    fn a_view_counts_only_on_exit_zero() {
        assert_eq!(
            view_output("ps", Some(0), "rows", ""),
            Ok("rows".to_owned())
        );
        for code in [Some(1), Some(127), None] {
            let why = view_output("ipcs", code, "rows", "ipcs: not allowed\n").unwrap_err();
            assert!(why.starts_with("ipcs exited"), "{why}");
            assert!(why.contains("ipcs: not allowed"), "{why}");
        }
    }

    /// Asserts that files found by `find -uid` are split by token in the path, kept in
    /// order; every path is examined and only the printed notes are cut at the limit, with the
    /// cut reported. It catches a split that calls the
    /// machine's own cache files a leak (or a leak a cache file).
    #[test]
    fn found_files_are_split_by_token_and_cut_at_the_limit() {
        let a = token(0xa1);
        let text = format!(
            "/private/tmp/iso-{a}\n/private/var/folders/x/y/T/com.apple.something\n\n/Users/Shared/{a}.txt\n/private/tmp/other\n"
        );
        let (with, without, cut) = split_found_files(&text, std::slice::from_ref(&a), 10);
        assert_eq!(
            with,
            [
                format!("/private/tmp/iso-{a}"),
                format!("/Users/Shared/{a}.txt")
            ]
        );
        assert_eq!(
            without,
            [
                "/private/var/folders/x/y/T/com.apple.something",
                "/private/tmp/other"
            ]
        );
        assert!(!cut);
        let (with, without, cut) = split_found_files(&text, std::slice::from_ref(&a), 2);
        assert_eq!((with.len(), without.len(), cut), (2, 2, false));
        let (with, without, cut) = split_found_files(&text, std::slice::from_ref(&a), 1);
        assert_eq!((with.len(), without.len(), cut), (2, 1, true));
        assert_eq!(split_found_files("", &[a], 3), (vec![], vec![], false));
    }

    /// Asserts that a token in the 600th path is found with a limit of 500, and that the
    /// notes are cut at 500. It catches a scan that examines only the first `limit` paths,
    /// which passes a leak that happens to sort late.
    #[test]
    fn a_token_in_a_late_path_is_found_whatever_the_limit() {
        let a = token(0xb2);
        let mut text = String::new();
        for n in 0..599 {
            text.push_str(&format!("/private/var/folders/x/y/T/file{n}\n"));
        }
        text.push_str(&format!("/private/var/folders/x/y/C/iso-{a}\n"));
        let (with, without, cut) = split_found_files(&text, std::slice::from_ref(&a), 500);
        assert_eq!(with, [format!("/private/var/folders/x/y/C/iso-{a}")]);
        assert_eq!(without.len(), 500);
        assert!(cut);
    }

    /// Asserts that a run's handle is recorded before the run is finished, so that a run that
    /// fails midway (a settle error, a non-zero child) is still in the list the cleanup
    /// walks. It catches a start whose handle is recorded only on success, which leaks the
    /// pool user of a failed run.
    #[test]
    fn a_run_is_recorded_before_it_can_fail() {
        let mut runs: Vec<&str> = Vec::new();
        let failed: Result<u8, String> =
            record_then(&mut runs, "a.1", |_| Err("the run did not end".to_owned()));
        assert_eq!(failed, Err("the run did not end".to_owned()));
        assert_eq!(runs, ["a.1"]);
        let ok = record_then(&mut runs, "b.1", |handle| Ok(handle.len()));
        assert_eq!(ok, Ok(3));
        assert_eq!(runs, ["a.1", "b.1"]);
        let mut seen = Vec::new();
        let mut runs: Vec<u8> = Vec::new();
        let _: Result<(), String> = record_then(&mut runs, 7, |_| {
            seen.push(true);
            Ok(())
        });
        assert_eq!((runs, seen), (vec![7], vec![true]));
    }

    /// Asserts that a value read back must equal what was written (trimmed), and that a mismatch
    /// names both. It catches a seed that is called seeded because the write returned 0.
    #[test]
    fn a_seed_must_read_back_what_was_written() {
        assert_eq!(readback_matches("0123", "0123\n"), Ok(()));
        let why = readback_matches("0123", "4567").unwrap_err();
        assert!(why.contains("0123") && why.contains("4567"), "{why}");
        assert!(readback_matches("0123", "").is_err());
    }

    /// Asserts that root's `launchctl print` of a domain proves an agent seed only when it is
    /// found and holds the token: not when it is absent, when it lacks the token, or when the
    /// look failed. It catches a control that assumes the agent is loaded.
    #[test]
    fn a_launch_agent_is_proven_by_the_print_of_its_domain() {
        let a = token(0xc3);
        let with = Outcome::Found(format!("services = {{\n  0  -  com.komira.iso.{a}\n}}"));
        assert_eq!(domain_holds("user/700", &with, &a), Ok(()));
        for seen in [
            Outcome::Found("services = {\n}".to_owned()),
            Outcome::Absent,
            Outcome::CouldNotLook("launchctl exited Some(1)".to_owned()),
        ] {
            let why = domain_holds("user/700", &seen, &a).unwrap_err();
            assert!(why.contains("user/700"), "{why}");
        }
    }

    /// Asserts that the content of a file is searched for the tokens as bytes, so that a file
    /// of the uid whose name lacks the token (a cfprefs plist, a keychain database) is still
    /// caught, and that a token cut in two is not.
    #[test]
    fn a_token_in_the_content_of_a_file_is_found() {
        let a = token(0xd4);
        let b = token(0xe5);
        let mut bytes = vec![0u8, 1, 2, 255];
        bytes.extend_from_slice(a.as_str().as_bytes());
        bytes.extend_from_slice(b"\0tail");
        assert!(content_with_token(&bytes, std::slice::from_ref(&a)));
        assert!(!content_with_token(&bytes, std::slice::from_ref(&b)));
        assert!(content_with_token(&bytes, &[b.clone(), a.clone()]));
        assert!(!content_with_token(
            &a.as_str().as_bytes()[..15],
            std::slice::from_ref(&a)
        ));
        assert!(!content_with_token(b"", &[b]));
    }

    fn seeded(kind: Kind) -> Line {
        Line::new(Verdict::Seeded, kind, token(1), "x")
    }

    /// Asserts that the minimum set is a file, a process, a keychain item, a launchd agent
    /// and some shared memory (either kind), counted only when seeded and not refused. It
    /// catches a run where the profile refused the hard items and a clean B was then
    /// called a pass.
    #[test]
    fn a_run_must_have_seeded_the_minimum_kinds() {
        let all = [
            seeded(Kind::HomeFile),
            seeded(Kind::Process),
            seeded(Kind::Keychain),
            seeded(Kind::LaunchAgent),
            seeded(Kind::PosixShm),
        ];
        assert!(missing_minimum(&all).is_empty());
        let mut with_sysv = all.to_vec();
        with_sysv[4] = seeded(Kind::SysvShm);
        assert!(missing_minimum(&with_sysv).is_empty());
        assert_eq!(missing_minimum(&[]).len(), 5);
        let mut refused = all.to_vec();
        refused[2] = Line::new(Verdict::Refused, Kind::Keychain, token(2), "denied");
        assert_eq!(missing_minimum(&refused), ["keychain-item"]);
        refused.remove(4);
        assert_eq!(
            missing_minimum(&refused),
            ["keychain-item", "shared memory (System V or POSIX)"]
        );
        let semaphores = [seeded(Kind::SysvSem), seeded(Kind::PosixSem)];
        assert_eq!(missing_minimum(&semaphores).len(), 5);
    }

    /// Asserts that a seed failure is the profile's only when it says so: `EPERM` and `EACCES`
    /// as `io::Error` prints them, and the sandbox's words from a tool ("Operation not
    /// permitted", "deny(1)", sandbox-violation text). `ENOSPC`, a missing tool, a crash and
    /// the empty text are not. It catches a classifier that excuses any failure, which would
    /// turn a broken seed into a pass.
    #[test]
    fn only_a_sandbox_denial_excuses_a_seed() {
        for code in [libc::EPERM, libc::EACCES] {
            let text = format!("/x/y: {}", io::Error::from_raw_os_error(code));
            assert_eq!(
                classify_seed_failure(&text),
                SeedFailure::ProfileRefused,
                "{text}"
            );
        }
        for text in [
            "/usr/bin/security exited Some(1):  security: Operation not permitted",
            "launchctl exited Some(1): Bootstrap failed: deny(1) mach-lookup com.apple.xpc.launchd",
            "Sandbox: sleep(123) deny(1) process-exec /bin/sleep",
            "defaults exited Some(1): sandbox violation: cfprefsd",
        ] {
            assert_eq!(
                classify_seed_failure(text),
                SeedFailure::ProfileRefused,
                "{text}"
            );
        }
        for code in [
            libc::ENOSPC,
            libc::ENOENT,
            libc::EIO,
            libc::EEXIST,
            libc::EINVAL,
        ] {
            let text = format!("/x/y: {}", io::Error::from_raw_os_error(code));
            assert_eq!(classify_seed_failure(&text), SeedFailure::Other, "{text}");
        }
        for text in [
            "/usr/bin/getconf: No such file or directory (os error 2)",
            "security exited None: ",
            "defaults exited Some(1): cfprefsd crashed",
            "shmget: Invalid argument (os error 22)",
            "",
        ] {
            assert_eq!(classify_seed_failure(text), SeedFailure::Other, "{text}");
        }
    }

    /// Asserts that the minimum set is met by each kind being seeded or denied by the
    /// profile, and not by one that was refused for another reason. It catches a minimum that
    /// counts a broken seed as covered.
    #[test]
    fn a_denial_by_the_profile_covers_a_kind_and_another_refusal_does_not() {
        let denied = |kind| Line::new(Verdict::Denied, kind, token(3), "Operation not permitted");
        let covered = [
            seeded(Kind::HomeFile),
            denied(Kind::Process),
            denied(Kind::Keychain),
            denied(Kind::LaunchAgent),
            denied(Kind::SysvShm),
        ];
        assert!(missing_minimum(&covered).is_empty());
        let mut broken = covered.to_vec();
        broken[2] = Line::new(Verdict::Refused, Kind::Keychain, token(4), "ENOSPC");
        assert_eq!(missing_minimum(&broken), ["keychain-item"]);
        let all_denied: Vec<Line> = [
            Kind::HomeFile,
            Kind::Process,
            Kind::Keychain,
            Kind::LaunchAgent,
        ]
        .into_iter()
        .map(denied)
        .collect();
        assert_eq!(
            missing_minimum(&all_denied),
            ["shared memory (System V or POSIX)"]
        );
    }

    /// Asserts that the uid line round-trips among other output, that the first one is
    /// read, and that A, B and the configured uid must be the same number: a differing
    /// uid or a run that printed none is an error saying the run proves nothing.
    #[test]
    fn the_uid_a_run_prints_is_read_and_must_match() {
        assert_eq!(uid_line(700), "ISOUID 700");
        assert_eq!(
            parse_uid("running 1 test\nISOUID 700\nISO seeded x"),
            Some(700)
        );
        assert_eq!(parse_uid("ISOUID seven\nISOUID 5"), Some(5));
        assert_eq!(parse_uid("ISOUID700\nno"), None);
        assert_eq!(parse_uid(""), None);
        assert_eq!(uids_agree(Some(700), Some(700), 700), Ok(()));
        for (a, b) in [
            (Some(700), Some(701)),
            (Some(701), Some(700)),
            (Some(701), Some(701)),
            (None, Some(700)),
            (Some(700), None),
        ] {
            assert!(uids_agree(a, b, 700).is_err(), "{a:?} {b:?}");
        }
        assert!(
            uids_agree(Some(1), Some(2), 700)
                .unwrap_err()
                .contains("proves nothing")
        );
    }
}
