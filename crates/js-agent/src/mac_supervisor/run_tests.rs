//! The supervisor's steps over a host that records every call, and the real host's parts that
//! run anywhere.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::rc::Rc;

use js_mac_pool::seatbelt::SandboxInit;
use js_testkit::dir::TestDir;

use super::{JobChild, Reaped, SuperviseHost, SystemHost, supervise};
use crate::mac_supervisor::record::{
    EXIT, Ended, ExitRecord, JobRusage, READY, Ready, Records, Step, parse_exit, parse_ready,
};
use crate::mac_supervisor::spec::tests::spec;
use crate::mac_supervisor::{SuperviseSpec, status};

/// The calls made, by name, in order, shared with the job the host starts.
type Log = Rc<RefCell<Vec<String>>>;

/// A host that records each call in its log and fails the call named [`Fake::fail`], with the
/// error `<name> refused`. Its job has pid 4321 and ends as [`Fake::job`] says.
pub(crate) struct Fake {
    /// The calls.
    pub(crate) log: Log,
    /// The call that fails, by its name in the log.
    pub(crate) fail: Option<&'static str>,
    /// Whether the host says it runs as root.
    pub(crate) root: bool,
    /// The job's wait status, as `waitpid` gives it.
    pub(crate) job: i32,
    /// The profile it was given.
    pub(crate) applied: RefCell<Option<SandboxInit>>,
    /// A path that must be gone when the job starts, and whether it was.
    pub(crate) gone_at_spawn: RefCell<Option<(PathBuf, bool)>>,
    /// The pid the host says is its parent.
    pub(crate) parent: u32,
}

impl Fake {
    /// A host that fails nothing, runs as root, and whose job exits 0.
    pub(crate) fn new() -> Self {
        Self {
            log: Log::default(),
            fail: None,
            root: true,
            job: 0,
            applied: RefCell::new(None),
            gone_at_spawn: RefCell::new(None),
            parent: AGENT,
        }
    }

    /// Records `name`; its error when it is the call that fails.
    fn call(&self, name: &'static str) -> io::Result<()> {
        self.log.borrow_mut().push(name.to_owned());
        match self.fail {
            Some(fail) if fail == name => Err(io::Error::other(format!("{name} refused"))),
            _ => Ok(()),
        }
    }

    /// The calls made.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl SuperviseHost for Fake {
    fn open_records(&self, dir: &Path) -> io::Result<Records> {
        self.call("records")?;
        Records::open(dir)
    }

    fn audit_session(&self, _auid: u32) -> io::Result<i32> {
        self.call("audit-session")?;
        Ok(77)
    }

    fn is_root(&self) -> bool {
        self.root
    }

    fn drop_to(&self, _uid: u32, _gid: u32) -> io::Result<()> {
        self.call("drop")
    }

    fn limits(&self, _max_file_bytes: u64, _max_processes: u32) -> io::Result<()> {
        self.call("limits")
    }

    fn apply_profile(&self, init: &SandboxInit) -> io::Result<()> {
        self.call("profile")?;
        *self.applied.borrow_mut() = Some(init.clone());
        Ok(())
    }

    fn parent(&self) -> u32 {
        self.log.borrow_mut().push("parent".to_owned());
        self.parent
    }

    fn spawn(&self, _spec: &SuperviseSpec) -> io::Result<Box<dyn JobChild>> {
        let watched = self.gone_at_spawn.borrow().clone();
        if let Some((path, _)) = watched {
            let gone = !path.exists();
            *self.gone_at_spawn.borrow_mut() = Some((path, gone));
        }
        self.call("spawn")?;
        Ok(Box::new(FakeJob {
            log: Rc::clone(&self.log),
            fail: self.fail,
            status: self.job,
        }))
    }

    fn write_ready(&self, records: &mut Records, ready: &Ready) -> io::Result<()> {
        self.call("ready")?;
        records.write_ready(ready)
    }

    fn write_exit(&self, records: &mut Records, record: &ExitRecord) -> io::Result<()> {
        self.call("exit")?;
        records.write_exit(record)
    }
}

