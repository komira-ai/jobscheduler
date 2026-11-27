//! The plain parts of the launchd probe, built and tested on every platform: the job's
//! plist, the line the job writes once its child runs, the reader of `ps`, and the verdict.
//!
//! The probe (macOS only, in `macos/launchd.rs`) asks what `docs/design/mac-one-program.md`
//! section 4.3 rests on: a process that left a launchd job's process group runs on, with
//! parent pid 1, after the job is booted out. The job runs `child launchd-job`, which starts
//! `child hold` in a process group of its own (as the agent starts a supervisor, with
//! `process_group(0)`), writes [`ready_line`], and waits for it. The probe then boots the
//! job out and looks at the held child again. A process is the probe's only when its pid
//! matches and its command line carries the probe's tag, so a reused pid is never taken
//! for it.

use std::path::Path;

/// One row of `ps -o pid=,ppid=,pgid=,command=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The process id.
    pub pid: u32,
    /// The parent's process id.
    pub ppid: u32,
    /// The process group id.
    pub pgid: u32,
    /// The command line.
    pub command: String,
}

/// What one look at a pid found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// No process has the pid.
    Gone,
    /// A process has the pid but not the tag: the pid was reused.
    Other(Row),
    /// The probe's process.
    Ours(Row),
}

/// The plist of the probe's job: `program` run once at load, its stderr to `log`. No
/// `KeepAlive` (a booted-out job is not restarted) and no `AbandonProcessGroup` (the design
/// leaves it unset, D4).
#[must_use]
pub fn plist(label: &str, program: &[String], log: &Path) -> String {
    let args: String = program
        .iter()
        .map(|arg| format!("\t\t<string>{}</string>\n", escape(arg)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>{label}</string>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
{args}\t</array>\n\
\t<key>RunAtLoad</key>\n\
\t<true/>\n\
\t<key>StandardErrorPath</key>\n\
\t<string>{log}</string>\n\
</dict>\n\
</plist>\n",
        label = escape(label),
        log = escape(&log.to_string_lossy()),
    )
}

/// `text` with the three characters XML gives a meaning inside an element escaped.
#[must_use]
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The line the job writes once its held child runs.
#[must_use]
pub fn ready_line(job: u32, hold: u32) -> String {
    format!("job={job} hold={hold}\n")
}

/// The job's and the held child's pids from a [`ready_line`].
#[must_use]
pub fn ready(text: &str) -> Option<(u32, u32)> {
    let rest = text.trim_end().strip_prefix("job=")?;
    let (job, hold) = rest.split_once(" hold=")?;
    Some((job.parse().ok()?, hold.parse().ok()?))
}

/// One `ps -o pid=,ppid=,pgid=,command=` line.
#[must_use]
pub fn row(line: &str) -> Option<Row> {
    let mut rest = line.trim_start();
    let mut ids = [0u32; 3];
    for id in &mut ids {
        let end = rest.find(char::is_whitespace)?;
        *id = rest[..end].parse().ok()?;
        rest = rest[end..].trim_start();
    }
    let [pid, ppid, pgid] = ids;
    Some(Row {
        pid,
        ppid,
        pgid,
        command: rest.trim_end().to_owned(),
    })
}

/// What `ps` output says of `pid`: gone, another process, or the probe's (its command line
/// carries `tag`).
#[must_use]
pub fn seen(ps: &str, pid: u32, tag: &str) -> Seen {
    match ps.lines().filter_map(row).find(|r| r.pid == pid) {
        None => Seen::Gone,
        Some(r) if r.command.contains(tag) => Seen::Ours(r),
        Some(r) => Seen::Other(r),
    }
}

/// Before the bootout: the held child is the job's child, in a group of its own.
#[must_use]
pub fn under_job(seen: &Seen, job: u32) -> bool {
    matches!(seen, Seen::Ours(r) if r.ppid == job && r.pgid == r.pid && r.pgid != job)
}

/// After the bootout: the held child runs on, with parent pid 1, still in its own group.
#[must_use]
pub fn survived(seen: &Seen) -> bool {
    matches!(seen, Seen::Ours(r) if r.ppid == 1 && r.pgid == r.pid)
}

/// A [`Seen`] for the report.
#[must_use]
pub fn describe(seen: &Seen) -> String {
    match seen {
        Seen::Gone => "gone".to_owned(),
        Seen::Other(r) => format!("pid {} reused by another process", r.pid),
        Seen::Ours(r) => format!("pid {} ppid {} pgid {}", r.pid, r.ppid, r.pgid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "js-mac-probe.42";

    fn ours(pid: u32, ppid: u32, pgid: u32) -> Seen {
        Seen::Ours(Row {
            pid,
            ppid,
            pgid,
            command: format!("/p/js-mac-probe child hold --secs 300 --tag {TAG}"),
        })
    }

    #[test]
    fn the_plist_runs_the_program_once_with_its_arguments_escaped() {
        let program = ["/p/js-mac-probe".to_owned(), "a&b<c>".to_owned()];
        let text = plist("js-mac-probe.42", &program, Path::new("/w/job.log"));
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>js-mac-probe.42</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/p/js-mac-probe</string>
\t\t<string>a&amp;b&lt;c&gt;</string>
\t</array>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>StandardErrorPath</key>
\t<string>/w/job.log</string>
</dict>
</plist>
";
        assert_eq!(text, expected);
    }

    #[test]
    fn the_ready_line_reads_back_and_a_torn_or_foreign_line_does_not() {
        assert_eq!(ready(&ready_line(7, 9)), Some((7, 9)));
        assert_eq!(ready(""), None);
        assert_eq!(ready("job=7"), None);
        assert_eq!(ready("job=7 hold="), None);
        assert_eq!(ready("job=x hold=9"), None);
        assert_eq!(ready("pid=7 hold=9"), None);
    }

    #[test]
    fn a_ps_row_reads_three_ids_and_the_whole_command_line() {
        assert_eq!(
            row("  501     1   501 /p/js-mac-probe child hold --tag t  "),
            Some(Row {
                pid: 501,
                ppid: 1,
                pgid: 501,
                command: "/p/js-mac-probe child hold --tag t".to_owned(),
            })
        );
        assert_eq!(row(""), None);
        assert_eq!(row("501 1"), None);
        assert_eq!(row("501 x 501 cmd"), None);
    }

    #[test]
    fn seen_tells_the_probes_process_from_a_reused_pid_and_from_none() {
        let ps = format!(
            "  PID  PPID  PGID COMMAND\n  600   500   600 /p/js-mac-probe child hold --secs 300 --tag {TAG}\n  601     1   601 /bin/zsh\n"
        );
        assert_eq!(seen(&ps, 600, TAG), ours(600, 500, 600));
        let reused = Seen::Other(Row {
            pid: 601,
            ppid: 1,
            pgid: 601,
            command: "/bin/zsh".to_owned(),
        });
        assert_eq!(seen(&ps, 601, TAG), reused);
        assert_eq!(seen(&ps, 602, TAG), Seen::Gone);
        assert_eq!(seen("", 600, TAG), Seen::Gone);
    }

    #[test]
    fn under_the_job_means_the_jobs_child_in_a_group_of_its_own() {
        assert!(under_job(&ours(600, 500, 600), 500));
        // Another parent, the job's own group, or a group that is neither.
        assert!(!under_job(&ours(600, 1, 600), 500));
        assert!(!under_job(&ours(600, 500, 500), 500));
        assert!(!under_job(&ours(600, 500, 700), 500));
        // A child that is its job (the job's group is its own pid).
        assert!(!under_job(&ours(500, 500, 500), 500));
        assert!(!under_job(&Seen::Gone, 500));
        let reused = Seen::Other(Row {
            pid: 600,
            ppid: 500,
            pgid: 600,
            command: "/bin/zsh".to_owned(),
        });
        assert!(!under_job(&reused, 500));
    }

    #[test]
    fn survived_means_running_under_pid_1_in_its_own_group() {
        assert!(survived(&ours(600, 1, 600)));
        // Killed with the job, still the job's child, moved to another group, or a reused pid.
        assert!(!survived(&Seen::Gone));
        assert!(!survived(&ours(600, 500, 600)));
        assert!(!survived(&ours(600, 1, 500)));
        let reused = Seen::Other(Row {
            pid: 600,
            ppid: 1,
            pgid: 600,
            command: "/bin/zsh".to_owned(),
        });
        assert!(!survived(&reused));
    }

    #[test]
    fn describe_names_each_look() {
        assert_eq!(describe(&Seen::Gone), "gone");
        assert_eq!(describe(&ours(600, 1, 600)), "pid 600 ppid 1 pgid 600");
        let reused = Seen::Other(Row {
            pid: 600,
            ppid: 1,
            pgid: 600,
            command: "/bin/zsh".to_owned(),
        });
        assert_eq!(describe(&reused), "pid 600 reused by another process");
    }
}
