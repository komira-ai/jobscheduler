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
//! the state, so nothing can carry over). The denial must carry the SANDBOX's own signature:
//! the kernel's `Sandbox: <proc>(<pid>) deny(` line in the failure, or a bare `EPERM`/`EACCES` that
//! the unified log (read from the start of run A) confirms as that seed's by such a line with
//! its token, or with its operation from a process the job ran, bound by PID only (the pids the
//! runs print: the test process, each tool as a direct child, the process left running). A bare `EPERM` or `EACCES` alone is an ordinary permission failure (a
//! home with the wrong owner, a tool that is not executable) and proves nothing, like any
//! other failure (`ENOSPC`, a missing tool, a crash). The denied row is recorded as refused by
//! the profile. The minimum set (a file, a process, a keychain item, a launchd agent, shared
//! memory) is met when each kind is covered, and at least one item must have been really
//! seeded by the job (an agent loaded by root does not count).

use std::io::{self, Read as _};
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

/// The paths a `find -uid -print0` printed (NUL-separated, so a name with a line break is one path): those with a token in them, which are left behind by the
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
    for path in output.split('\0').filter(|path| !path.is_empty()) {
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

/// The text `child` puts in a launch agent's row when root loaded it: not the job's doing.
pub const LOADED_BY_ROOT: &str = "loaded by root";

/// Whether a seed failure could be a sandbox denial that the unified log may confirm: a
/// permission failure (`EPERM`, `EACCES`). Anything else never is.
pub fn is_permission_failure(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "operation not permitted",
        "permission denied",
        "(os error 1)",
        "(os error 13)",
    ]
    .iter()
    .any(|words| text.contains(words))
}

/// The processes the job ran: the pids the runs printed.
#[derive(Clone, Debug, Default)]
pub struct JobProcs {
    pub pids: Vec<u32>,
}

impl JobProcs {
    /// Whether `pid` is a process the job ran: one a run printed (its own, a tool it ran as a direct
    /// child, the process it left running). Never by name: another `security`, a retry or a
    /// concurrent job on the same Mac has the same names.
    fn owns(&self, pid: u32) -> bool {
        self.pids.contains(&pid)
    }
}

/// The arguments of `log show` for the sandbox lines since `start` (a local time, `YYYY-MM-DD
/// HH:MM:SS`, read just before run A began).
pub fn log_show_args(start: &str, predicate: &str) -> Vec<String> {
    [
        "show",
        "--start",
        start,
        "--style",
        "compact",
        "--predicate",
        predicate,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// A `find -print0` listing split on NUL into raw paths (a name need not be UTF-8).
pub fn nul_paths(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect()
}

/// The pids a run printed with `ISOPID <n>` lines.
pub fn parse_pids(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| line.strip_prefix("ISOPID ")?.trim().parse().ok())
        .collect()
}

/// `(process name, pid)` of a kernel sandbox line, `Sandbox: <name>(<pid>) deny(1) <op> ...`.
pub fn parse_sandbox_line(line: &str) -> Option<(String, u32)> {
    let rest = &line[line.find("Sandbox: ")? + "Sandbox: ".len()..];
    let (name, rest) = rest.split_once('(')?;
    let (pid, _) = rest.split_once(')')?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_owned(), pid.parse().ok()?))
}

/// Whether the unified log (`log show` lines of the sandbox) holds a denial that is this
/// seed's: a sandbox `deny(` line with the token in it (tokens are unique), or one for the
/// operation of the kind from a process the job ran (`job`). The operation alone is not
/// enough: other sandboxed processes log such lines all the time.
pub fn log_confirms_denial(kind: Kind, token: &Token, log: &str, job: &JobProcs) -> bool {
    let markers: &[&str] = match kind {
        Kind::Process => &["process-exec", "process-fork"],
        Kind::Keychain => &["com.apple.SecurityServer"],
        Kind::Defaults => &["cfprefs", "user-preference"],
        Kind::LaunchAgent | Kind::GuiLaunchAgent => &["launchd"],
        Kind::SysvShm => &["ipc-sysv-shm"],
        Kind::SysvSem => &["ipc-sysv-sem"],
        Kind::SysvMsg => &["ipc-sysv-msg"],
        Kind::PosixShm => &["ipc-posix-shm"],
        Kind::PosixSem => &["ipc-posix-sem"],
        _ => &[],
    };
    log.lines()
        .filter(|line| line.contains("Sandbox") && line.contains("deny("))
        .any(|line| {
            line.contains(token.as_str())
                || (markers.iter().any(|m| line.contains(m))
                    && parse_sandbox_line(line).is_some_and(|(_, pid)| job.owns(pid)))
        })
}