/// The pid of the agent that started the fake host's supervisor: its parent unless a test
/// says otherwise.
const AGENT: u32 = 812;

/// What the fake host's job reports it used when it is waited for.
const USED: JobRusage = JobRusage {
    cpu_usec: 1_500_000,
    max_rss_bytes: 3 << 20,
};

/// The fake host's job.
struct FakeJob {
    log: Log,
    fail: Option<&'static str>,
    status: i32,
}

impl JobChild for FakeJob {
    fn pid(&self) -> i32 {
        4321
    }

    fn wait(&mut self) -> io::Result<Reaped> {
        self.log.borrow_mut().push("wait".to_owned());
        if self.fail == Some("wait") {
            return Err(io::Error::other("wait refused"));
        }
        Ok(Reaped {
            status: ExitStatus::from_raw(self.status),
            usage: USED,
        })
    }

    fn kill_group(&mut self) -> io::Result<()> {
        self.log.borrow_mut().push("kill".to_owned());
        Ok(())
    }
}

/// A scratch directory with a record directory in it.
fn scratch(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), &format!("mac-supervisor-run-{name}"));
    let records = dir.join("records");
    fs::create_dir(&records).unwrap();
    (dir, records)
}

/// The supervisor's status, the line it wrote to its error stream, and its two records (each
/// `None` when not written), for `spec` over `host`.
fn supervised(
    host: &Fake,
    spec: &SuperviseSpec,
) -> (u8, String, Option<Ready>, Option<ExitRecord>) {
    let mut err = Vec::new();
    let code = supervise(host, spec, &mut err);
    let text = |name: &str| fs::read_to_string(Path::new(&spec.record_dir).join(name));
    (
        code,
        String::from_utf8(err).unwrap(),
        text(READY).ok().and_then(|text| parse_ready(&text)),
        text(EXIT).ok().and_then(|text| parse_exit(&text)),
    )
}

/// The log of a start that went through, as root and with a new session: the records are
/// opened before the drop (a supervisor that opened them after could not write as the slot
/// user), the profile is applied after the drop and before the job starts, and the job is
/// waited for between the ready and the exit record. As an ordinary user with no new session
/// the session and the drop are left out. The profile applied is the spec's. Catches a
/// reordering, and above all a job started before its fence.
#[test]
fn supervise_runs_the_steps_in_order() {
    let (_dir, records) = scratch("order");
    let wanted = spec(&records);
    let host = Fake::new();
    let (code, err, ready, exit) = supervised(&host, &wanted);
    assert_eq!(
        host.calls(),
        [
            "records",
            "audit-session",
            "drop",
            "limits",
            "profile",
            "spawn",
            "ready",
            "wait",
            "exit"
        ]
    );
    assert_eq!((code, err.as_str()), (0, ""));
    let want = Ready {
        job_pid: 4321,
        asid: Some(77),
    };
    assert_eq!(ready, Some(want));
    let ended = ExitRecord {
        ended: Ended::Exited(0),
        job_pid: Some(4321),
        asid: Some(77),
        usage: Some(USED),
    };
    assert_eq!(exit, Some(ended));
    assert_eq!(
        host.applied.borrow().clone(),
        Some(SandboxInit::new(&wanted.profile, &wanted.params).unwrap())
    );

    let (_dir, records) = scratch("order-user");
    let as_user = SuperviseSpec {
        new_session: false,
        ..spec(&records)
    };
    let host = Fake {
        root: false,
        ..Fake::new()
    };
    let (code, _, ready, _) = supervised(&host, &as_user);
    assert_eq!(
        host.calls(),
        [
            "records", "limits", "profile", "spawn", "ready", "wait", "exit"
        ]
    );
    assert_eq!(code, 0);
    let want = Ready {
        job_pid: 4321,
        asid: None,
    };
    assert_eq!(ready, Some(want));
}

