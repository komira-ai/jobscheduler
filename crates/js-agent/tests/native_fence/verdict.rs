//! The pure part of the fence test: the places probed, what one probe of a place came to, the
//! lines the probe role prints and their parser, and what the four processes' answers add up
//! to. Nothing here touches the machine, so ordinary tests run it on every platform.

use std::collections::BTreeMap;
use std::fmt;
use std::io;

/// The names of the six privacy-only places (`js_agent::native::fence::PRIVACY_ONLY_PLACES`),
/// in the order of that list: only the privacy controls guard them from a job's account.
pub const PRIVACY: [&str; 6] = [
    "tcc-dir",
    "tcc-db",
    "timemachine-prefs",
    "tcc-dir-data",
    "tcc-db-data",
    "timemachine-prefs-data",
];

/// The other places every process probes: root's home under both spellings, a file the
/// invoking user made that only that user may read, a home of another user whose modes are
/// open, the invoking user's folders the privacy controls guard, and three other paths to the
/// privacy database that no read deny names as written (a symlink the test made, the
/// lower-case spelling, the volume-and-inode path): root opening them shows they reach the
/// database, so the job's refusal of them is the profile's.
pub const OTHER: [&str; 12] = [
    "root-home",
    "root-home-var",
    "other-user-private",
    "other-home",
    "owner-documents",
    "owner-desktop",
    "owner-mail",
    "owner-messages",
    "owner-safari",
    "evasion-symlink",
    "evasion-lowercase",
    "evasion-vol",
];

/// The invoking user's folders a Mac may not have (one whose user never opened Mail has no
/// `Library/Mail`): when root finds one absent there is nothing there to guard, and the place
/// is left out rather than making the run inconclusive.
pub const OPTIONAL: [&str; 5] = [
    "owner-documents",
    "owner-desktop",
    "owner-mail",
    "owner-messages",
    "owner-safari",
];

/// What the job makes of the privacy database itself, in its working directory: a hard link
/// and a clone, each then read. Each must be refused (an open is a failure).
pub const EVASIONS: [&str; 2] = ["evasion-hardlink", "evasion-clone"];

/// The same two made of a file the job wrote itself: each must open, which shows the job can
/// link and clone in its working directory, so a refusal of [`EVASIONS`] is the profile's
/// deny of the database and not a missing tool or a refused directory.
pub const LINK_CONTROLS: [&str; 2] = ["control-hardlink", "control-clone"];

/// Every place all four processes probe, privacy-only places first.
pub fn places() -> Vec<&'static str> {
    PRIVACY.iter().chain(OTHER.iter()).copied().collect()
}

/// What one probe of a place came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A directory listed or a file read.
    Open,
    /// The system answered permission denied (the errno is kept).
    Refused(i32),
    /// There is nothing at the path.
    Absent,
    /// Any other error (the errno is kept, -1 when there is none).
    Failed(i32),
}

impl Answer {
    /// The answer an attempt that returned `result` came to.
    pub fn from_io(result: &io::Result<()>) -> Self {
        match result {
            Ok(()) => Self::Open,
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound => Self::Absent,
                io::ErrorKind::PermissionDenied => Self::Refused(error.raw_os_error().unwrap_or(0)),
                _ => Self::Failed(error.raw_os_error().unwrap_or(-1)),
            },
        }
    }

    /// The answer that `text` spells (what [`fmt::Display`] prints), or `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        let word = parts.next()?;
        let code = parts.next();
        if parts.next().is_some() {
            return None;
        }
        match (word, code) {
            ("open", None) => Some(Self::Open),
            ("absent", None) => Some(Self::Absent),
            ("refused", Some(code)) => code.parse().ok().map(Self::Refused),
            ("failed", Some(code)) => code.parse().ok().map(Self::Failed),
            _ => None,
        }
    }
}

impl fmt::Display for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("open"),
            Self::Refused(code) => write!(f, "refused {code}"),
            Self::Absent => f.write_str("absent"),
            Self::Failed(code) => write!(f, "failed {code}"),
        }
    }
}

