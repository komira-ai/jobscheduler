//! The launchd probe's plain parts: the cases it runs, the job's plist, the files the job
//! writes and the readers of `ps` and `launchctl print`, and the judgement of what outlived
//! the job. They build and are tested on every platform; the probe that bootstraps the jobs
//! is macOS only (`macos/launchd.rs`).
//!
//! The question (`docs/design/mac-one-program.md`, section 4.3): when launchd stops a job,
//! restarts it or sees it die, which of the job's children does it end? The job starts one
//! child of each [`Kind`] and stays; the probe stops the job one [`Stop`] way, with and
//! without `AbandonProcessGroup`, and reads which children run on and with which parent.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The file in a case's state directory where the job lists the children it started, one
/// line each, written whole (a temporary file renamed).
pub const CHILDREN: &str = "children";

/// The file in a case's state directory where each start of the job appends its pid.
pub const INSTANCES: &str = "instances";

/// How the probe ends the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `launchctl bootout`: launchd stops the job and forgets it.
    Bootout,
    /// `launchctl kickstart -k`: launchd kills the job and starts it again.
    Kickstart,
    /// `kill -9` of the job's process; `KeepAlive` starts it again.
    Kill,
}

impl Stop {
    /// Every way, in the order the probe runs them.
    pub const ALL: [Self; 3] = [Self::Bootout, Self::Kickstart, Self::Kill];

    /// The name in a probe's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bootout => "bootout",
            Self::Kickstart => "kickstart-k",
            Self::Kill => "kill-9",
        }
    }

    /// Whether launchd starts the job again after this stop, which is how the probe knows
    /// that launchd has dealt with the end of the job's first process.
    #[must_use]
    pub fn restarts(self) -> bool {
        self != Self::Bootout
    }
}

/// Where a child of the job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// In the job's own process group, as the job's other processes are.
    SameGroup,
    /// In a process group of its own (`setpgid`), as a run's supervisor is.
    OwnGroup,
    /// In a session of its own (`setsid`).
    OwnSession,
    /// In a process group of its own, as another uid (root only): a supervisor's job.
    Dropped,
}

impl Kind {
    const ALL: [Self; 4] = [
        Self::SameGroup,
        Self::OwnGroup,
        Self::OwnSession,
        Self::Dropped,
    ];

    /// The children a job starts: [`Kind::Dropped`] only when it has ids to drop to.
    #[must_use]
    pub fn started(dropped: bool) -> Vec<Self> {
        let all = Self::ALL.into_iter();
        all.filter(|kind| dropped || *kind != Self::Dropped)
            .collect()
    }

    /// The name in the children file and in a probe's facts.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SameGroup => "same-group",
            Self::OwnGroup => "own-group",
            Self::OwnSession => "own-session",
            Self::Dropped => "dropped",
        }
    }

    /// The kind of `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Whether the design needs this child to outlive the job. A child in the job's own
    /// group is what launchd may clean up, so the probe only reports what happened to it.
    #[must_use]
    pub fn must_outlive(self) -> bool {
        self != Self::SameGroup
    }

    /// Whether the child leads a process group of its own from its start.
    #[must_use]
    pub fn own_group(self) -> bool {
        matches!(self, Self::OwnGroup | Self::Dropped)
    }
}

/// One run of the probe: a stop, with or without `AbandonProcessGroup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Case {
    /// How the job is ended.
    pub stop: Stop,
    /// Whether the plist sets `AbandonProcessGroup` true.
    pub abandon: bool,
}

impl Case {
    /// Every case: each stop without the key, then each with it.
    #[must_use]
    pub fn all() -> Vec<Self> {
        let with = |abandon| Stop::ALL.map(|stop| Self { stop, abandon });
        with(false).into_iter().chain(with(true)).collect()
    }

    /// The name in the probe's name, the job's label and its state directory.
    #[must_use]
    pub fn name(self) -> String {
        let key = ["", "-abandon"][usize::from(self.abandon)];
        format!("{}{key}", self.stop.name())
    }
}

/// The uid and gid a job's [`Kind::Dropped`] child runs as: `<uid>:<gid>` on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    /// The user id.
    pub uid: u32,
    /// The group id.
    pub gid: u32,
}

/// Text that is not `<uid>:<gid>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected <uid>:<gid> (two numbers), got {0:?}")]
pub struct BadIds(pub String);

impl FromStr for Ids {
    type Err = BadIds;

