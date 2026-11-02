//! What the test does once run A has seeded: a seed refused with a bare permission error is
//! confirmed (or not) as the sandbox's by the unified log, and a launch agent run A could not
//! load is loaded by root, every attempt printed, or shown to have no path out of the run.

use std::fs;

use super::lines::{Line, Verdict};
use super::no_path::{self, AGENT_KINDS};
use super::{Trial, look, run_bounded, view};

/// How many times, and how far apart, the unified log is read for a denial to confirm a seed.
const LOG_READS: u32 = 4;
const LOG_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);

impl Trial {
    /// A seed that failed with a bare permission error is the profile's only if the unified
    /// log, read from the start of run A, holds a sandbox denial that is that seed's. The log
    /// is not read when nothing failed that way. The unified log lags the event, so it is read
    /// again, up to `LOG_READS` times with a pause, until nothing is left to confirm; nothing
    /// asserts how long that takes. A log that cannot be read leaves the seeds refused.
    pub(super) fn confirm_denials_by_log(&mut self, since: &str, job: &look::JobProcs) {
        let predicate = "eventMessage CONTAINS \"Sandbox\" AND eventMessage CONTAINS \"deny(\"";
        for read in 1..=LOG_READS {
            let pending = |lines: &[Line]| {
                lines.iter().any(|line| {
                    line.verdict == Verdict::Refused && look::is_permission_failure(&line.text)
                })
            };
            if !pending(&self.seeded) {
                return;
            }
            let mut args = look::log_show_args(since, predicate);
            // Every level: a denial may be logged below the default one.
            args.extend(["--info".to_owned(), "--debug".to_owned()]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let log = match view("/usr/bin/log", &args) {
                Ok(text) => {
                    let (denies, own) = look::sandbox_log_counts(&text, job);
                    println!(
                        "PROBE note unified log read {read} of {LOG_READS} since {since}: {denies} sandbox deny line(s), {own} naming a pid of the job {:?}",
                        job.pids
                    );
                    if read == 1 {
                        for line in text
                            .lines()
                            .filter(|l| l.contains("Sandbox") && l.contains("deny("))
                            .take(5)
                        {
                            println!("PROBE note log sample: {line}");
                        }
                    }
                    Some(text)
                }
                Err(why) => {
                    println!("PROBE note the unified log could not be read: {why}");
                    None
                }
            };
            for line in &mut self.seeded {
                if line.verdict != Verdict::Refused {
                    continue;
                }
                let verdict =
                    look::resolve_seed(line.kind, &line.token, &line.text, log.as_deref(), job);
                if verdict == Verdict::Denied {
                    line.verdict = Verdict::Denied;
                    line.text = format!("confirmed in the unified log: {}", line.text);
                }
            }
            if read < LOG_READS && pending(&self.seeded) {
                std::thread::sleep(LOG_PAUSE);
            }
        }
    }

    /// A launch agent run A could not load (a user cannot load into its own domain on
    /// every macOS) is loaded by root into the user's domain: the item is still the run's
    /// to leave, and the domain is the one the helper's delete does not end. A pool uid has
    /// no login session, so launchd may have no domain for it: root then tries the documented
    /// ways to reach one (`launchctl asuser`) and prints every attempt with its exact command,
    /// status and text. An agent root cannot load either has NO PATH when the evidence
    /// `no_path.rs` names is all there (launchd's domain-level refusal of the job's own
    /// bootstrap, the job's plist valid by `plutil -lint` of `home`'s copy, every attempt of
    /// root run, and the domain shown without the agent), which covers the kind; otherwise it
    /// is NOT EXERCISED: it covers nothing, the run cannot pass, and the attempts say why.
    pub(super) fn bootstrap_agents_as_root(&mut self, uid: u32, home: &std::path::Path) {
        for (kind, domain) in AGENT_KINDS {
            let Some(line) = self.seeded.iter_mut().find(|line| {
                line.kind == kind && matches!(line.verdict, Verdict::Refused | Verdict::Denied)
            }) else {
                continue;
            };
            let label = format!("com.komira.iso.{}", line.token);
            let plist = self.fixture.root.join(format!("{label}-{domain}.plist"));
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n"
            );
            if let Err(why) = fs::write(&plist, body) {
                println!(
                    "PROBE note agent plist for {} not written: {why}",
                    kind.label()
                );
                continue;
            }
            let target = format!("{domain}/{uid}");
            let plist = plist.display().to_string();
            let uid_text = uid.to_string();
            // The attempts, in order; the first that works ends them.
            let mut plan: Vec<(String, Vec<String>)> = vec![(
                format!("launchctl bootstrap {target} {plist}"),
                vec!["bootstrap".into(), target.clone(), plist.clone()],
            )];
            if domain == "user" {
                // As the uid: a bootstrap through `asuser`; `asuser` of a no-op first, which
                // is how a session for the uid is made when launchd can make one.
                plan.push((
                    format!(
                        "launchctl asuser {uid_text} /bin/launchctl bootstrap {target} {plist}"
                    ),
                    vec![
                        "asuser".into(),
                        uid_text.clone(),
                        "/bin/launchctl".into(),
                        "bootstrap".into(),
                        target.clone(),
                        plist.clone(),
                    ],
                ));
                plan.push((
                    format!("launchctl asuser {uid_text} /usr/bin/true"),
                    vec!["asuser".into(), uid_text.clone(), "/usr/bin/true".into()],
                ));
                plan.push((
                    format!(
                        "launchctl asuser {uid_text} /bin/launchctl bootstrap {target} {plist}"
                    ),
                    vec![
                        "asuser".into(),
                        uid_text.clone(),
                        "/bin/launchctl".into(),
                        "bootstrap".into(),
                        target.clone(),
                        plist.clone(),
                    ],
                ));
            }
            let mut attempts: Vec<look::AgentAttempt> = Vec::new();
            for (command, args) in plan {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let attempt = match run_bounded("/bin/launchctl", &args) {
                    Ok((code, stdout, stderr)) => look::AgentAttempt {
                        command,
                        code,
                        text: format!(
                            "{} {}",
                            String::from_utf8_lossy(&stdout).trim(),
                            stderr.trim()
                        ),
                    },
                    Err(why) => look::AgentAttempt {
                        command,
                        code: None,
                        text: why,
                    },
                };
                println!("{}", look::attempt_probe(kind, &attempt));
                let worked = attempt.code == Some(0);
                // A no-op `asuser true` that works is not the agent being loaded.
                let loaded = worked && attempt.command.contains(" bootstrap ");
                attempts.push(attempt);
                if loaded {
                    break;
                }
            }
            let printed = run_bounded("/bin/launchctl", &["print", &target]);
            let domain_look = match printed {
                Ok((code, stdout, stderr)) => {
                    look::classify_launchctl_print(code, &String::from_utf8_lossy(&stdout), &stderr)
                }
                Err(why) => look::Outcome::CouldNotLook(why),
            };
            // The plist the job itself bootstrapped (child.rs `seed_agent`).
            let job_plist = home.join(format!("iso-{}-{domain}.plist", line.token));
            let job_plist = job_plist.display().to_string();
            let lint =
                no_path::plist_lint(run_bounded("/usr/bin/plutil", &["-lint", &job_plist]).map(
                    |(code, stdout, stderr)| {
                        (
                            code,
                            format!("{}{stderr}", String::from_utf8_lossy(&stdout)),
                        )
                    },
                ));
            let evidence = no_path::Evidence {
                target: &target,
                label: &label,
                attempts: &attempts,
                domain: &domain_look,
                lint: &lint,
            };
            let (verdict, note) = no_path::agent_verdict(line, &evidence);
            println!("PROBE note {note}");
            *line = verdict;
        }
    }
}