/// Who a probe process was: its pid and the process it answers to for the privacy controls.
/// (Which account the job ran as the test reads itself, from the owner of the file the job
/// wrote: see [`Outcomes::job_owner`].)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Who {
    /// The probe's own pid.
    pub pid: u32,
    /// The pid of its responsible process, `None` when the system could not say.
    pub responsible: Option<u32>,
}

/// The marker that starts the text of every line a probe prints.
const PREFIX: &str = "FENCE";

/// The line a probe prints for the place `name`.
pub fn place_line(name: &str, answer: &Answer) -> String {
    format!("{PREFIX} place {name} {answer}")
}

/// The line a probe prints about itself.
pub fn who_line(who: &Who) -> String {
    let responsible = who
        .responsible
        .map_or_else(|| "unreadable".to_owned(), |pid| pid.to_string());
    format!("{PREFIX} who pid={} responsible={responsible}", who.pid)
}

/// What one probe process printed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The answer to each place by name.
    pub answers: BTreeMap<String, Answer>,
    /// The probe's own line, `None` when it printed none.
    pub who: Option<Who>,
}

/// The value of the field `name=` in `fields`.
fn field<'a>(fields: &[&'a str], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
}

/// The `who` line's fields, or `None` when they do not spell one.
fn parse_who(rest: &str) -> Option<Who> {
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let responsible = match field(&fields, "responsible")? {
        "unreadable" => None,
        pid => Some(pid.parse().ok()?),
    };
    Some(Who {
        pid: field(&fields, "pid")?.parse().ok()?,
        responsible,
    })
}

/// Reads what a probe printed. The probe's text starts at the first `FENCE place ` or
/// `FENCE who ` of a line, wherever that is (libtest prints `test <name> ... ` before the
/// test's first output, on the same line). A line without one (libtest's own output, the
/// job's other words) is ignored, and so is a probe line that does not parse: a place with no
/// answer is a place the probe did not answer.
pub fn parse_report(text: &str) -> Report {
    let place = format!("{PREFIX} place ");
    let who = format!("{PREFIX} who ");
    let mut report = Report::default();
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once(place.as_str()) {
            if let Some((name, answer)) = rest.trim().split_once(' ')
                && let Some(answer) = Answer::parse(answer)
            {
                report.answers.insert(name.to_owned(), answer);
            }
        } else if let Some((_, rest)) = line.split_once(who.as_str())
            && let Some(found) = parse_who(rest)
        {
            report.who = Some(found);
        }
    }
    report
}

/// What the four probe processes printed, and what the test knows itself.
#[derive(Clone, Debug)]
pub struct Outcomes {
    /// The responsible pid of the test process itself, `None` when it could not be read.
    pub test_responsible: Option<u32>,
    /// The uid of the pool's one slot.
    pub slot_uid: u32,
    /// The owner of the file the job wrote in its run directory, which the test read before
    /// the run was removed: the account the job ran as. `None` when there was no such file.
    pub job_owner: Option<u32>,
    /// The test's ordinary child, as root, no profile.
    pub root: Report,
    /// A child as the slot uid, counted as the test, no profile.
    pub counted: Report,
    /// A child as the slot uid, disclaimed, no profile.
    pub disclaimed: Report,
    /// The job: a native run under the production profile.
    pub job: Report,
}

/// What the four processes add up to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The job was refused everywhere, and every control held.
    Pass,
    /// The job opened these places or evasions.
    Fail(Vec<String>),
    /// The job opened nothing, but a control failed, so its refusals prove nothing; each
    /// reason says which and why.
    Inconclusive(Vec<String>),
}

/// `name (answer)` for each of `names` that `report` did not answer `open`.
fn not_open(report: &Report, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| report.answers.get(**name) != Some(&Answer::Open))
        .map(|name| describe(report, name))
        .collect()
}