/// The verdict of a failed seed. `log` is the sandbox lines of the unified log for run A's
/// window, or none when it could not be read.
pub fn resolve_seed(
    kind: Kind,
    token: &Token,
    text: &str,
    log: Option<&str>,
    job: &JobProcs,
) -> Verdict {
    let confirmed = || {
        is_permission_failure(text)
            && log.is_some_and(|log| log_confirms_denial(kind, token, log, job))
    };
    match classify_seed_failure(text) == SeedFailure::ProfileRefused || confirmed() {
        true => Verdict::Denied,
        false => Verdict::Refused,
    }
}

/// Reads at most `limit` bytes and says whether the reader had more.
///
/// # Errors
/// The read failed.
pub fn read_bounded(reader: impl io::Read, limit: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut reader = reader;
    let mut bytes = Vec::new();
    (&mut reader).take(limit).read_to_end(&mut bytes)?;
    let mut one = [0u8; 1];
    let more = reader.read(&mut one)? > 0;
    Ok((bytes, more))
}

/// The children whose device differs from the root's (`find -x` skips them).
pub fn other_volume_children(root_dev: u64, children: &[(String, u64)]) -> Vec<String> {
    children
        .iter()
        .filter(|(_, dev)| *dev != root_dev)
        .map(|(path, _)| path.clone())
        .collect()
}

/// Why run A could not seed an item: the sandbox of the product profile said no (so the job
/// could not create the state and nothing can carry over), or anything else (a full disk, a
/// missing tool, a crash), which proves nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedFailure {
    ProfileRefused,
    Other,
}

/// Tells the two apart from the failure text of a seed: only the SANDBOX's own signature
/// (the kernel's `Sandbox: <proc>(<pid>) deny(` line) is the profile's. A bare `EPERM` or `EACCES` is an ordinary
/// permission failure (a home with the wrong owner, a tool that is not executable) and proves
/// nothing, unless the unified log confirms it ([`resolve_seed`]).
pub fn classify_seed_failure(text: &str) -> SeedFailure {
    match has_sandbox_signature(text) {
        true => SeedFailure::ProfileRefused,
        false => SeedFailure::Other,
    }
}

