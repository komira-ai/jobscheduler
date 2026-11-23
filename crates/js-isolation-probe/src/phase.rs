//! The three phases, run against a [`Host`], printed one line per probe and judged against
//! the pinned answers.
//!
//! - `probe` tries every write, read, connect, lookup and privilege and reports the limits;
//!   whatever it manages to make it removes at once.
//! - `leave` makes one marker of every kind it is allowed to, and a file in every place it
//!   may write, and keeps them.
//! - `find` (the next job, on the same slot or another) looks for each marker `leave` made
//!   and, on the leaver's own slot, removes what the reset left.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::expect::{Pins, Section, judge};
use crate::files;
use crate::host::{Named, Target, Who, with_extra};
use crate::limits;
use crate::marks::{self, Ctx, Kind};
use crate::net;
use crate::outcome::Line;
use crate::privilege;
use crate::tools::Tools;

/// What the phases run against.
pub struct Host<'a> {
    /// The system tools.
    pub tools: &'a dyn Tools,
    /// Who the job runs as.
    pub who: Who,
    /// The process's group ids.
    pub ids: Ids,
    /// The built-in write targets for an account ([`crate::host::writes`] on a Mac).
    pub writes: &'a dyn Fn(&Who) -> Vec<Target>,
    /// The built-in reads ([`crate::host::reads`] on a Mac).
    pub reads: Vec<Named>,
    /// How long a connect may take.
    pub connect_limit: Duration,
    /// How many small files `probe` makes.
    pub many: u32,
    /// The folders whose setuid and setgid programs `probe` lists.
    pub setuid_dirs: Vec<PathBuf>,
}

/// A process's group ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    /// The real group id.
    pub gid: u32,
    /// The effective group id.
    pub egid: u32,
    /// The supplementary groups, as `getgroups` gives them.
    pub groups: Vec<u32>,
}

impl Ids {
    /// Every group id the process holds, in order, each once.
    #[must_use]
    pub fn all(&self) -> Vec<u32> {
        let mut all = self.groups.clone();
        all.extend([self.gid, self.egid]);
        all.sort_unstable();
        all.dedup();
        all
    }
}

/// The phase to run, with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// `probe`.
    Probe {
        /// `--write NAME=DIR`.
        writes: Vec<Named>,
        /// `--read NAME=PATH`.
        reads: Vec<Named>,
        /// `--connect NAME=HOST:PORT`.
        connects: Vec<Named>,
        /// `--lookup NAME=HOST`.
        lookups: Vec<Named>,
        /// `--connect-unix NAME=PATH`.
        unix: Vec<Named>,
    },
    /// `leave`.
    Leave {
        /// The run's token.
        token: String,
        /// `--write NAME=DIR`.
        writes: Vec<Named>,
        /// How long the background process lingers.
        linger_secs: u64,
    },
    /// `find`.
    Find {
        /// The token `leave` was given.
        token: String,
        /// `--write NAME=DIR`, as `leave` was given them.
        writes: Vec<Named>,
        /// The uid that ran `leave`.
        uid: u32,
        /// Its user name.
        user: String,
        /// Its home.
        home: PathBuf,
    },
}

/// Runs `phase`, prints its lines and the verdict to `out`, and says whether it passed.
///
/// # Errors
/// A failed write to `out`.
pub fn run(phase: &Phase, host: &Host<'_>, pins: &Pins, out: &mut dyn Write) -> io::Result<bool> {
    let who = &host.who;
    let (section, head, lines) = match phase {
        Phase::Probe {
            writes,
            reads,
            connects,
            lookups,
            unix,
        } => {
            let tag = format!("probe-{}", std::process::id());
            let probe = Probe {
                writes,
                reads,
                connects,
                lookups,
                unix,
            };
            (
                Section::Probe,
                vec![header("probe", host)],
                probe.lines(host, &tag),
            )
        }
        Phase::Leave {
            token,
            writes,
            linger_secs,
        } => {
            let next = format!(
                "# next: find --token {token} --left-by-uid {} --left-by-user {} --left-by-home {}",
                who.uid,
                who.user,
                who.home.display()
            );
            (
                Section::Leave,
                vec![header("leave", host), next],
                leave(host, token, writes, *linger_secs),
            )
        }
        Phase::Find {
            token,
            writes,
            uid,
            user,
            home,
        } => {
            let same = *uid == who.uid;
            let (section, slot, leaver) = if same {
                (Section::FindSameSlot, "the same slot", who.clone())
            } else {
                (
                    Section::FindOtherSlot,
                    "another slot",
                    who.leaver(*uid, user, home),
                )
            };
            (
                section,
                vec![header("find", host), format!("# left by uid {uid}: {slot}")],
                find(host, token, writes, &leaver, same),
            )
        }
    };
    let verdict = judge(pins.table(section), &lines);
    let text = head
        .into_iter()
        .chain(lines.iter().map(ToString::to_string))
        .chain(verdict.lines(section));
    for line in text {
        writeln!(out, "{line}")?;
    }
    Ok(verdict.passed())
}