/// Each step that can fail before the job starts, failed in turn: the status is 125, the exit
/// record names exactly that step with the host's words, the error stream has one line saying
/// so, no ready record is written and the job is never started (no call after the failed one
/// but the exit record). Catches a job that starts unfenced after a failed fence.
#[test]
fn a_failing_step_ends_the_start() {
    for (fail, step) in [
        ("audit-session", Step::AuditSession),
        ("drop", Step::Drop),
        ("limits", Step::Limits),
        ("profile", Step::Profile),
        ("spawn", Step::Spawn),
    ] {
        let (_dir, records) = scratch(fail);
        let host = Fake {
            fail: Some(fail),
            ..Fake::new()
        };
        let (code, err, ready, exit) = supervised(&host, &spec(&records));
        assert_eq!(code, status::SETUP_FAILED, "{fail}");
        assert_eq!(
            err,
            format!("js-agent: __supervise-native: {fail}: {fail} refused\n")
        );
        assert_eq!(ready, None, "{fail}");
        let want = ExitRecord {
            ended: Ended::SetupFailed {
                step,
                why: format!("{fail} refused"),
            },
            job_pid: None,
            asid: None,
            usage: None,
        };
        assert_eq!(exit, Some(want), "{fail}");
        let calls = host.calls();
        let at = calls.iter().position(|call| call == fail).unwrap();
        assert_eq!(calls[at + 1..], ["exit"], "{fail}: {calls:?}");
        assert!(!calls.contains(&"wait".to_owned()), "{fail}");
        assert!(!calls.contains(&"ready".to_owned()), "{fail}");
        if fail != "spawn" {
            assert!(!calls.iter().any(|call| call == "spawn"), "{fail}");
        }
    }
}

/// A spec that names the agent that started the supervisor has the parent checked after the
/// profile and right before the job: with that agent still the parent the job starts and runs
/// as before; with another parent (the agent died, and launchd took the supervisor) the start
/// ends at the spawn step, before the job, with the status 125, one error line that names the
/// agent and the parent, the exit record saying the same, and no ready record. Catches a job
/// started by a supervisor whose run no journal names: the next agent ends the run's account
/// and nothing would end that job.
#[test]
fn a_supervisor_whose_agent_is_gone_starts_no_job() {
    let (_dir, records) = scratch("parent-kept");
    let host = Fake::new();
    let kept = SuperviseSpec {
        parent: Some(AGENT),
        ..spec(&records)
    };
    let (code, err, ready, _) = supervised(&host, &kept);
    assert_eq!(
        host.calls(),
        [
            "records",
            "audit-session",
            "drop",
            "limits",
            "profile",
            "parent",
            "spawn",
            "ready",
            "wait",
            "exit"
        ]
    );
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(ready.map(|ready| ready.job_pid), Some(4321));

    let (_dir, records) = scratch("parent-gone");
    let host = Fake {
        parent: 1,
        ..Fake::new()
    };
    let gone = SuperviseSpec {
        parent: Some(AGENT),
        ..spec(&records)
    };
    let (code, err, ready, exit) = supervised(&host, &gone);
    let why = "the agent (pid 812) that started it is gone: its parent is 1";
    assert_eq!(
        host.calls(),
        [
            "records",
            "audit-session",
            "drop",
            "limits",
            "profile",
            "parent",
            "exit"
        ]
    );
    assert_eq!(code, status::SETUP_FAILED);
    assert_eq!(err, format!("js-agent: __supervise-native: spawn: {why}\n"));
    assert_eq!(ready, None);
    let want = ExitRecord {
        ended: Ended::SetupFailed {
            step: Step::Spawn,
            why: why.to_owned(),
        },
        job_pid: None,
        asid: None,
        usage: None,
    };
    assert_eq!(exit, Some(want));
}

/// Records that cannot be opened end the start with only the error line: nothing else is
/// called, nothing is written. Catches a supervisor that starts a job it could never report.
#[test]
fn records_that_cannot_be_opened_end_the_start_with_a_line() {
    let (_dir, records) = scratch("no-records");
    let host = Fake {
        fail: Some("records"),
        ..Fake::new()
    };
    let (code, err, ready, exit) = supervised(&host, &spec(&records));
    assert_eq!(code, status::SETUP_FAILED);
    assert_eq!(
        err,
        "js-agent: __supervise-native: records: records refused\n"
    );
    assert_eq!((ready, exit), (None, None));
    assert_eq!(host.calls(), ["records"]);
}

