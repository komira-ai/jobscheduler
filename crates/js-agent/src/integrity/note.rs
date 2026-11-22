//! The words of what the checks report: the process as a finding or a note names it, and the
//! accounts that changed in `/etc/passwd` or `/etc/group`.

use std::collections::{BTreeMap, BTreeSet};

use super::{Integrity, Process};

/// The accounts of the lines that are in one of `before` and `after` and not in the other,
/// by the name before the first colon: `added <name>` for a name only the second has,
/// `removed <name>` for one only the first has, and `changed <name>` for one whose line is
/// not the same in both. In the order of the names.
pub(super) fn named_changes(before: &BTreeSet<String>, after: &BTreeSet<String>) -> Vec<String> {
    let name_of = |line: &String| line.split(':').next().unwrap_or_default().to_owned();
    let mut names: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for line in before.difference(after) {
        names.entry(name_of(line)).or_default().0 = true;
    }
    for line in after.difference(before) {
        names.entry(name_of(line)).or_default().1 = true;
    }
    names
        .into_iter()
        .map(|(name, seen)| match seen {
            (true, true) => format!("changed {name}"),
            (true, false) => format!("removed {name}"),
            (false, _) => format!("added {name}"),
        })
        .collect()
}

/// The owner of a process: `name (uid)` when a line of the user database `users` is of the
/// user `uid`, else the uid alone.
fn owner(users: &BTreeSet<String>, uid: u32) -> String {
    let id = uid.to_string();
    users
        .iter()
        .find_map(
            |line| match line.split(':').collect::<Vec<_>>().as_slice() {
                [name, _, found, ..] if *found == id => Some(format!("{name} ({uid})")),
                _ => None,
            },
        )
        .unwrap_or(id)
}

impl Integrity {
    /// The process as a finding or a note names it: `pid <pid> (<comm>, state <state>, cgroup
    /// <cgroup>, owner <owner>, cmd "<command line>")`. The owner is looked up in `users`. The
    /// command line is read from the host now; the `cmd` part is left out when it is empty (the
    /// process ended, it is a kernel thread, or the line cannot be read).
    pub(super) fn describe(&self, process: &Process, users: &BTreeSet<String>) -> String {
        let cmd = match self.view.cmdline(process.pid).as_str() {
            "" => String::new(),
            text => format!(", cmd \"{text}\""),
        };
        format!(
            "pid {} ({}, state {}, cgroup {}, owner {}{cmd})",
            process.pid,
            process.comm,
            process.state,
            process.cgroup,
            owner(users, process.uid)
        )
    }
}