/// `name (answer)`, or `name (no answer)`.
fn describe(report: &Report, name: &str) -> String {
    match report.answers.get(name) {
        Some(answer) => format!("{name} ({answer})"),
        None => format!("{name} (no answer)"),
    }
}

/// The reasons, if any, that the processes' responsible pids are not what the test needs.
fn responsibility(o: &Outcomes, reasons: &mut Vec<String>) {
    let labelled = [
        ("root", &o.root),
        ("the counted slot process", &o.counted),
        ("the disclaimed slot process", &o.disclaimed),
        ("the job", &o.job),
    ];
    for (label, report) in labelled {
        match report.who {
            None => reasons.push(format!("{label} printed no who line")),
            Some(Who {
                responsible: None, ..
            }) => reasons.push(format!(
                "{label}: its responsible process could not be read"
            )),
            Some(_) => {}
        }
    }
    if o.test_responsible.is_none() {
        reasons.push("the test's own responsible process could not be read".to_owned());
    }
    let counted_as_test = [
        ("root", &o.root),
        ("the counted slot process", &o.counted),
        ("the job", &o.job),
    ];
    if let Some(test) = o.test_responsible {
        for (label, report) in counted_as_test {
            if let Some(Who {
                responsible: Some(pid),
                ..
            }) = report.who
                && pid != test
            {
                reasons.push(format!(
                    "{label} answers for pid {pid}, not for the test's responsible process {test}"
                ));
            }
        }
    }
    if let Some(Who {
        pid,
        responsible: Some(responsible),
        ..
    }) = o.disclaimed.who
        && responsible != pid
    {
        reasons.push(format!(
            "the disclaimed slot process answers for pid {responsible}, not for itself ({pid})"
        ));
    }
}

/// What the four processes' answers add up to: a failure when the job opened anything but its
/// own link controls (a fact whatever the controls say), else inconclusive when a control
/// failed, else a pass.
pub fn verdict(o: &Outcomes) -> Verdict {
    let opened: Vec<String> = o
        .job
        .answers
        .iter()
        .filter(|(name, answer)| {
            **answer == Answer::Open && !LINK_CONTROLS.contains(&name.as_str())
        })
        .map(|(name, _)| name.clone())
        .collect();
    if !opened.is_empty() {
        return Verdict::Fail(opened);
    }
    // A folder of the invoking user that root found absent is not on this Mac: left out.
    let all: Vec<&str> = places()
        .into_iter()
        .filter(|name| {
            !(OPTIONAL.contains(name) && o.root.answers.get(*name) == Some(&Answer::Absent))
        })
        .collect();
    let mut reasons = Vec::new();
    let root = not_open(&o.root, &all);
    if !root.is_empty() {
        reasons.push(format!(
            "root did not open {}: the chain holds no Full Disk Access grant, or the place is missing on this Mac",
            root.join(", ")
        ));
    }
    let counted = not_open(&o.counted, &PRIVACY);
    if !counted.is_empty() {
        reasons.push(format!(
            "the counted slot process did not open {}: the grant does not reach a slot uid, so the job's refusal proves nothing",
            counted.join(", ")
        ));
    }
    let disclaimed: Vec<&str> = PRIVACY
        .iter()
        .copied()
        .filter(|name| o.disclaimed.answers.get(*name) == Some(&Answer::Open))
        .collect();
    if !disclaimed.is_empty() {
        reasons.push(format!(
            "the disclaimed slot process opened {}: disclaiming does not drop the grant, so the control proves nothing",
            disclaimed.join(", ")
        ));
    }
    responsibility(o, &mut reasons);
    match o.job_owner {
        None => reasons.push(
            "the job's own file was not found, so the account it ran as is not known".to_owned(),
        ),
        Some(owner) if owner != o.slot_uid => reasons.push(format!(
            "the job's file is owned by {owner}, not by the slot's uid {}",
            o.slot_uid
        )),
        Some(_) => {}
    }
    let unshown: Vec<String> = all
        .iter()
        .filter(|name| o.root.answers.get(**name) == Some(&Answer::Open))
        .filter(|name| !matches!(o.job.answers.get(**name), Some(Answer::Refused(_))))
        .map(|name| describe(&o.job, name))
        .collect();
    if !unshown.is_empty() {
        reasons.push(format!(
            "the job was not shown refused: {}",
            unshown.join(", ")
        ));
    }
    let controls = not_open(&o.job, &LINK_CONTROLS);
    if !controls.is_empty() {
        reasons.push(format!(
            "the job could not link or clone a file of its own: {}, so its refusal to link or clone the database proves nothing",
            controls.join(", ")
        ));
    }
    let unrefused: Vec<String> = EVASIONS
        .iter()
        .filter(|name| !matches!(o.job.answers.get(**name), Some(Answer::Refused(_))))
        .map(|name| describe(&o.job, name))
        .collect();
    if !unrefused.is_empty() {
        reasons.push(format!(
            "the job's link or clone of the database was not shown refused: {}",
            unrefused.join(", ")
        ));
    }
    if reasons.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Inconclusive(reasons)
    }
}