/// A ready record that cannot be written kills the started job's group and waits for it, and
/// the exit record says the records failed, with the job's pid; a wait that fails kills the
/// group and says the spawn step failed; an exit record that cannot be written still exits
/// with the job's status, and says so on the error stream. Catches a job left running that the
/// agent cannot find, and a lost exit record taken for a failed start.
#[test]
fn failures_after_the_job_started_are_reported() {
    let (_dir, records) = scratch("no-ready");
    let host = Fake {
        fail: Some("ready"),
        ..Fake::new()
    };
    let (code, err, ready, exit) = supervised(&host, &spec(&records));
    assert_eq!(code, status::SETUP_FAILED);
    assert_eq!(ready, None);
    let why = "writing the ready record: ready refused";
    assert_eq!(
        err,
        format!("js-agent: __supervise-native: records: {why}\n")
    );
    let want = ExitRecord {
        ended: Ended::SetupFailed {
            step: Step::Records,
            why: why.to_owned(),
        },
        job_pid: Some(4321),
        asid: Some(77),
        usage: None,
    };
    assert_eq!(exit, Some(want));
    assert_eq!(
        host.calls()[5..],
        ["spawn", "ready", "kill", "wait", "exit"]
    );

    let (_dir, records) = scratch("no-wait");
    let host = Fake {
        fail: Some("wait"),
        ..Fake::new()
    };
    let (code, err, ready, exit) = supervised(&host, &spec(&records));
    assert_eq!(code, status::SETUP_FAILED);
    assert!(ready.is_some());
    let why = "waiting for the job: wait refused";
    assert_eq!(err, format!("js-agent: __supervise-native: spawn: {why}\n"));
    assert_eq!(
        exit.map(|exit| exit.ended),
        Some(Ended::SetupFailed {
            step: Step::Spawn,
            why: why.to_owned()
        })
    );
    assert_eq!(host.calls()[6..], ["ready", "wait", "kill", "exit"]);

    let (_dir, records) = scratch("no-exit");
    let host = Fake {
        fail: Some("exit"),
        job: 7 << 8,
        ..Fake::new()
    };
    let (code, err, _, exit) = supervised(&host, &spec(&records));
    assert_eq!(code, 7);
    assert_eq!(exit, None);
    assert_eq!(
        err,
        "js-agent: __supervise-native: records: writing the exit record: exit refused\n"
    );
}

/// A job that exits 125 is recorded as its exit, and a failed step as the failure, and both
/// exit the supervisor 125; a job's 7 is 7, a job killed by signal 9 is 137. Catches an agent
/// side that would have to read the status instead of the record.
#[test]
fn the_job_status_and_setup_failure_are_told_apart() {
    for (job, fail, code, ended) in [
        (125 << 8, None, 125, Ended::Exited(125)),
        (
            0,
            Some("profile"),
            125,
            Ended::SetupFailed {
                step: Step::Profile,
                why: "profile refused".to_owned(),
            },
        ),
        (7 << 8, None, 7, Ended::Exited(7)),
        (9, None, 137, Ended::Signalled(9)),
    ] {
        let (_dir, records) = scratch("status");
        let host = Fake {
            fail,
            job,
            ..Fake::new()
        };
        let (got, _, _, exit) = supervised(&host, &spec(&records));
        assert_eq!(got, code, "{ended:?}");
        assert_eq!(exit.map(|exit| exit.ended), Some(ended));
    }
}