/// Whether `text` holds the kernel's sandbox line, `Sandbox: <proc>(<pid>) deny(`. A bare
/// word or `deny(1)` alone does not count.
fn has_sandbox_signature(text: &str) -> bool {
    text.match_indices("Sandbox: ").any(|(at, word)| {
        let rest = &text[at + word.len()..];
        let Some((name, after)) = rest.split_once('(') else {
            return false;
        };
        let Some((pid, after)) = after.split_once(')') else {
            return false;
        };
        !name.is_empty()
            && !name.contains(char::is_whitespace)
            && pid.parse::<u32>().is_ok()
            && after.starts_with(" deny(")
    })
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
    // Something the job itself seeded, so that a run where everything was denied (or loaded by
    // root) cannot pass having proven nothing.
    let by_the_job = lines
        .iter()
        .any(|line| line.verdict == Verdict::Seeded && !line.text.contains(LOADED_BY_ROOT));
    if !by_the_job {
        missing.push("an item seeded by the job");
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
            "/private/tmp/iso-{a}\0/private/var/folders/x/y/T/com.apple.something\0\0/Users/Shared/{a}.txt\0/private/tmp/other\0"
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
            text.push_str(&format!("/private/var/folders/x/y/T/file{n}\0"));
        }
        text.push_str(&format!("/private/var/folders/x/y/C/iso-{a}\0"));
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
        assert_eq!(missing_minimum(&[]).len(), 6);
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
        // A bare EPERM or EACCES is an ordinary permission failure (a home with the wrong
        // owner, a tool that is not executable): it proves nothing.
        for code in [libc::EPERM, libc::EACCES] {
            let text = format!("/x/y: {}", io::Error::from_raw_os_error(code));
            assert_eq!(classify_seed_failure(&text), SeedFailure::Other, "{text}");
        }
        assert_eq!(
            classify_seed_failure(
                "/usr/bin/security exited Some(1):  security: Operation not permitted"
            ),
            SeedFailure::Other
        );
        for text in [
            "launchctl exited Some(1): Bootstrap failed: Sandbox: launchctl(55) deny(1) mach-lookup com.apple.xpc.launchd",
            "Sandbox: sleep(123) deny(1) process-exec /bin/sleep",
            "/x/y: 2026-11-01 kernel: Sandbox: native_isolation(9) deny(1) file-write-create",
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
            [
                "shared memory (System V or POSIX)",
                "an item seeded by the job"
            ]
        );
    }

    /// Asserts that a run where every minimum kind is denied by the profile and nothing was
    /// seeded by the job proves nothing, and that an item loaded by root does not count as
    /// seeded by the job. It catches a fixture bug that leaves everything "covered".
    #[test]
    fn a_run_must_have_really_seeded_something() {
        let denied = |kind| Line::new(Verdict::Denied, kind, token(3), "deny(1) file-write-create");
        let all_denied: Vec<Line> = [
            Kind::HomeFile,
            Kind::Process,
            Kind::Keychain,
            Kind::LaunchAgent,
            Kind::SysvShm,
        ]
        .into_iter()
        .map(denied)
        .collect();
        assert_eq!(missing_minimum(&all_denied), ["an item seeded by the job"]);
        let by_root = Line::new(
            Verdict::Seeded,
            Kind::LaunchAgent,
            token(4),
            &format!("com.x ({LOADED_BY_ROOT} into user/1: run A could not)"),
        );
        let mut with_root = all_denied.to_vec();
        with_root[3] = by_root;
        assert_eq!(missing_minimum(&with_root), ["an item seeded by the job"]);
        with_root[0] = seeded(Kind::HomeFile);
        assert!(missing_minimum(&with_root).is_empty());
    }

    /// Asserts which failures could be a sandbox denial that the unified log may confirm
    /// (`EPERM`, `EACCES`, "Operation not permitted", "Permission denied") and which never
    /// are (`ENOSPC`, a missing tool, a crash).
    #[test]
    fn only_a_permission_failure_may_be_confirmed_by_the_log() {
        for code in [libc::EPERM, libc::EACCES] {
            let text = format!("/x/y: {}", io::Error::from_raw_os_error(code));
            assert!(is_permission_failure(&text), "{text}");
        }
        assert!(is_permission_failure(
            "security exited Some(1): Operation not permitted"
        ));
        for text in [
            "/x: No space left on device (os error 28)",
            "security exited None: ",
            "",
        ] {
            assert!(!is_permission_failure(text), "{text}");
        }
    }

    /// Asserts that a sandbox violation in the log confirms a seed only when it is that
    /// seed's: its token is in the denied path, or the denied operation is the kind's (the
    /// shared-memory family for shared memory), and the line is a sandbox deny. It catches a
    /// log match that excuses any denial for any kind.
    #[test]
    fn the_log_confirms_a_denial_of_the_same_seed_only() {
        let job = JobProcs { pids: vec![412] };
        let a = token(0xa1);
        let path = format!(
            "Sandbox: native_isolation(412) deny(1) file-write-create /var/folders/x/T/iso-{a}\n"
        );
        assert!(log_confirms_denial(Kind::UserTempDir, &a, &path, &job));
        let posix = "Sandbox: native_isolation(412) deny(1) ipc-posix-shm-write-create /jsiaaaa\n";
        assert!(log_confirms_denial(Kind::PosixShm, &a, posix, &job));
        assert!(!log_confirms_denial(Kind::PosixSem, &a, posix, &job));
        assert!(!log_confirms_denial(Kind::SysvShm, &a, posix, &job));
        let exec = "Sandbox: native_isolation(412) deny(1) process-exec* /bin/sleep\n";
        assert!(log_confirms_denial(Kind::Process, &a, exec, &job));
        assert!(!log_confirms_denial(Kind::HomeFile, &a, exec, &job));
        let other = token(0xb2);
        assert!(!log_confirms_denial(Kind::UserTempDir, &other, &path, &job));
        assert!(!log_confirms_denial(
            Kind::UserTempDir,
            &a,
            &format!("allow file-write-create iso-{a}"),
            &job
        ));
        assert!(!log_confirms_denial(Kind::UserTempDir, &a, "", &job));
    }

    /// Asserts that an operation-only sandbox line confirms a seed only when the process in
    /// the line is the job's, by a pid the runs printed and never by name (another `security`, a
    /// retry or a parallel runner has the same name). A line from any other process (a spotlight
    /// worker, say) never upgrades a seed, and a line with a token still does. It catches a
    /// log match that lets a bystander's denial cover the job's kind.
    #[test]
    fn an_operation_only_line_needs_the_jobs_own_process() {
        let a = token(0xa1);
        // The pids the runs printed: the test process, a tool it ran, the process it left.
        let job = JobProcs {
            pids: vec![412, 777, 900, 901],
        };
        let stranger = "Sandbox: mdworker(123) deny(1) ipc-posix-shm-write-create /x\n";
        assert!(!log_confirms_denial(Kind::PosixShm, &a, stranger, &job));
        let mine = "Sandbox: native_isolation(412) deny(1) ipc-posix-shm-write-create /x\n";
        assert!(log_confirms_denial(Kind::PosixShm, &a, mine, &job));
        // The right name with a pid the job did not print is another process: a retry, a
        // parallel runner, a concurrent job on the same Mac.
        let same_name = "Sandbox: native_isolation(999) deny(1) ipc-posix-shm-write-create /x\n";
        assert!(!log_confirms_denial(Kind::PosixShm, &a, same_name, &job));
        // The process the job left running counts by its pid, and only by its pid.
        let by_pid = "Sandbox: sleep(777) deny(1) process-exec* /bin/sleep\n";
        assert!(log_confirms_denial(Kind::Process, &a, by_pid, &job));
        let other_pid = "Sandbox: sleep(778) deny(1) process-exec* /bin/sleep\n";
        assert!(!log_confirms_denial(Kind::Process, &a, other_pid, &job));
        let other_tool = "Sandbox: mdworker(777) deny(1) process-exec* /bin/sleep\n";
        assert!(log_confirms_denial(Kind::Process, &a, other_tool, &job));
        let other_tool = "Sandbox: mdworker(778) deny(1) process-exec* /bin/sleep\n";
        assert!(!log_confirms_denial(Kind::Process, &a, other_tool, &job));
        // A tool the job ran counts by the pid it printed; another `security` does not.
        let tool = "Sandbox: security(900) deny(1) mach-lookup com.apple.SecurityServer\n";
        assert!(log_confirms_denial(Kind::Keychain, &a, tool, &job));
        let other = "Sandbox: security(5555) deny(1) mach-lookup com.apple.SecurityServer\n";
        assert!(!log_confirms_denial(Kind::Keychain, &a, other, &job));
        let defaults = "Sandbox: defaults(901) deny(1) user-preference-write x\n";
        assert!(!log_confirms_denial(Kind::Keychain, &a, defaults, &job));
        assert!(log_confirms_denial(Kind::Defaults, &a, defaults, &job));
        // A token in the line is unique to the job and needs no process.
        let tokened = format!("Sandbox: mdworker(123) deny(1) file-write-create /x/iso-{a}\n");
        assert!(log_confirms_denial(Kind::HomeFile, &a, &tokened, &job));
        // And the verdict follows.
        let bare = "/x: Operation not permitted (os error 1)";
        assert_eq!(
            resolve_seed(Kind::PosixShm, &a, bare, Some(stranger), &job),
            Verdict::Refused
        );
        assert_eq!(
            resolve_seed(Kind::Keychain, &a, bare, Some(other), &job),
            Verdict::Refused
        );
        assert_eq!(
            resolve_seed(Kind::PosixShm, &a, bare, Some(mine), &job),
            Verdict::Denied
        );
    }

    /// Asserts that the log is read from the start of run A (`--start`), not from a lookback
    /// before it (`--last`), and that the predicate is passed whole.
    #[test]
    fn the_log_is_read_from_the_start_of_run_a() {
        let args = log_show_args("2026-11-01 12:00:00", "eventMessage CONTAINS \"Sandbox\"");
        assert_eq!(
            args,
            [
                "show",
                "--start",
                "2026-11-01 12:00:00",
                "--style",
                "compact",
                "--predicate",
                "eventMessage CONTAINS \"Sandbox\""
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--last"));
    }

    /// Asserts that `find -print0` output is split on NUL into raw byte paths, empty entries
    /// dropped, and a name that is not UTF-8 kept byte for byte (so its content is scanned).
    #[test]
    fn nul_separated_paths_are_kept_as_bytes() {
        let bytes = b"/a\0/b\xff\xfe\0\0/c\0";
        let paths = nul_paths(bytes);
        assert_eq!(paths, [&b"/a"[..], &b"/b\xff\xfe"[..], &b"/c"[..]]);
        assert!(nul_paths(b"").is_empty());
    }

    /// Asserts that the kernel line is read as a process name and a pid, and a line of another
    /// shape is not, and that `ISOPID` lines are read among other output.
    #[test]
    fn a_sandbox_line_gives_its_process_and_the_runs_give_their_pids() {
        assert_eq!(
            parse_sandbox_line(
                "2026-11-01 12:00:00.1 Df kernel: Sandbox: mdworker(123) deny(1) file-read-data /x"
            ),
            Some(("mdworker".to_owned(), 123))
        );
        assert_eq!(
            parse_sandbox_line("Sandbox: com.apple.foo(9) deny(1) x"),
            Some(("com.apple.foo".to_owned(), 9))
        );
        for bad in [
            "",
            "Sandbox: nopid deny(1)",
            "Sandbox: x(abc) deny(1)",
            "deny(1) x(1)",
        ] {
            assert_eq!(parse_sandbox_line(bad), None, "{bad:?}");
        }
        assert_eq!(
            parse_pids("running\nISOPID 412\nISO seeded x\nISOPID 5\nISOPID x"),
            [412, 5]
        );
        assert!(parse_pids("").is_empty());
    }

    /// Asserts the verdict of a failed seed: the sandbox's own words are Denied; a bare
    /// permission failure is Denied only when the log confirms it, and Refused (proves
    /// nothing) when the log does not, could not be read or the failure is something else.
    #[test]
    fn a_failed_seed_is_denied_only_on_the_sandbox_signature() {
        let job = JobProcs::default();
        let a = token(0xa1);
        let sig = "Sandbox: t(3) deny(1) file-write-create";
        assert_eq!(
            resolve_seed(Kind::HomeFile, &a, sig, None, &job),
            Verdict::Denied
        );
        let bare = "/x/y: Permission denied (os error 13)";
        assert_eq!(
            resolve_seed(Kind::HomeFile, &a, bare, None, &job),
            Verdict::Refused
        );
        assert_eq!(
            resolve_seed(Kind::HomeFile, &a, bare, Some(""), &job),
            Verdict::Refused
        );
        let log = format!("Sandbox: t(1) deny(1) file-write-create /h/iso-{a}\n");
        assert_eq!(
            resolve_seed(Kind::HomeFile, &a, bare, Some(&log), &job),
            Verdict::Denied
        );
        let full = "/x: No space left on device (os error 28)";
        assert_eq!(
            resolve_seed(Kind::HomeFile, &a, full, Some(&log), &job),
            Verdict::Refused
        );
    }

    /// Asserts that a read of a file stops at the limit and says whether there was more: a
    /// file of exactly the limit is whole, a file of the limit plus a token is not, and the
    /// token past the limit is not in the bytes returned. It catches a scan that reads a
    /// prefix and calls the file clean.
    #[test]
    fn a_read_says_when_the_file_was_longer_than_the_limit() {
        let a = token(0xd4);
        let limit = 1 << 20;
        let whole = vec![7u8; limit];
        let (bytes, more) = read_bounded(&whole[..], limit as u64).unwrap();
        assert_eq!((bytes.len(), more), (limit, false));
        let mut longer = whole.clone();
        longer.extend_from_slice(a.as_str().as_bytes());
        let (bytes, more) = read_bounded(&longer[..], limit as u64).unwrap();
        assert_eq!(bytes.len(), limit);
        assert!(more);
        assert!(!content_with_token(&bytes, std::slice::from_ref(&a)));
        let (bytes, more) = read_bounded(&b""[..], 8).unwrap();
        assert_eq!((bytes.len(), more), (0, false));
    }

    /// Asserts that the children of a root on another volume are the ones named (`find -x`
    /// does not enter them).
    #[test]
    fn a_child_on_another_volume_is_named() {
        let children = [
            ("/Users/Shared/a".to_owned(), 5),
            ("/Users/Shared/mnt".to_owned(), 9),
            ("/Users/Shared/b".to_owned(), 5),
        ];
        assert_eq!(other_volume_children(5, &children), ["/Users/Shared/mnt"]);
        assert!(other_volume_children(5, &children[..1]).is_empty());
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