/// The first line: the phase, the account and its group ids, `HOME`, the account's own home
/// when it is known, and the working directory.
fn header(phase: &str, host: &Host<'_>) -> String {
    let who = &host.who;
    let ids = &host.ids;
    let groups = ids
        .groups
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let account = who.account_home.as_ref().map_or_else(String::new, |home| {
        format!(", account home {}", home.display())
    });
    format!(
        "# js-isolation-probe {phase} as {} (uid {}, gid {}, egid {}, groups {groups}), home {}{account}, in {}",
        who.user,
        who.uid,
        ids.gid,
        ids.egid,
        who.home.display(),
        who.cwd.display()
    )
}

struct Probe<'p> {
    writes: &'p [Named],
    reads: &'p [Named],
    connects: &'p [Named],
    lookups: &'p [Named],
    unix: &'p [Named],
}

impl Probe<'_> {
    fn lines(&self, host: &Host<'_>, tag: &str) -> Vec<Line> {
        let who = &host.who;
        let ctx = Ctx {
            tools: host.tools,
            who,
            linger_secs: 0,
        };
        let mut lines: Vec<Line> = with_extra((host.writes)(who), self.writes)
            .iter()
            .map(|t| files::write(t, tag))
            .collect();
        lines.extend(host.reads.iter().chain(self.reads).map(files::read));
        lines.extend(
            self.connects
                .iter()
                .map(|n| net::connect(n, host.connect_limit)),
        );
        lines.extend(self.lookups.iter().map(net::lookup));
        lines.extend(self.unix.iter().map(net::connect_unix));
        lines.push(privilege::sudo(host.tools));
        lines.push(privilege::su(host.tools));
        lines.push(privilege::admin(host.tools, who));
        lines.extend(privilege::groups(&host.ids.all()));
        lines.push(files::setuid(&host.setuid_dirs));
        lines.push(privilege::record_same_value(host.tools, who));
        lines.extend(
            Kind::PRIVILEGED
                .iter()
                .map(|kind| marks::try_and_undo(&ctx, *kind, tag)),
        );
        lines.extend(limits::lines());
        lines.push(files::space(&who.cwd));
        lines.push(files::many(&who.cwd, tag, host.many));
        lines
    }
}

fn leave(host: &Host<'_>, token: &str, writes: &[Named], linger_secs: u64) -> Vec<Line> {
    let who = &host.who;
    let ctx = Ctx {
        tools: host.tools,
        who,
        linger_secs,
    };
    let mut lines: Vec<Line> = Kind::ALL
        .iter()
        .map(|kind| marks::leave(&ctx, *kind, token))
        .collect();
    lines.extend(
        with_extra((host.writes)(who), writes)
            .iter()
            .map(|t| files::leave(t, token)),
    );
    lines
}

fn find(host: &Host<'_>, token: &str, writes: &[Named], leaver: &Who, same: bool) -> Vec<Line> {
    let ctx = Ctx {
        tools: host.tools,
        who: &host.who,
        linger_secs: 0,
    };
    let mut lines: Vec<Line> = Kind::ALL
        .iter()
        .map(|kind| marks::find(&ctx, *kind, token, leaver, same))
        .collect();
    lines.extend(
        with_extra((host.writes)(leaver), writes)
            .iter()
            .map(|t| files::find(t, token, same)),
    );
    lines
}

#[cfg(test)]
mod tests;
