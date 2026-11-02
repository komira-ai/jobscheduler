//! Whether a launch agent has no path out of a run, and what run A's line for it becomes once
//! root has tried to load it ([`agent_verdict`]).
//!
//! NO PATH needs all of this evidence, and anything less leaves the kind NOT EXERCISED:
//! - the job's own `launchctl bootstrap` into its `user/<uid>` or `gui/<uid>` domain ran and
//!   exited non-zero with one of launchd's domain-level refusals ([`REFUSALS`]): not a
//!   generic failure such as `Bootstrap failed: 5: Input/output error`, which launchctl also
//!   gives for a plist it cannot load;
//! - the plist the job bootstrapped is valid: root's `plutil -lint` of it said OK (a missing,
//!   unreadable or invalid plist proves nothing about the domain);
//! - root tried at least once to load an agent, every attempt ran (one that did not start
//!   proves nothing), and none loaded it;
//! - root's `launchctl print` of the domain shows it absent, or present without the agent.
//!
//! A pool uid has no login session, so launchd may refuse both the job and root; the test then
//! still proves the job has no way to leave the item. A job whose own bootstrap worked has
//! seeded the agent: run B looks for it, and one that survives is a FAIL.

use super::lines::{Kind, Line, Verdict};
use super::look::{self, AgentAttempt, Outcome};

/// The program run A's seed of a launch agent runs.
const LAUNCHCTL: &str = "/bin/launchctl";

/// launchd's domain-level refusals, as `launchctl bootstrap` prints them: the domain does not
/// exist (113), the domain does not take the request (125), or launchd refuses the caller the
/// domain (1). Only these show the job has no way into its domain.
pub const REFUSALS: [&str; 3] = [
    "Could not find domain",
    "Domain does not support specified action",
    "Bootstrap failed: 1: Operation not permitted",
];

/// What root's `plutil -lint <plist>` of the job's plist said: `Ok(())` when it exited 0 and
/// printed `OK`, or why the plist is not shown valid. `run` is the exit status and the
/// output, or why the tool did not run.
///
/// # Errors
/// The tool did not run, exited non-zero, or did not say OK.
pub fn plist_lint(run: Result<(Option<i32>, String), String>) -> Result<(), String> {
    match run {
        Ok((Some(0), out)) if out.trim_end().ends_with(": OK") => Ok(()),
        Ok((code, out)) => Err(format!("plutil -lint exited {code:?}: {}", out.trim())),
        Err(why) => Err(format!("plutil -lint did not run: {why}")),
    }
}

/// Why the agent `label` has no path into `target`, for the line's text; or why that is not
/// shown. `job` is the failure text of run A's own seed (a [`look::tool_failure`] text),
/// `lint` root's check of the plist the job bootstrapped ([`plist_lint`]), `attempts` root's
/// attempts to load an agent, and `domain` root's look at `target`.
///
/// # Errors
/// Any piece of the evidence the module names is missing.
pub fn no_path(
    job: &str,
    lint: &Result<(), String>,
    attempts: &[AgentAttempt],
    (target, label): (&str, &str),
    domain: &Outcome,
) -> Result<String, String> {
    if !refused_by_launchctl(job) {
        return Err(format!(
            "run A's own bootstrap did not fail by launchctl exiting with a status: {job}"
        ));
    }
    let Some(refusal) = REFUSALS.iter().find(|r| job.contains(*r)) else {
        return Err(format!(
            "run A's own bootstrap failed without a domain-level refusal of launchd: {job}"
        ));
    };
    if let Err(why) = lint {
        return Err(format!("the job's plist is not shown valid: {why}"));
    }
    if attempts.is_empty() {
        return Err("root made no attempt to load the agent".to_owned());
    }
    if let Some(attempt) = attempts.iter().find(|a| a.code.is_none()) {
        return Err(format!(
            "root's attempt did not run: {}: {}",
            attempt.command, attempt.text
        ));
    }
    if look::agent_loaded(attempts) {
        return Err(format!("root loaded {label} into {target}"));
    }
    let state = match domain {
        Outcome::Absent => format!("{target} does not exist"),
        Outcome::Found(text) if !text.contains(label) => format!("{target} holds no {label}"),
        Outcome::Found(_) => return Err(format!("{target} holds {label}")),
        Outcome::CouldNotLook(why) => return Err(format!("launchctl print {target}: {why}")),
    };
    Ok(format!(
        "no leak path: run A's own bootstrap into {target} was refused by launchd ({refusal}: {job}), its plist is valid, root could not load one either, and {state}"
    ))
}