    fn from_str(text: &str) -> Result<Self, BadIds> {
        let bad = || BadIds(text.to_owned());
        let (uid, gid) = text.split_once(':').ok_or_else(bad)?;
        let number = |part: &str| part.parse::<u32>().map_err(|_| bad());
        Ok(Self {
            uid: number(uid)?,
            gid: number(gid)?,
        })
    }
}

impl fmt::Display for Ids {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// The job's launchd property list.
#[derive(Debug, Clone, Copy)]
pub struct Plist<'a> {
    /// The label.
    pub label: &'a str,
    /// The program and its arguments.
    pub program: &'a [String],
    /// Sets `AbandonProcessGroup` true when set; the key is absent otherwise.
    pub abandon: bool,
    /// Where launchd sends the job's standard output and error.
    pub log: &'a Path,
}

impl Plist<'_> {
    /// The XML text: `KeepAlive` and `RunAtLoad` true, and a `ThrottleInterval` of one
    /// second so that a restart is not held back by launchd's default of ten.
    #[must_use]
    pub fn render(&self) -> String {
        let string = |text: &str| format!("<string>{}</string>", escape(text));
        let args: String = self
            .program
            .iter()
            .map(|arg| format!("\t\t{}\n", string(arg)))
            .collect();
        let log = string(&self.log.to_string_lossy());
        let abandon =
            ["", "\t<key>AbandonProcessGroup</key>\n\t<true/>\n"][usize::from(self.abandon)];
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
\t<key>Label</key>\n\t{label}\n\
\t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
\t<key>KeepAlive</key>\n\t<true/>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
\t<key>ThrottleInterval</key>\n\t<integer>1</integer>\n\
{abandon}\
\t<key>StandardOutPath</key>\n\t{log}\n\
\t<key>StandardErrorPath</key>\n\t{log}\n\
</dict>\n</plist>\n",
            label = string(self.label),
        )
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

/// What a child says about itself when it has started: `pid=.. pgid=.. sid=.. uid=..`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// Its pid.
    pub pid: i32,
    /// Its process group.
    pub pgid: i32,
    /// Its session.
    pub sid: i32,
    /// Its real uid.
    pub uid: u32,
}

impl Hello {
    /// Reads the line [`Hello`]'s `Display` writes; `None` unless it has exactly these four
    /// fields, each a number of its type.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let mut field = |key: &str| {
            let (name, value) = words.next()?.split_once('=')?;
            (name == key).then_some(value)
        };
        let (pid, pgid, sid, uid) = (field("pid")?, field("pgid")?, field("sid")?, field("uid")?);
        let rest = words.next();
        let hello = Self {
            pid: pid.parse().ok()?,
            pgid: pgid.parse().ok()?,
            sid: sid.parse().ok()?,
            uid: uid.parse().ok()?,
        };
        rest.is_none().then_some(hello)
    }
}

impl fmt::Display for Hello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pid={} pgid={} sid={} uid={}",
            self.pid, self.pgid, self.sid, self.uid
        )
    }
}

/// A line of the children file: the child's kind and its [`Hello`], or why it did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    /// Which child.
    pub kind: Kind,
    /// What it said when it started, or the reason it did not.
    pub outcome: Result<Hello, String>,
}

/// The children file's text: `<kind> <hello>` or `<kind> error <reason>`, a line each.
#[must_use]
pub fn render_children(children: &[Spawned]) -> String {
    let line = |child: &Spawned| {
        let what = match &child.outcome {
            Ok(hello) => hello.to_string(),
            Err(reason) => format!("error {}", reason.replace('\n', " ")),
        };
        format!("{} {what}\n", child.kind.name())
    };
    children.iter().map(line).collect()
}

/// Reads [`render_children`]'s text. A line with an unknown kind or an unreadable start line
/// is left out, so the probe reports that child as not started.
#[must_use]
pub fn parse_children(text: &str) -> Vec<Spawned> {
    let read = |line: &str| {
        let (kind, what) = line.split_once(' ')?;
        let kind = Kind::from_name(kind)?;
        let outcome = match what.strip_prefix("error ") {
            Some(reason) => Err(reason.to_owned()),
            None => Ok(Hello::parse(what)?),
        };
        Some(Spawned { kind, outcome })
    };
    text.lines().filter_map(read).collect()
}

/// The pids in the instances file, in the order the job's starts wrote them.
#[must_use]
pub fn parse_instances(text: &str) -> Vec<i32> {
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// The tag a child carries on its command line (`--tag`), so that a reused pid is never
/// taken for it: `<state>/<kind>`.
#[must_use]
pub fn tag(state: &Path, kind: Kind) -> PathBuf {
    state.join(kind.name())
}

/// A process as `ps -o pid=,ppid=,pgid=,uid=,command=` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Its pid.
    pub pid: i32,
    /// Its parent's pid.
    pub ppid: i32,
    /// Its process group.
    pub pgid: i32,
    /// Its uid.
    pub uid: u32,
    /// Its command line.
    pub command: String,
}