/// The real host starts the program in a process group of its own and waits for it; a job
/// that stops itself is killed with its group and reads as killed by signal 9; a program that
/// does not exist is not started; a job reaped once cannot be waited for again. Catches a wait
/// that misreads the status, a kill that misses the job, and a second wait that would read
/// another process's end.
#[test]
fn the_system_host_starts_waits_for_and_kills_the_job() {
    let (_dir, records) = scratch("system-job");
    let shell = |script: &str| SuperviseSpec {
        args: vec!["-c".to_owned(), script.to_owned()],
        ..spec(&records)
    };
    let mut job = SystemHost.spawn(&shell("exit 7")).unwrap();
    let pid = job.pid();
    assert!(pid > 0);
    assert_eq!(job.wait().unwrap().status.code(), Some(7));
    let again = job.wait().unwrap_err();
    assert_eq!(again.raw_os_error(), Some(libc::ECHILD));

    let mut job = SystemHost.spawn(&shell("kill -STOP $$")).unwrap();
    job.kill_group().unwrap();
    assert_eq!(job.wait().unwrap().status.signal(), Some(9));

    let absent = SuperviseSpec {
        program: records.join("absent").display().to_string(),
        ..spec(&records)
    };
    assert!(SystemHost.spawn(&absent).is_err());
    assert!(!SystemHost.is_root(), "the tests run as an ordinary user");
}

/// The real host's parent is this process's parent, as the standard library reads it: the
/// pid the check of a spec's `parent` compares. Catches a parent read as this process's own
/// pid, or its group's.
#[test]
fn the_system_host_names_this_process_s_parent() {
    assert_eq!(SystemHost.parent(), std::os::unix::process::parent_id());
}

/// The real host's wait says what the job used, its descendants it waited for included: a job
/// that waits for a child which burns CPU and fills memory reports at least the memory the
/// child held and some CPU. The child's work is a fixed amount, not a time, and nothing is
/// asserted of how long it took. Catches a wait that reports none of the CPU or the memory,
/// the descendants' left out, and a resident set read in the wrong unit (KiB taken for bytes
/// reads 1024 times too small).
#[test]
fn the_system_host_s_wait_reports_what_the_job_and_its_children_used() {
    let (_dir, records) = scratch("system-usage");
    // The child holds 64 MiB of text in a variable (a resident set of at least that) and
    // counts to 200000: work the kernel times as CPU, whatever the speed of the host. The job
    // waits for it.
    let child = "v=$(head -c 67108864 /dev/zero | tr '\\0' a); i=0; \
                 while [ $i -lt 200000 ]; do i=$((i+1)); done; [ ${#v} -gt 0 ]";
    let job = SuperviseSpec {
        args: ["-c", "sh -c \"$1\"; exit 0", "job", child]
            .map(str::to_owned)
            .to_vec(),
        ..spec(&records)
    };
    let reaped = SystemHost.spawn(&job).unwrap().wait().unwrap();
    assert_eq!(reaped.status.code(), Some(0));
    assert!(reaped.usage.cpu_usec > 0, "{:?}", reaped.usage);
    assert!(reaped.usage.max_rss_bytes >= 64 << 20, "{:?}", reaped.usage);
}

/// The real host's records are the files of [`Records`], written through it. Catches a host
/// that writes its records anywhere else.
#[test]
fn the_system_host_writes_the_records_it_opened() {
    let (_dir, records) = scratch("system-records");
    let mut held = SystemHost.open_records(&records).unwrap();
    let ready = Ready {
        job_pid: 12,
        asid: None,
    };
    SystemHost.write_ready(&mut held, &ready).unwrap();
    let exit = ExitRecord {
        ended: Ended::Exited(3),
        job_pid: Some(12),
        asid: None,
        usage: Some(USED),
    };
    SystemHost.write_exit(&mut held, &exit).unwrap();
    let text = |name: &str| fs::read_to_string(records.join(name)).unwrap();
    assert_eq!(parse_ready(&text(READY)), Some(ready));
    assert_eq!(parse_exit(&text(EXIT)), Some(exit));
}

/// Off macOS the real host has no audit session, drop, limit or profile, and says so for each.
/// Catches a Linux build that would start a job with no fence.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_the_fence_steps_are_refused() {
    let init = SandboxInit::new("(version 1)", &[]).unwrap();
    for refused in [
        SystemHost.audit_session(701).map(|_| ()),
        SystemHost.drop_to(701, 701),
        SystemHost.limits(1, 1),
        SystemHost.apply_profile(&init),
    ] {
        let why = refused.unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::Unsupported);
        assert_eq!(why.to_string(), "macOS only");
    }
}