/// Whether `job` is launchctl's own failure: it ran (a pid) and exited with a status that is
/// not 0.
fn refused_by_launchctl(job: &str) -> bool {
    let prefix = format!("{LAUNCHCTL} (pid ");
    let status = job
        .split_once(" exited Some(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(code, _)| code.parse::<i32>().ok());
    job.starts_with(&prefix) & look::failed_tool_pid(job).is_some() & status.is_some_and(|c| c != 0)
}

/// The evidence root gathered for one launch agent kind after run A: its attempts to load an
/// agent into `target` (the agent `label`), its look at `target`, and its lint of the job's
/// plist.
pub struct Evidence<'a> {
    pub target: &'a str,
    pub label: &'a str,
    pub attempts: &'a [AgentAttempt],
    pub domain: &'a Outcome,
    pub lint: &'a Result<(), String>,
}

/// What run A's `line` for a launch agent becomes, and the PROBE note that says why: a line
/// the job seeded, or that the sandbox denied, stays as it is (both cover the kind already);
/// an agent root loaded is seeded, [`look::LOADED_BY_ROOT`]; a refused line with all of the
/// evidence of [`no_path`] has NO PATH; any other refused line stays refused (NOT EXERCISED).
pub fn agent_verdict(line: &Line, evidence: &Evidence<'_>) -> (Line, String) {
    let kind = line.kind;
    let Evidence {
        target,
        label,
        attempts,
        domain,
        lint,
    } = *evidence;
    // Root tries only where run A left no agent: a seeded line, or a later verdict, stays.
    if !matches!(line.verdict, Verdict::Refused | Verdict::Denied) {
        return (line.clone(), format!("{} kept as run A said", kind.label()));
    }
    if look::agent_loaded(attempts) {
        let text = format!(
            "{label} ({} into {target}: run A could not)",
            look::LOADED_BY_ROOT
        );
        let note = format!("{} {}", kind.label(), look::LOADED_BY_ROOT);
        return (
            Line::new(Verdict::Seeded, kind, line.token.clone(), &text),
            note,
        );
    }
    // A seed the sandbox denied covers the kind already.
    if line.verdict == Verdict::Denied {
        return (
            line.clone(),
            format!("{} denied by the profile", kind.label()),
        );
    }
    match no_path(&line.text, lint, attempts, (target, label), domain) {
        Ok(why) => {
            let note = format!("{} {why}", kind.label());
            (
                Line::new(Verdict::NoPath, kind, line.token.clone(), &why),
                note,
            )
        }
        Err(why) => (
            line.clone(),
            format!(
                "{} not exercised: root could not load it in {target}, and no path is shown ({why}); every attempt is above",
                kind.label()
            ),
        ),
    }
}

/// The two launch agent kinds and the domain each is seeded into.
pub const AGENT_KINDS: [(Kind, &str); 2] =
    [(Kind::LaunchAgent, "user"), (Kind::GuiLaunchAgent, "gui")];

#[cfg(test)]
mod tests {
    use super::super::token::Token;
    use super::*;

    const TARGET: &str = "user/501";
    const LABEL: &str = "com.komira.iso.t1";

    fn job_failed(code: i32, stderr: &str) -> String {
        look::tool_failure(LAUNCHCTL, 42, Some(code), stderr, "")
    }

    fn job_refused() -> String {
        job_failed(
            125,
            "Bootstrap failed: 125: Domain does not support specified action",
        )
    }

    fn lint_ok() -> Result<(), String> {
        Ok(())
    }

    fn root_failed() -> Vec<AgentAttempt> {
        vec![AgentAttempt {
            command: format!("launchctl bootstrap {TARGET} /p"),
            code: Some(125),
            text: "Bootstrap failed: 125: Domain does not support specified action".to_owned(),
        }]
    }

    fn token(n: u8) -> Token {
        Token::from_bytes([n; 8])
    }

    fn check(
        job: &str,
        lint: &Result<(), String>,
        attempts: &[AgentAttempt],
        domain: &Outcome,
    ) -> Result<String, String> {
        no_path(job, lint, attempts, (TARGET, LABEL), domain)
    }