/// The rows of `ps` output; a line that does not start with four numbers is left out.
#[must_use]
pub fn parse_ps(text: &str) -> Vec<Row> {
    let read = |line: &str| {
        let mut rest = line.trim_start();
        let mut word = || {
            let end = rest.find(char::is_whitespace)?;
            let (word, after) = rest.split_at(end);
            rest = after.trim_start();
            Some(word)
        };
        let (pid, ppid, pgid, uid) = (word()?, word()?, word()?, word()?);
        Some(Row {
            pid: pid.parse().ok()?,
            ppid: ppid.parse().ok()?,
            pgid: pgid.parse().ok()?,
            uid: uid.parse().ok()?,
            command: rest.to_owned(),
        })
    };
    text.lines().filter_map(read).collect()
}

/// What became of a child after its job was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// No process with its pid and tag is left.
    Gone,
    /// It runs on.
    Running {
        /// Its parent now.
        ppid: i32,
        /// Its process group now.
        pgid: i32,
        /// Its uid now.
        uid: u32,
    },
}

impl Fate {
    /// The child's fate: running when a row has its pid and its tag on the command line.
    #[must_use]
    pub fn of(hello: &Hello, tag: &Path, rows: &[Row]) -> Self {
        let tag = tag.to_string_lossy();
        let found = rows
            .iter()
            .find(|row| row.pid == hello.pid && row.command.contains(tag.as_ref()));
        found.map_or(Self::Gone, |row| Self::Running {
            ppid: row.ppid,
            pgid: row.pgid,
            uid: row.uid,
        })
    }

    /// Whether the child outlived the job the way a supervisor must: running, its parent
    /// launchd (pid 1).
    #[must_use]
    pub fn outlived(self) -> bool {
        matches!(self, Self::Running { ppid: 1, .. })
    }

    /// Whether the child no longer waits on the job's end: gone, or no longer the child of
    /// the job's process `job`.
    #[must_use]
    pub fn settled(self, job: i32) -> bool {
        !matches!(self, Self::Running { ppid, .. } if ppid == job)
    }
}

impl fmt::Display for Fate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("gone"),
            Self::Running { ppid, pgid, uid } => {
                write!(f, "runs on, parent pid {ppid}, pgid {pgid}, uid {uid}")
            }
        }
    }
}

/// The fates of the children that started, in the children file's order.
#[must_use]
pub fn fates(children: &[Spawned], state: &Path, rows: &[Row]) -> Vec<Fate> {
    let fate = |child: &Spawned| {
        let hello = child.outcome.as_ref().ok()?;
        Some(Fate::of(hello, &tag(state, child.kind), rows))
    };
    children.iter().filter_map(fate).collect()
}

/// The verdict on one case and the facts behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    /// Every child the design needs to outlive the job started and outlived it.
    pub held: bool,
    /// Per expected kind, what became of it.
    pub facts: Vec<(String, String)>,
}

/// Judges the children of `expected` kinds after the stop.
#[must_use]
pub fn judge(expected: &[Kind], children: &[Spawned], state: &Path, rows: &[Row]) -> Judgement {
    let mut held = true;
    let mut facts = Vec::new();
    for &kind in expected {
        let found = children.iter().find(|child| child.kind == kind);
        let (text, outlived) = match found.map(|child| &child.outcome) {
            None => (
                "not started: no line in the children file".to_owned(),
                false,
            ),
            Some(Err(reason)) => (format!("not started: {reason}"), false),
            Some(Ok(hello)) => {
                let fate = Fate::of(hello, &tag(state, kind), rows);
                (fate.to_string(), fate.outlived())
            }
        };
        held &= outlived || !kind.must_outlive();
        facts.push((kind.name().to_owned(), text));
    }
    Judgement { held, facts }
}

/// The lines of `launchctl print` that say what state the job is in, joined with ` | `.
#[must_use]
pub fn print_summary(text: &str) -> String {
    const KEYS: [&str; 7] = [
        "state = ",
        "pid = ",
        "runs = ",
        "last exit code = ",
        "last terminating signal = ",
        "properties = ",
        "abandon",
    ];
    let lines = text.lines().map(str::trim);
    let wanted = lines.filter(|line| KEYS.iter().any(|key| line.starts_with(key)));
    wanted.collect::<Vec<_>>().join(" | ")
}

#[cfg(test)]
mod tests;
