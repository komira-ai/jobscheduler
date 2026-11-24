//! `js-agent __supervise-native`, the real binary: on macOS the start of a job under a profile,
//! as the test's own user (no new audit session and no drop: both need root), and elsewhere
//! the refusal at the first step that needs macOS. The test process stands in for the agent.
//!
//! Every wait here is for an event (the supervisor's records, the job's end), never for a
//! length of time.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use js_agent::mac_supervisor::{
    ExitRecord, LaunchRequest, Launcher, SuperviseSpec, Supervised, SystemLauncher,
};

use crate::support::Scratch;

/// The binary under test.
const AGENT: &str = env!("CARGO_BIN_EXE_js-agent");

/// One test's folders, resolved (a profile names resolved paths): the root, and in it the
/// record directory, mode 0700 as the agent makes it (the test user's, where the agent's is
/// root's).
struct Folders {
    _scratch: Scratch,
    root: PathBuf,
    records: PathBuf,
}

impl Folders {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let root = scratch.path("case");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let records = root.join("records");
        fs::create_dir(&records).unwrap();
        fs::set_permissions(&records, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _scratch: scratch,
            root,
            records,
        }
    }

    /// A folder `name` in the root, made.
    #[cfg(target_os = "macos")]
    fn folder(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir(&path).unwrap();
        path
    }

    /// What the supervisor wrote to `name` in the record directory.
    #[cfg(target_os = "macos")]
    fn record(&self, name: &str) -> String {
        fs::read_to_string(self.records.join(name)).unwrap()
    }
}

/// A spec for `program` with `args` under `profile`, as this process's own user, with limits
/// this user may set.
fn spec(
    folders: &Folders,
    profile: &str,
    params: &[(&str, &Path)],
    program: &str,
    args: &[&str],
) -> SuperviseSpec {
    let processes = rustix::process::getrlimit(rustix::process::Resource::Nproc)
        .maximum
        .map_or(1000, |most| u32::try_from(most).unwrap_or(u32::MAX));
    SuperviseSpec {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        new_session: false,
        profile: profile.to_owned(),
        params: params
            .iter()
            .map(|(name, path)| ((*name).to_owned(), path.display().to_string()))
            .collect(),
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        max_file_bytes: 1 << 40,
        max_processes: processes,
        record_dir: folders.records.display().to_string(),
    }
}

/// The exit record, once `run` has ended (it ends on its own: this waits for an event).
fn ended(run: &mut dyn Supervised) -> ExitRecord {
    std::iter::repeat_with(|| {
        std::thread::yield_now();
        run.try_exit().unwrap()
    })
    .flatten()
    .next()
    .unwrap()
}