/// The one final line of the test.
pub fn final_line(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Pass => "fence: PASS".to_owned(),
        Verdict::Fail(names) => format!("fence: FAIL {}", names.join(", ")),
        Verdict::Inconclusive(why) => format!("fence: INCONCLUSIVE {}", why.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test's own responsible pid in these tests.
    const TEST: u32 = 100;
    /// The slot's uid in these tests.
    const SLOT: u32 = 4312;

    fn who(pid: u32, responsible: u32) -> Option<Who> {
        Some(Who {
            pid,
            responsible: Some(responsible),
        })
    }

    /// Every one of `names` answered `answer`.
    fn answers(names: &[&str], answer: Answer) -> BTreeMap<String, Answer> {
        names.iter().map(|n| ((*n).to_owned(), answer)).collect()
    }

    /// The four reports of a run in which every control holds and the job is refused
    /// everywhere.
    fn sound() -> Outcomes {
        let root = Report {
            answers: answers(&places(), Answer::Open),
            who: who(10, TEST),
        };
        let mut counted = answers(&OTHER, Answer::Refused(13));
        counted.extend(answers(&PRIVACY, Answer::Open));
        let mut disclaimed = answers(&OTHER, Answer::Refused(13));
        disclaimed.extend(answers(&PRIVACY, Answer::Refused(1)));
        let mut job = answers(&places(), Answer::Refused(1));
        job.extend(answers(&EVASIONS, Answer::Refused(1)));
        job.extend(answers(&LINK_CONTROLS, Answer::Open));
        Outcomes {
            test_responsible: Some(TEST),
            slot_uid: SLOT,
            job_owner: Some(SLOT),
            root,
            counted: Report {
                answers: counted,
                who: who(11, TEST),
            },
            disclaimed: Report {
                answers: disclaimed,
                who: who(12, 12),
            },
            job: Report {
                answers: job,
                who: who(13, TEST),
            },
        }
    }

    /// The text of the one reason of an inconclusive verdict.
    fn only_reason(verdict: &Verdict) -> &str {
        match verdict {
            Verdict::Inconclusive(why) => {
                assert_eq!(why.len(), 1, "{why:?}");
                &why[0]
            }
            other => panic!("not inconclusive: {other:?}"),
        }
    }

    /// Asserts that a job refused everywhere, with every control holding, passes, and that
    /// the final line is exactly `fence: PASS`. Catches a verdict that asks for more than the
    /// controls and the refusals.
    #[test]
    fn a_job_refused_everywhere_under_sound_controls_passes() {
        let verdict = verdict(&sound());
        assert_eq!(verdict, Verdict::Pass);
        assert_eq!(final_line(&verdict), "fence: PASS");
    }

    /// Asserts that a job that opened a place fails, naming it, while the same run's controls
    /// have failed too (root did not open the place, the job's file is not the slot's): an
    /// opened place is a fact whatever the controls say. Catches a verdict that lets a failed
    /// control hide an open.
    #[test]
    fn a_job_that_opened_a_place_fails_even_with_a_failed_control() {
        let mut o = sound();
        o.job.answers.insert("tcc-db".to_owned(), Answer::Open);
        o.root.answers.insert("tcc-db".to_owned(), Answer::Absent);
        o.job_owner = Some(0);
        let verdict = verdict(&o);
        assert_eq!(verdict, Verdict::Fail(vec!["tcc-db".to_owned()]));
        assert_eq!(final_line(&verdict), "fence: FAIL tcc-db");
    }

    /// Asserts that a job that opened an evasion (a hard link read) and a place fails naming
    /// both, in name order, on one line. Catches an evasion that is not counted as an open.
    #[test]
    fn a_job_that_opened_an_evasion_fails_naming_it() {
        let mut o = sound();
        o.job
            .answers
            .insert("evasion-hardlink".to_owned(), Answer::Open);
        o.job.answers.insert("tcc-dir".to_owned(), Answer::Open);
        assert_eq!(
            final_line(&verdict(&o)),
            "fence: FAIL evasion-hardlink, tcc-dir"
        );
    }

    /// Asserts that a place root did not open (here a missing Time Machine file) makes the
    /// run inconclusive, with that one reason, in exact words. Catches a pass on a Mac whose
    /// chain holds no grant or that lacks a place.
    #[test]
    fn a_place_root_did_not_open_makes_it_inconclusive() {
        let mut o = sound();
        o.root
            .answers
            .insert("timemachine-prefs".to_owned(), Answer::Absent);
        let verdict = verdict(&o);
        assert_eq!(
            final_line(&verdict),
            "fence: INCONCLUSIVE root did not open timemachine-prefs (absent): the chain holds no Full Disk Access grant, or the place is missing on this Mac"
        );
    }

    /// Asserts that a root that gave no answer to a place is named as `no answer`. Catches a
    /// missing line counted as an open.
    #[test]
    fn a_place_root_gave_no_answer_to_is_named() {
        let mut o = sound();
        o.root.answers.remove("owner-mail");
        assert!(only_reason(&verdict(&o)).starts_with("root did not open owner-mail (no answer):"));
    }

    /// Asserts that a counted slot process refused a privacy-only place makes the run
    /// inconclusive, naming the place. Catches a pass where the grant does not reach a slot
    /// uid, so the job's refusal shows nothing.
    #[test]
    fn a_counted_slot_process_that_did_not_open_a_privacy_place_is_inconclusive() {
        let mut o = sound();
        o.counted
            .answers
            .insert("tcc-db-data".to_owned(), Answer::Refused(1));
        assert_eq!(
            only_reason(&verdict(&o)),
            "the counted slot process did not open tcc-db-data (refused 1): the grant does not reach a slot uid, so the job's refusal proves nothing"
        );
    }

    /// Asserts that a disclaimed slot process that opened a privacy-only place makes the run
    /// inconclusive. Catches a disclaim that does not drop the grant, which would make the
    /// job's refusal no evidence of the profile.
    #[test]
    fn a_disclaimed_slot_process_that_opened_a_privacy_place_is_inconclusive() {
        let mut o = sound();
        o.disclaimed
            .answers
            .insert("tcc-dir".to_owned(), Answer::Open);
        assert_eq!(
            only_reason(&verdict(&o)),
            "the disclaimed slot process opened tcc-dir: disclaiming does not drop the grant, so the control proves nothing"
        );
    }

    /// Asserts that a disclaimed slot process that opened another user's home, which its
    /// modes leave open, is not a control failure. Catches a control that confuses a mode
    /// with a grant.
    #[test]
    fn a_disclaimed_slot_process_may_open_a_place_the_modes_leave_open() {
        let mut o = sound();
        o.disclaimed
            .answers
            .insert("other-home".to_owned(), Answer::Open);
        assert_eq!(verdict(&o), Verdict::Pass);
    }

    /// Asserts that a missing who line, of each of the four processes in turn, makes the run
    /// inconclusive with a reason naming it. Catches a process that died before it answered
    /// being taken as a good control.
    #[test]
    fn a_missing_who_line_is_inconclusive() {
        let labels = [
            "root",
            "the counted slot process",
            "the disclaimed slot process",
            "the job",
        ];
        for (index, label) in labels.iter().enumerate() {
            let mut o = sound();
            [&mut o.root, &mut o.counted, &mut o.disclaimed, &mut o.job][index].who = None;
            assert_eq!(
                only_reason(&verdict(&o)),
                format!("{label} printed no who line")
            );
        }
    }

    /// Asserts that a responsible pid a process could not read, and the test's own, make the
    /// run inconclusive. Catches a comparison that treats an unreadable pid as a match.
    #[test]
    fn an_unreadable_responsible_pid_is_inconclusive() {
        let mut o = sound();
        o.job.who = Some(Who {
            pid: 13,
            responsible: None,
        });
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job: its responsible process could not be read"
        );
        let mut o = sound();
        o.test_responsible = None;
        assert_eq!(
            only_reason(&verdict(&o)),
            "the test's own responsible process could not be read"
        );
    }

    /// Asserts that root, the counted slot process and the job each make the run
    /// inconclusive when they do not answer for the test's responsible process, and that the
    /// disclaimed process does when it does not answer for itself. Catches a chain in which
    /// the starts do not carry the responsibility the way the test assumes.
    #[test]
    fn responsibility_that_is_not_the_assumed_one_is_inconclusive() {
        let mut o = sound();
        o.root.who = who(10, 7);
        assert_eq!(
            only_reason(&verdict(&o)),
            "root answers for pid 7, not for the test's responsible process 100"
        );
        let mut o = sound();
        o.counted.who = who(11, 12);
        assert_eq!(
            only_reason(&verdict(&o)),
            "the counted slot process answers for pid 12, not for the test's responsible process 100"
        );
        let mut o = sound();
        o.job.who = who(13, 13);
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job answers for pid 13, not for the test's responsible process 100"
        );
        let mut o = sound();
        o.disclaimed.who = who(12, TEST);
        assert_eq!(
            only_reason(&verdict(&o)),
            "the disclaimed slot process answers for pid 100, not for itself (12)"
        );
    }

    /// Asserts that a job whose file is owned by another uid than the slot's, and a job whose
    /// file the test did not find, make the run inconclusive. Catches a job that ran as
    /// another user, or not at all, being taken as the slot's.
    #[test]
    fn a_job_that_did_not_run_as_the_slot_is_inconclusive() {
        let mut o = sound();
        o.job_owner = Some(501);
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job's file is owned by 501, not by the slot's uid 4312"
        );
        o.job_owner = None;
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job's own file was not found, so the account it ran as is not known"
        );
    }

    /// Asserts that a place root opened and the job answered absent, failed, or not at all
    /// is not shown refused, so the run is inconclusive naming each. Catches a missing
    /// file or a crashed job taken as a refusal.
    #[test]
    fn a_job_that_was_not_shown_refused_is_inconclusive() {
        let mut o = sound();
        o.job.answers.insert("tcc-db".to_owned(), Answer::Absent);
        o.job
            .answers
            .insert("root-home".to_owned(), Answer::Failed(5));
        o.job.answers.remove("owner-safari");
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job was not shown refused: tcc-db (absent), root-home (failed 5), owner-safari (no answer)"
        );
    }

    /// Asserts that the job's link or clone of the database answered absent, failed or not
    /// at all makes the run inconclusive naming each, in exact words: only a refusal shows
    /// the deny. Catches a link that failed for a reason of its own (a missing tool, another
    /// volume) taken as the profile's refusal.
    #[test]
    fn a_link_or_clone_of_the_database_that_was_not_refused_is_inconclusive() {
        let mut o = sound();
        o.job
            .answers
            .insert("evasion-clone".to_owned(), Answer::Failed(18));
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job's link or clone of the database was not shown refused: evasion-clone (failed 18)"
        );
        o.job.answers.remove("evasion-hardlink");
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job's link or clone of the database was not shown refused: evasion-hardlink (no answer), evasion-clone (failed 18)"
        );
    }

    /// Asserts that a job that could not link or clone a file of its own makes the run
    /// inconclusive, and that its opening of those two is not a failure (they are its
    /// controls, not the database). Catches a refused database link taken as the profile's
    /// when the job cannot link at all, and a control counted as an open place.
    #[test]
    fn the_jobs_own_link_and_clone_are_its_controls() {
        let mut o = sound();
        o.job
            .answers
            .insert("control-clone".to_owned(), Answer::Failed(45));
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job could not link or clone a file of its own: control-clone (failed 45), so its refusal to link or clone the database proves nothing"
        );
        let o = sound();
        assert_eq!(o.job.answers.get("control-hardlink"), Some(&Answer::Open));
        assert_eq!(verdict(&o), Verdict::Pass);
    }

    /// Asserts that an invoking user's folder root found absent is left out (the run passes
    /// whatever the job answered for it), while a folder root could not open for another
    /// reason, and an absent privacy-only place, still make the run inconclusive. Catches a
    /// lab Mac without `Library/Mail` that could never pass, and an optional place that hides
    /// a missing grant.
    #[test]
    fn an_absent_folder_of_the_invoking_user_is_left_out() {
        let mut o = sound();
        o.root
            .answers
            .insert("owner-mail".to_owned(), Answer::Absent);
        o.job
            .answers
            .insert("owner-mail".to_owned(), Answer::Absent);
        assert_eq!(verdict(&o), Verdict::Pass);
        o.root
            .answers
            .insert("owner-mail".to_owned(), Answer::Refused(1));
        assert!(only_reason(&verdict(&o)).starts_with("root did not open owner-mail (refused 1):"));
        let mut o = sound();
        o.root.answers.insert("tcc-db".to_owned(), Answer::Absent);
        assert!(only_reason(&verdict(&o)).starts_with("root did not open tcc-db (absent):"));
    }

    /// Asserts that a path evasion root could not open (the lower-case spelling on a
    /// case-sensitive volume) makes the run inconclusive, and that one root opened and the job
    /// answered absent is not shown refused. Catches an evasion path that names nothing
    /// passing as a refusal.
    #[test]
    fn a_path_evasion_needs_roots_open_and_the_jobs_refusal() {
        let mut o = sound();
        o.root
            .answers
            .insert("evasion-lowercase".to_owned(), Answer::Absent);
        assert!(
            only_reason(&verdict(&o)).starts_with("root did not open evasion-lowercase (absent):")
        );
        let mut o = sound();
        o.job
            .answers
            .insert("evasion-vol".to_owned(), Answer::Absent);
        assert_eq!(
            only_reason(&verdict(&o)),
            "the job was not shown refused: evasion-vol (absent)"
        );
    }

    /// Asserts that several failed controls are all in the one final line, joined by
    /// semicolons, after `fence: INCONCLUSIVE `. Catches a verdict that stops at the first.
    #[test]
    fn every_reason_is_in_the_final_line() {
        let mut o = sound();
        o.root.who = None;
        o.job_owner = Some(501);
        assert_eq!(
            final_line(&verdict(&o)),
            "fence: INCONCLUSIVE root printed no who line; the job's file is owned by 501, not by the slot's uid 4312"
        );
    }

    /// Asserts each answer form's text and its parse back, and that junk, a missing code and
    /// a trailing word do not parse. Catches a printer and a parser that disagree.
    #[test]
    fn each_answer_form_prints_and_parses_back() {
        for (answer, text) in [
            (Answer::Open, "open"),
            (Answer::Refused(1), "refused 1"),
            (Answer::Absent, "absent"),
            (Answer::Failed(-1), "failed -1"),
        ] {
            assert_eq!(answer.to_string(), text);
            assert_eq!(Answer::parse(text), Some(answer));
        }
        for junk in [
            "",
            "opened",
            "refused",
            "refused x",
            "open 1",
            "absent 2",
            "failed 1 2",
        ] {
            assert_eq!(Answer::parse(junk), None, "{junk:?}");
        }
    }

    /// Asserts how an attempt's error becomes an answer: success open, not found absent, a
    /// permission error refused with its errno (EPERM and EACCES alike), anything else failed
    /// with its errno. Catches a refusal read as a failure, which would not count as shown.
    #[test]
    fn an_error_becomes_the_answer_it_is() {
        assert_eq!(Answer::from_io(&Ok(())), Answer::Open);
        let err = |code| Err(io::Error::from_raw_os_error(code));
        assert_eq!(Answer::from_io(&err(2)), Answer::Absent);
        assert_eq!(Answer::from_io(&err(1)), Answer::Refused(1));
        assert_eq!(Answer::from_io(&err(13)), Answer::Refused(13));
        assert_eq!(Answer::from_io(&err(5)), Answer::Failed(5));
        let other = Err(io::Error::other("no errno"));
        assert_eq!(Answer::from_io(&other), Answer::Failed(-1));
    }

    /// Asserts that the lines a probe prints parse back to the same report, the first one
    /// glued to libtest's `test child_of_the_runs ... ` as libtest prints it, and that
    /// libtest's own lines, the job's other output and malformed probe lines are ignored. An
    /// unreadable responsible pid prints as `unreadable` and reads back as `None`. Catches a
    /// parser that is thrown by the lines around the probe's.
    #[test]
    fn a_probes_lines_parse_back_and_other_lines_are_ignored() {
        let who = Who {
            pid: 7,
            responsible: None,
        };
        let text = [
            "running 1 test".to_owned(),
            format!(
                "test child_of_the_runs ... {}",
                place_line("tcc-db", &Answer::Refused(1))
            ),
            "FENCE place".to_owned(),
            "FENCE place tcc-dir sideways".to_owned(),
            "FENCE who pid=x responsible=1".to_owned(),
            place_line("tcc-dir", &Answer::Open),
            who_line(&who),
            "ok".to_owned(),
        ]
        .join("\n");
        assert_eq!(who_line(&who), "FENCE who pid=7 responsible=unreadable");
        let report = parse_report(&text);
        assert_eq!(report.who, Some(who));
        assert_eq!(
            report.answers,
            BTreeMap::from([
                ("tcc-db".to_owned(), Answer::Refused(1)),
                ("tcc-dir".to_owned(), Answer::Open),
            ])
        );
        let counted = Who {
            pid: 8,
            responsible: Some(9),
        };
        assert_eq!(parse_report(&who_line(&counted)).who, Some(counted));
        assert_eq!(parse_report("").who, None);
    }

    /// Asserts the three final lines of a failure with one and with two names and of a pass,
    /// and the place names: six privacy-only places, twelve others, two evasions and two link
    /// controls, none twice, every optional folder among the others.
    /// Catches a changed word in a line an operator greps for, and a name listed twice.
    #[test]
    fn the_final_lines_and_the_names_are_exact() {
        assert_eq!(
            final_line(&Verdict::Fail(vec!["a".into()])),
            "fence: FAIL a"
        );
        assert_eq!(
            final_line(&Verdict::Fail(vec!["a".into(), "b".into()])),
            "fence: FAIL a, b"
        );
        assert_eq!(final_line(&Verdict::Pass), "fence: PASS");
        let mut names: Vec<&str> = places();
        names.extend(EVASIONS);
        names.extend(LINK_CONTROLS);
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!((count, names.len()), (22, 22));
        assert!(OPTIONAL.iter().all(|name| OTHER.contains(name)));
    }
}
