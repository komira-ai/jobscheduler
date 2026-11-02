//! Whether a launch agent has no path out of a run: the job's own `launchctl bootstrap` into
//! its `user/<uid>` or `gui/<uid>` domain was refused by launchd, root could not load one
//! either, and root's `launchctl print` of the domain shows it absent, or present without the
//! agent. A pool uid has no login session, so launchd may refuse both; then the kind cannot be
//! seeded, and the test still proves the job has no way to leave it. A job whose own bootstrap
//! worked has seeded the agent: run B looks for it, and one that survives is a FAIL.

use super::look::{self, AgentAttempt, Outcome};

/// The program run A's seed of a launch agent runs.
const LAUNCHCTL: &str = "/bin/launchctl";

/// Why the agent `label` has no path into `target`, for the line's text; or why that is not
/// shown. `job` is the failure text of run A's own seed (a [`look::tool_failure`] text),
/// `attempts` root's attempts to load it, and `domain` root's look at `target`.
///
/// # Errors
/// The job's seed did not fail by launchctl exiting with a status (a tool that did not start
/// or was killed proves nothing), root loaded the agent, the domain holds the agent, or the
/// domain could not be looked at.
pub fn no_path(
    job: &str,
    attempts: &[AgentAttempt],
    (target, label): (&str, &str),
    domain: &Outcome,
) -> Result<String, String> {
    if !refused_by_launchctl(job) {
        return Err(format!(
            "run A's own bootstrap did not fail by launchctl exiting with a status: {job}"
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
        "no leak path: run A's own bootstrap into {target} was refused ({job}), root could not load one either, and {state}"
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

#[cfg(test)]
mod tests {
    use super::super::lines::{Kind, Line, Verdict};
    use super::super::token::Token;
    use super::*;

    const TARGET: &str = "user/501";
    const LABEL: &str = "com.komira.iso.t1";

    fn job_refused() -> String {
        look::tool_failure(
            LAUNCHCTL,
            42,
            Some(5),
            "Bootstrap failed: 5: Input/output error",
            "",
        )
    }

    fn root_failed() -> Vec<AgentAttempt> {
        vec![AgentAttempt {
            command: format!("launchctl bootstrap {TARGET} /p"),
            code: Some(5),
            text: "Bootstrap failed: 5: Input/output error".to_owned(),
        }]
    }

    fn token(n: u8) -> Token {
        Token::from_bytes([n; 8])
    }

    /// Asserts that a job refused by launchd, with root's attempts failed and the domain
    /// absent, has no path, and that the text names all three. It catches a launch agent left
    /// NOT EXERCISED when the job has no way to leave one.
    #[test]
    fn a_refused_job_with_root_failed_and_no_domain_has_no_path() {
        let why = no_path(
            &job_refused(),
            &root_failed(),
            (TARGET, LABEL),
            &Outcome::Absent,
        )
        .unwrap();
        assert_eq!(
            why,
            format!(
                "no leak path: run A's own bootstrap into user/501 was refused ({}), root could not load one either, and user/501 does not exist",
                job_refused()
            )
        );
    }

    /// Asserts that a domain that exists but holds no agent of the label also leaves no path.
    /// It catches a check that takes any existing domain as the agent being loaded.
    #[test]
    fn a_domain_without_the_agent_has_no_path() {
        let domain = Outcome::Found("user/501 = {\n services = {\n }\n}\n".to_owned());
        let why = no_path(&job_refused(), &root_failed(), (TARGET, LABEL), &domain).unwrap();
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
            no_path(&job_refused(), &root_failed(), (TARGET, LABEL), &domain),
            Err("user/501 holds com.komira.iso.t1".to_owned())
        );
    }

    /// Asserts that a domain that could not be looked at proves nothing. It catches a failed
    /// look read as absence.
    #[test]
    fn a_domain_that_could_not_be_looked_at_proves_nothing() {
        let domain = Outcome::CouldNotLook("launchctl exited Some(1): no".to_owned());
        assert_eq!(
            no_path(&job_refused(), &root_failed(), (TARGET, LABEL), &domain),
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
            look::tool_failure(LAUNCHCTL, 42, None, "", ""),
            look::tool_failure(LAUNCHCTL, 42, Some(0), "", ""),
            look::tool_failure("/usr/bin/security", 42, Some(5), "no", ""),
        ];
        for job in not_refusals {
            let result = no_path(&job, &root_failed(), (TARGET, LABEL), &Outcome::Absent);
            assert!(result.is_err(), "{job}: {result:?}");
        }
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
            no_path(&job_refused(), &attempts, (TARGET, LABEL), &Outcome::Absent),
            Err("root loaded com.komira.iso.t1 into user/501".to_owned())
        );
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