    /// Asserts that each of launchd's domain-level refusals, with a valid plist, root's
    /// attempts run and failed and the domain absent, is no path, with the full reason text.
    /// It catches a launch agent left NOT EXERCISED when the job has no way to leave one.
    #[test]
    fn each_domain_refusal_with_all_evidence_has_no_path() {
        for (code, text) in [
            (113, "Could not find domain for user/501"),
            (
                125,
                "Bootstrap failed: 125: Domain does not support specified action",
            ),
            (1, "Bootstrap failed: 1: Operation not permitted"),
        ] {
            let job = job_failed(code, text);
            let refusal = REFUSALS.iter().find(|r| text.contains(*r)).unwrap();
            assert_eq!(
                check(&job, &lint_ok(), &root_failed(), &Outcome::Absent),
                Ok(format!(
                    "no leak path: run A's own bootstrap into user/501 was refused by launchd ({refusal}: {job}), its plist is valid, root could not load one either, and user/501 does not exist"
                ))
            );
        }
    }

    /// Asserts that a domain that exists but holds no agent of the label also leaves no path.
    /// It catches a check that takes any existing domain as the agent being loaded.
    #[test]
    fn a_domain_without_the_agent_has_no_path() {
        let domain = Outcome::Found("user/501 = {\n services = {\n }\n}\n".to_owned());
        let why = check(&job_refused(), &lint_ok(), &root_failed(), &domain).unwrap();
        assert!(
            why.ends_with("and user/501 holds no com.komira.iso.t1"),
            "{why}"
        );
    }

    /// Asserts that a domain holding the agent is a path. It catches an agent that got loaded
    /// (by anyone) being reported as having no way out.
    #[test]
    fn a_domain_holding_the_agent_is_a_path() {
        let domain = Outcome::Found(format!("services = {{\n 0 - {LABEL}\n}}"));
        assert_eq!(
            check(&job_refused(), &lint_ok(), &root_failed(), &domain),
            Err("user/501 holds com.komira.iso.t1".to_owned())
        );
    }

    /// Asserts that a domain that could not be looked at proves nothing. It catches a failed
    /// look read as absence.
    #[test]
    fn a_domain_that_could_not_be_looked_at_proves_nothing() {
        let domain = Outcome::CouldNotLook("launchctl exited Some(1): no".to_owned());
        assert_eq!(
            check(&job_refused(), &lint_ok(), &root_failed(), &domain),
            Err("launchctl print user/501: launchctl exited Some(1): no".to_owned())
        );
    }

    /// Asserts that only launchctl's own non-zero exit counts as the job's refusal: a tool that
    /// did not start, was killed, exited 0, or is another program proves nothing. It catches a
    /// missing launchctl or a crash read as launchd refusing the job.
    #[test]
    fn only_launchctls_own_refusal_counts_as_the_jobs() {
        let not_refusals = [
            "/bin/launchctl: No such file or directory (os error 2)".to_owned(),
            look::tool_failure(LAUNCHCTL, 42, None, "Could not find domain", ""),
            look::tool_failure(LAUNCHCTL, 42, Some(0), "Could not find domain", ""),
            look::tool_failure(
                "/usr/bin/security",
                42,
                Some(113),
                "Could not find domain",
                "",
            ),
        ];
        for job in not_refusals {
            assert_eq!(
                check(&job, &lint_ok(), &root_failed(), &Outcome::Absent),
                Err(format!(
                    "run A's own bootstrap did not fail by launchctl exiting with a status: {job}"
                ))
            );
        }
    }

    /// Asserts that a launchctl failure that is not one of launchd's domain-level refusals
    /// proves nothing: the input/output error launchctl also gives for a plist it cannot load,
    /// an unknown subcommand, and an empty message. It catches an ordinary failed probe read
    /// as the uid being unable to bootstrap.
    #[test]
    fn an_unrelated_launchctl_failure_proves_nothing() {
        for (code, text) in [
            (5, "Bootstrap failed: 5: Input/output error"),
            (64, "Unrecognized subcommand: bootstrapp"),
            (1, ""),
        ] {
            let job = job_failed(code, text);
            assert_eq!(
                check(&job, &lint_ok(), &root_failed(), &Outcome::Absent),
                Err(format!(
                    "run A's own bootstrap failed without a domain-level refusal of launchd: {job}"
                ))
            );
        }
    }