/// Off macOS the real launcher starts the real binary, which reads and removes its spec, makes
/// its records and fails at the limits, the first step an ordinary user with no new session
/// reaches; the agent's side reads that from the exit record. Catches a launcher whose command
/// line the verb does not answer, and a Linux build that would start a job with no fence.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_the_launched_supervisor_stops_at_the_limits() {
    use js_agent::mac_supervisor::{Ended, Step};

    let folders = Folders::new("supervisor-off-macos");
    let err = folders.root.join("err");
    let env = [("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
    let request = LaunchRequest {
        spec: spec(&folders, "(version 1)", &[], "/bin/true", &[]),
        env: &env,
        cwd: &folders.root,
        stdio: [
            Stdio::null(),
            Stdio::null(),
            fs::File::create(&err).unwrap().into(),
        ],
    };
    let mut run = SystemLauncher::at(AGENT).launch(request).unwrap();
    let refused = Ended::SetupFailed {
        step: Step::Limits,
        why: "macOS only".to_owned(),
    };
    assert_eq!(run.wait_ready(), Err(refused.clone()));
    assert_eq!(ended(&mut *run).ended, refused);
    assert!(!folders.records.join("spec.json").exists());
    assert_eq!(
        fs::read_to_string(err).unwrap(),
        "js-agent: __supervise-native: limits: macOS only\n"
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::Write as _;
    use std::process::{Command, Output};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    use js_agent::mac_supervisor::record::{parse_exit, parse_ready};
    use js_agent::mac_supervisor::{Ended, Ready, Step, VERB};

    use super::*;

    /// A profile that allows everything but writes beneath the folders its parameters
    /// `DENIED` and `RECORDS` name.
    const FENCE: &str = r#"(version 1)
(allow default)
(deny file-write* (subpath (param "DENIED")) (subpath (param "RECORDS")))
"#;

    /// A profile that allows everything.
    const OPEN: &str = "(version 1)\n(allow default)\n";

    /// Closes every descriptor above 2, then executes the rest of its command line: the
    /// supervisor starts with nothing this test process (or the harness that runs it) holds.
    const CLEAN_START: &str =
        r#"fd=3; while [ "$fd" -lt 256 ]; do eval "exec $fd>&-"; fd=$((fd + 1)); done; exec "$@""#;

    /// Runs the verb on `spec`, written as the agent writes it, from a clean start; its output,
    /// and its ready and exit records (`None` when not written).
    fn supervised(
        folders: &Folders,
        spec: &SuperviseSpec,
    ) -> (Output, Option<Ready>, Option<ExitRecord>) {
        let path = folders.records.join("spec.json");
        fs::write(&path, serde_json::to_string(spec).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", CLEAN_START, "sh", AGENT, VERB, "--spec"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(!path.exists(), "the spec stays: {output:?}");
        let ready = parse_ready(&folders.record("ready.json"));
        let exit = parse_exit(&folders.record("exit.json"));
        (output, ready, exit)
    }

    /// A job that exits 7: the status is 7, the ready record has the pid the job prints (its
    /// group is checked from outside the sandbox, where `ps` may look, by
    /// `the_job_runs_in_the_chosen_directory_and_its_own_group`), no audit session (none was
    /// asked for), and the exit record has the code and what the job used. Catches a supervisor
    /// that execs the job in its own place, reports a code it did not see, or records no
    /// usage.
    #[test]
    fn a_job_s_pid_and_code_are_recorded() {
        let folders = Folders::new("supervisor-code");
        let script = "echo $$; exit 7";
        let job = spec(&folders, OPEN, &[], "/bin/sh", &["-c", script]);
        let (output, ready, exit) = supervised(&folders, &job);
        assert_eq!(output.status.code(), Some(7), "{output:?}");
        let ready = ready.unwrap();
        let pid = ready.job_pid;
        assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{pid}\n"));
        assert_eq!(ready.asid, None);
        let exit = exit.unwrap();
        // The supervisor's wait says what the job used: a shell holds some memory.
        let usage = exit.usage.expect("the job's usage");
        assert!(usage.max_rss_bytes > 0, "{usage:?}");
        assert_eq!(
            exit,
            ExitRecord {
                ended: Ended::Exited(7),
                job_pid: Some(pid),
                asid: None,
                usage: Some(usage),
            }
        );
    }

    /// A job that kills itself with signal 9: `Signalled(9)`, status 137. Catches a signal read
    /// as an exit code.
    #[test]
    fn a_job_killed_by_a_signal_is_recorded_so() {
        let folders = Folders::new("supervisor-signal");
        let job = spec(&folders, OPEN, &[], "/bin/sh", &["-c", "kill -9 $$"]);
        let (output, _, exit) = supervised(&folders, &job);
        assert_eq!(output.status.code(), Some(137), "{output:?}");
        assert_eq!(exit.map(|exit| exit.ended), Some(Ended::Signalled(9)));
    }

    /// A profile that denies writes beneath a folder and beneath the record directory: the job
    /// can write neither, and the supervisor, under the same profile, still writes both its
    /// records through the files it opened before the profile applied. Catches a profile that
    /// is not applied, and a supervisor that writes its records by path.
    #[test]
    fn the_profile_fences_the_job_and_the_records_are_still_written() {
        let folders = Folders::new("supervisor-fence");
        let denied = folders.folder("denied");
        let script = r#"for dir in "$1" "$2"; do (echo x > "$dir/job.txt") 2>/dev/null && echo written || echo refused; done"#;
        let (to_deny, to_records) = (
            denied.display().to_string(),
            folders.records.display().to_string(),
        );
        let job = spec(
            &folders,
            FENCE,
            &[
                ("DENIED", denied.as_path()),
                ("RECORDS", folders.records.as_path()),
            ],
            "/bin/sh",
            &["-c", script, "sh", to_deny.as_str(), to_records.as_str()],
        );
        let (output, ready, exit) = supervised(&folders, &job);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "refused\nrefused\n"
        );
        assert!(!denied.join("job.txt").exists());
        assert!(!folders.records.join("job.txt").exists());
        assert!(ready.is_some());
        assert_eq!(exit.map(|exit| exit.ended), Some(Ended::Exited(0)));
    }

    /// A profile the library refuses: status 125, the exit record names the profile step, the
    /// error stream says so, and the job never runs (the file it would make is absent). Catches
    /// a job started unfenced after a refused profile.
    #[test]
    fn a_refused_profile_ends_the_start_before_the_job_runs() {
        let folders = Folders::new("supervisor-refused");
        let marker = folders.root.join("ran");
        let script = format!("echo ran > {}", marker.display());
        let job = spec(
            &folders,
            "(version 1)\n(no-such-operation)\n",
            &[],
            "/bin/sh",
            &["-c", &script],
        );
        let (output, ready, exit) = supervised(&folders, &job);
        assert_eq!(output.status.code(), Some(125), "{output:?}");
        assert_eq!(ready, None);
        assert!(matches!(
            exit.map(|exit| exit.ended),
            Some(Ended::SetupFailed {
                step: Step::Profile,
                ..
            })
        ));
        let err = String::from_utf8_lossy(&output.stderr);
        let prefix = format!("js-agent: {VERB}: profile: ");
        assert!(err.lines().any(|line| line.starts_with(&prefix)), "{err}");
        assert!(!marker.exists());
    }

    /// The job gets no descriptor beyond 0, 1 and 2 (the records are not passed on; each is
    /// probed by an external command, because bash saves a builtin's redirected output on a
    /// descriptor of its own from 10 up, which a builtin's probe would then find), and
    /// `SIGPIPE` has its default action in it (`trap -p PIPE` prints nothing, and a writer to
    /// a closed pipe is killed by the signal: status 141). Catches records, or the Rust
    /// runtime's ignored `SIGPIPE`, leaking into the job.
    #[test]
    fn the_job_gets_no_descriptor_of_the_supervisor_and_a_default_sigpipe() {
        let folders = Folders::new("supervisor-leaks");
        let script = r#"fd=3; while [ "$fd" -lt 256 ]; do /usr/bin/true 2>/dev/null >&"$fd" && echo "open $fd"; fd=$((fd + 1)); done
trap -p PIPE
{ /usr/bin/yes; echo "yes:$?" >&2; } | /usr/bin/head -n 1 > /dev/null"#;
        let job = spec(&folders, OPEN, &[], "/bin/sh", &["-c", script]);
        let (output, _, exit) = supervised(&folders, &job);
        assert_eq!(
            exit.map(|exit| exit.ended),
            Some(Ended::Exited(0)),
            "{output:?}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "yes:141\n");
    }

    /// Launched through the agent's launcher from a process that has three other live threads,
    /// the job still runs under its profile: it writes in its own folder and is refused beside
    /// it. Catches a start that applies the profile between `fork` and `exec` in a process with
    /// threads, where the library's allocation can meet another thread's lock.
    #[test]
    fn a_job_started_from_a_process_with_other_threads_runs_under_its_profile() {
        let folders = Folders::new("supervisor-threads");
        let (work, denied) = (folders.folder("work"), folders.folder("denied"));
        let out = folders.root.join("out");
        let script = r#"echo in > "$1/in" && echo in-written; (echo out > "$2/out") 2>/dev/null || echo out-refused"#;
        let (in_work, to_deny) = (work.display().to_string(), denied.display().to_string());
        let job = spec(
            &folders,
            FENCE,
            &[
                ("DENIED", denied.as_path()),
                ("RECORDS", folders.records.as_path()),
            ],
            "/bin/sh",
            &["-c", script, "sh", in_work.as_str(), to_deny.as_str()],
        );
        let (stop, stopped) = mpsc::channel::<()>();
        let stopped = Arc::new(Mutex::new(stopped));
        let workers: Vec<_> = (0..3)
            .map(|_| {
                let stopped = Arc::clone(&stopped);
                thread::spawn(move || {
                    let _ = stopped.lock().unwrap().recv();
                })
            })
            .collect();
        let env = [("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
        let request = LaunchRequest {
            spec: job,
            env: &env,
            cwd: &work,
            stdio: [
                Stdio::null(),
                fs::File::create(&out).unwrap().into(),
                Stdio::null(),
            ],
        };
        let mut run = SystemLauncher::at(AGENT).launch(request).unwrap();
        assert!(run.wait_ready().is_ok());
        let record = ended(&mut *run);
        drop(stop);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(record.ended, Ended::Exited(0));
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "in-written\nout-refused\n"
        );
        assert_eq!(fs::read_to_string(work.join("in")).unwrap(), "in\n");
        assert!(!denied.join("out").exists());
    }

    /// Through the launcher, the job's environment is exactly the one the agent chose, and
    /// nothing of this process's. (macOS's own `__CF_USER_TEXT_ENCODING`, which the system's
    /// frameworks set in a process that has none, is left out of the comparison.) Catches a
    /// supervisor that adds to the job's environment, or passes on the agent's own.
    #[test]
    fn the_job_gets_exactly_the_environment_the_agent_chose() {
        let folders = Folders::new("supervisor-env");
        let out = folders.root.join("out");
        let env = [
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("JS_SUPERVISED".to_owned(), "yes".to_owned()),
            ("LANG".to_owned(), "C".to_owned()),
        ];
        let request = LaunchRequest {
            spec: spec(&folders, OPEN, &[], "/usr/bin/env", &[]),
            env: &env,
            cwd: &folders.root,
            stdio: [
                Stdio::null(),
                fs::File::create(&out).unwrap().into(),
                Stdio::null(),
            ],
        };
        let mut run = SystemLauncher::at(AGENT).launch(request).unwrap();
        assert!(run.wait_ready().is_ok());
        assert_eq!(ended(&mut *run).ended, Ended::Exited(0));
        let printed = fs::read_to_string(&out).unwrap();
        let mut seen: Vec<&str> = printed
            .lines()
            .filter(|line| !line.starts_with("__CF_USER_TEXT_ENCODING="))
            .collect();
        seen.sort_unstable();
        let mut want: Vec<String> = env
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        want.sort_unstable();
        assert_eq!(seen, want);
    }

    /// Through the launcher, the job runs in the working directory the agent chose and, while
    /// it runs, leads a process group of its own, apart from the supervisor. Catches a job left
    /// in the supervisor's group, which a stop of the job's group would then miss or overreach.
    #[test]
    fn the_job_runs_in_the_chosen_directory_and_its_own_group() {
        let folders = Folders::new("supervisor-group");
        let work = folders.folder("work");
        let out = folders.root.join("out");
        let env = [("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
        let (reader, mut writer) = std::io::pipe().unwrap();
        let request = LaunchRequest {
            spec: spec(&folders, OPEN, &[], "/bin/sh", &["-c", "read line; pwd"]),
            env: &env,
            cwd: &work,
            stdio: [
                reader.into(),
                fs::File::create(&out).unwrap().into(),
                Stdio::null(),
            ],
        };
        let mut run = SystemLauncher::at(AGENT).launch(request).unwrap();
        let ready = run.wait_ready().unwrap();
        let job = rustix::process::Pid::from_raw(ready.job_pid).unwrap();
        assert_eq!(rustix::process::getpgid(Some(job)).unwrap(), job);
        let supervisor = rustix::process::Pid::from_raw(i32::try_from(run.pid()).unwrap()).unwrap();
        assert_ne!(job, supervisor);
        assert_eq!(
            rustix::process::getpgid(Some(supervisor)).unwrap(),
            supervisor
        );
        writer.write_all(b"go\n").unwrap();
        assert_eq!(ended(&mut *run).ended, Ended::Exited(0));
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            format!("{}\n", work.display())
        );
    }
}