    /// Asserts that `plutil -lint` shows a plist valid only when it exited 0 and said OK: an
    /// invalid plist, an unreadable or missing one, a lint that did not run, and an exit 0
    /// without OK are each not valid. It catches a bootstrap refused for its broken plist read
    /// as a refusal of the domain.
    #[test]
    fn only_a_plist_lint_that_says_ok_shows_it_valid() {
        assert_eq!(
            plist_lint(Ok((Some(0), "/h/iso-t-user.plist: OK\n".to_owned()))),
            Ok(())
        );
        let bad = [
            (
                Ok((
                    Some(1),
                    "/h/p.plist: Encountered unexpected character < on line 3\n".to_owned(),
                )),
                "plutil -lint exited Some(1): /h/p.plist: Encountered unexpected character < on line 3",
            ),
            (
                Ok((
                    Some(1),
                    "/h/p.plist: file does not exist or is not readable\n".to_owned(),
                )),
                "plutil -lint exited Some(1): /h/p.plist: file does not exist or is not readable",
            ),
            (
                Ok((Some(0), String::new())),
                "plutil -lint exited Some(0): ",
            ),
            (Ok((None, String::new())), "plutil -lint exited None: "),
            (
                Err("/usr/bin/plutil: No such file or directory".to_owned()),
                "plutil -lint did not run: /usr/bin/plutil: No such file or directory",
            ),
        ];
        for (run, want) in bad {
            let lint = plist_lint(run);
            assert_eq!(lint, Err(want.to_owned()));
            assert_eq!(
                check(&job_refused(), &lint, &root_failed(), &Outcome::Absent),
                Err(format!("the job's plist is not shown valid: {want}"))
            );
        }
    }

    /// Asserts that root's attempts must have been made and must all have run: none at all, or
    /// one that did not start, proves nothing. It catches a root that never reached launchd
    /// counted as root being refused.
    #[test]
    fn root_attempts_that_did_not_run_prove_nothing() {
        assert_eq!(
            check(&job_refused(), &lint_ok(), &[], &Outcome::Absent),
            Err("root made no attempt to load the agent".to_owned())
        );
        let mut attempts = root_failed();
        attempts.push(AgentAttempt {
            command: "launchctl asuser 501 /usr/bin/true".to_owned(),
            code: None,
            text: "/bin/launchctl: killed".to_owned(),
        });
        assert_eq!(
            check(&job_refused(), &lint_ok(), &attempts, &Outcome::Absent),
            Err("root's attempt did not run: launchctl asuser 501 /usr/bin/true: /bin/launchctl: killed".to_owned())
        );
    }

    /// Asserts that an agent root loaded is a path. It catches root's successful bootstrap
    /// being ignored because the job's own failed.
    #[test]
    fn an_agent_root_loaded_is_a_path() {
        let mut attempts = root_failed();
        attempts.push(AgentAttempt {
            command: format!("launchctl asuser 501 /bin/launchctl bootstrap {TARGET} /p"),
            code: Some(0),
            text: String::new(),
        });
        assert_eq!(
            check(&job_refused(), &lint_ok(), &attempts, &Outcome::Absent),
            Err("root loaded com.komira.iso.t1 into user/501".to_owned())
        );
    }

    fn evidence<'a>(
        target: &'a str,
        attempts: &'a [AgentAttempt],
        domain: &'a Outcome,
        lint: &'a Result<(), String>,
    ) -> Evidence<'a> {
        Evidence {
            target,
            label: LABEL,
            attempts,
            domain,
            lint,
        }
    }

    /// Asserts how run A's line for each agent kind (both the `user` and the `gui` domain)
    /// becomes its final line: a refused line with all the evidence has no path; with a domain
    /// that could not be looked at it stays refused (not exercised); a seeded line and a
    /// denied line stay as they are; a refused or denied line whose agent root loaded becomes
    /// seeded, loaded by root. It catches the orchestration replacing a line it should keep, or
    /// keeping one it should replace.
    #[test]
    fn run_a_s_agent_lines_become_their_final_verdicts() {
        for (kind, domain) in AGENT_KINDS {
            let target = format!("{domain}/501");
            let job = job_failed(113, &format!("Could not find domain for {target}"));
            let refused = Line::new(Verdict::Refused, kind, token(1), &job);
            let attempts = root_failed();
            let (absent, unlooked) = (Outcome::Absent, Outcome::CouldNotLook("x".to_owned()));
            let lint = lint_ok();

            let (line, note) =
                agent_verdict(&refused, &evidence(&target, &attempts, &absent, &lint));
            assert_eq!((line.verdict, line.kind), (Verdict::NoPath, kind));
            assert!(
                note.starts_with(&format!("{} no leak path: ", kind.label())),
                "{note}"
            );

            let (line, note) =
                agent_verdict(&refused, &evidence(&target, &attempts, &unlooked, &lint));
            assert_eq!(line, refused);
            assert!(note.contains("not exercised"), "{note}");

            let seeded = Line::new(Verdict::Seeded, kind, token(2), LABEL);
            let (line, _) = agent_verdict(&seeded, &evidence(&target, &attempts, &absent, &lint));
            assert_eq!(line, seeded);

            let denied = Line::new(
                Verdict::Denied,
                kind,
                token(3),
                "Sandbox: launchctl(7) deny(1)",
            );
            let (line, note) =
                agent_verdict(&denied, &evidence(&target, &attempts, &absent, &lint));
            assert_eq!(line, denied);
            assert_eq!(note, format!("{} denied by the profile", kind.label()));

            let loaded = [AgentAttempt {
                command: format!("launchctl bootstrap {target} /p"),
                code: Some(0),
                text: String::new(),
            }];
            for line in [&refused, &denied] {
                let (got, _) = agent_verdict(line, &evidence(&target, &loaded, &absent, &lint));
                assert_eq!(got.verdict, Verdict::Seeded);
                assert_eq!(
                    got.text,
                    format!(
                        "{LABEL} ({} into {target}: run A could not)",
                        look::LOADED_BY_ROOT
                    )
                );
            }
        }
    }

    /// Asserts that an agent the job itself seeded, which run B then finds, fails the run even
    /// when every other kind is covered. It catches a working bootstrap of the job (a path out
    /// of the run) that does not fail the test.
    #[test]
    fn a_seeded_agent_that_survives_fails_the_run() {
        let lines: Vec<Line> = Kind::ALL
            .into_iter()
            .map(|kind| Line::new(Verdict::Seeded, kind, token(5), "x"))
            .collect();
        assert_eq!(look::final_verdict(&lines, 1, &[]), look::RunVerdict::Fail);
        assert_eq!(look::final_verdict(&lines, 0, &[]), look::RunVerdict::Pass);
    }

    /// Asserts that a no-path line covers its kind: with every other kind seeded and nothing
    /// survived, the run passes and the minimum set is met. It catches a no-path agent still
    /// counted as unexercised, which keeps the run from passing.
    #[test]
    fn a_no_path_agent_covers_its_kind() {
        let lines: Vec<Line> = Kind::ALL
            .into_iter()
            .map(|kind| match kind {
                Kind::LaunchAgent | Kind::GuiLaunchAgent => {
                    Line::new(Verdict::NoPath, kind, token(1), "no leak path")
                }
                _ => Line::new(Verdict::Seeded, kind, token(2), "x"),
            })
            .collect();
        assert_eq!(look::final_verdict(&lines, 0, &[]), look::RunVerdict::Pass);
        assert!(look::missing_minimum(&lines).is_empty());
        let mut refused = lines.clone();
        refused[6] = Line::new(Verdict::Refused, Kind::LaunchAgent, token(3), "no");
        assert_eq!(
            look::final_verdict(&refused, 0, &[]),
            look::RunVerdict::PassWithUnexercised(vec![Kind::LaunchAgent])
        );
    }

    /// Asserts that a run of only no-path and denied lines still misses an item seeded by the
    /// job. It catches a run that proves nothing passing because nothing could be seeded.
    #[test]
    fn no_path_alone_is_not_an_item_seeded_by_the_job() {
        let lines: Vec<Line> = Kind::ALL
            .into_iter()
            .map(|kind| match kind {
                Kind::LaunchAgent | Kind::GuiLaunchAgent => {
                    Line::new(Verdict::NoPath, kind, token(1), "no leak path")
                }
                _ => Line::new(Verdict::Denied, kind, token(2), "refused by the profile"),
            })
            .collect();
        assert_eq!(look::missing_minimum(&lines), ["an item seeded by the job"]);
    }

    /// Asserts that a no-path line survives encode and parse with its own word. It catches the
    /// handoff to run B dropping or garbling the verdict.
    #[test]
    fn a_no_path_line_round_trips() {
        let line = Line::new(
            Verdict::NoPath,
            Kind::GuiLaunchAgent,
            token(4),
            "no leak path",
        );
        assert!(line.encode().starts_with("ISO no-path gui-launch-agent\t"));
        assert_eq!(
            super::super::lines::parse_lines(&line.encode()).unwrap(),
            vec![line]
        );
    }
}
