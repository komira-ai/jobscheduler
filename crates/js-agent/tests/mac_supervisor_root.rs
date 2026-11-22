//! The supervisor's privileged calls on macOS, executed for real by the real binary as root and
//! read back from the system: the new audit session (`macos/audit.rs`), the drop to the slot
//! user and the limits (`macos/sys.rs`). No fake stands in for any of them.
//!
//! Every test needs root, so each is `#[ignore]` with that reason: the ordinary test run lists
//! them as ignored, by name, and runs none. beta.yml's macOS job runs exactly this target as
//! root (`sudo ... cargo test -p js-agent --test mac_supervisor_root -- --ignored`). A test run
//! without root fails at its first line, saying so: it never passes without having run.
//!
//! The slot user is `nobody`, which every Mac has; the job is `/bin/sh` and reports what the
//! system says of it. Waits are for events (a line the job prints, its end), never for a time.

#![cfg(target_os = "macos")]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use js_agent::mac_supervisor::macos::audit::asid_of;
use js_agent::mac_supervisor::record::{parse_exit, parse_ready};
use js_agent::mac_supervisor::{Ended, Ready, SuperviseSpec, VERB};
use js_testkit::dir::TestDir;

/// The binary under test.
const AGENT: &str = env!("CARGO_BIN_EXE_js-agent");

/// The largest file the job may write.
const FILE_BYTES: u64 = 64 * 1024;

/// The most processes the slot user may have.
const PROCESSES: u32 = 300;

/// What the job does: prints its ids, its process limits and its file-size limits (`sh` is
/// bash in POSIX mode, whose `ulimit -f` counts 512-byte blocks), waits for a line on its input
/// (the test looks at its audit session meanwhile), then writes twice the file limit and
/// prints how `dd` ended.
const JOB: &str = r#"echo "ids $(/usr/bin/id -u) $(/usr/bin/id -g) $(/usr/bin/id -G)"
echo "processes $(ulimit -S -u) $(ulimit -H -u)"
echo "file blocks $(ulimit -S -f) $(ulimit -H -f)"
read line
/bin/dd if=/dev/zero of=./big bs=1024 count=128 2>/dev/null
echo "dd $?"
"#;

/// Fails the test unless it runs as root: a privileged test never passes without running.
fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "this test needs root on macOS: beta.yml runs it with sudo"
    );
}

/// An id as `id` or `ps` print it: macOS prints `nobody`'s as -2.
fn id32(text: &str) -> u32 {
    text.parse::<u32>()
        .unwrap_or_else(|_| text.parse::<i32>().unwrap().cast_unsigned())
}

/// The id `/usr/bin/id <flag> nobody` prints, as printed (the job's `id` prints the same) and as
/// a number.
fn nobody(flag: &str) -> (String, u32) {
    let out = Command::new("/usr/bin/id")
        .args([flag, "nobody"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let id = id32(&text);
    (text, id)
}

/// The user ids `ps` reports for `pid`: effective, real and saved. With all three the slot
/// user's, the process can never become root again.
fn uids_of(pid: u32) -> (u32, u32, u32) {
    let out = Command::new("/bin/ps")
        .args([
            "-o",
            "uid=",
            "-o",
            "ruid=",
            "-o",
            "svuid=",
            "-p",
            &pid.to_string(),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let ids: Vec<u32> = text.split_whitespace().map(id32).collect();
    assert_eq!(ids.len(), 3, "{text:?}");
    (ids[0], ids[1], ids[2])
}

/// The scratch folders, resolved: the record directory (root's, 0700, as the agent makes it)
/// and the job's working directory (the slot user's).
struct Folders {
    _scratch: TestDir,
    records: PathBuf,
    work: PathBuf,
}

impl Folders {
    fn new(name: &str, uid: u32, gid: u32) -> Self {
        let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let root = fs::canonicalize(scratch.path()).unwrap();
        let (records, work) = (root.join("records"), root.join("work"));
        fs::create_dir(&records).unwrap();
        fs::set_permissions(&records, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&work).unwrap();
        std::os::unix::fs::chown(&work, Some(uid), Some(gid)).unwrap();
        // The folders above it may not be searchable by the slot user; the job only needs its
        // working directory, which it inherits open.
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _scratch: scratch,
            records,
            work,
        }
    }

    /// The text of record `name`.
    fn record(&self, name: &str) -> String {
        fs::read_to_string(self.records.join(name)).unwrap_or_default()
    }
}

/// Starts the verb as root on `spec`, written root-only as the agent writes it, in `work`,
/// with its input and output piped.
fn start(folders: &Folders, spec: &SuperviseSpec) -> Child {
    let path = folders.records.join("spec.json");
    fs::write(&path, serde_json::to_string(spec).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    Command::new(AGENT)
        .args([VERB, "--spec"])
        .arg(&path)
        .current_dir(&folders.work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// The ready record, once the supervisor has written it whole (a wait for that event: the
/// supervisor writes it just after the job starts, and may do so after the job's first line).
/// A supervisor that ends without one fails the test with its status and its exit record.
fn ready(folders: &Folders, supervisor: &mut Child) -> Ready {
    loop {
        if let Some(ready) = parse_ready(&folders.record("ready.json")) {
            return ready;
        }
        if let Some(status) = supervisor.try_wait().unwrap() {
            return parse_ready(&folders.record("ready.json")).unwrap_or_else(|| {
                let exit = folders.record("exit.json");
                panic!("the supervisor ended ({status}) with no ready record; exit record: {exit}")
            });
        }
        std::thread::yield_now();
    }
}

/// As root, with a new audit session, the supervisor drops to the slot user and sets the
/// limits: the job runs as `nobody`, effective, real and saved, with that user's group as its
/// only group; the supervisor itself runs as `nobody` too, all three ids (it dropped for good:
/// root cannot be regained); the job and the supervisor are in one new audit session, which is
/// not this test's, and `asid_of` (the kernel's answer by pid, `auditon`) gives the id the
/// supervisor read back from its own session when it opened it (`getaudit_addr`, in the ready
/// record); the process and file-size limits are set soft and hard; and a write past the
/// file-size limit stops at exactly the limit. Catches a drop that keeps root's groups or
/// its real uid, a session that is not opened (the job would stay in the agent's), and limits
/// that are not set.
#[test]
#[ignore = "needs root on macOS: run by beta.yml's root step"]
fn as_root_the_supervisor_drops_opens_a_session_and_sets_the_limits() {
    require_root();
    let ((uid_text, uid), (gid_text, gid)) = (nobody("-u"), nobody("-g"));
    let folders = Folders::new("supervisor-root", uid, gid);
    let spec = SuperviseSpec {
        uid,
        gid,
        new_session: true,
        profile: "(version 1)\n(allow default)\n".to_owned(),
        params: vec![],
        program: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), JOB.to_owned()],
        max_file_bytes: FILE_BYTES,
        max_processes: PROCESSES,
        record_dir: folders.records.display().to_string(),
    };
    let mut supervisor = start(&folders, &spec);
    let mut out = BufReader::new(supervisor.stdout.take().unwrap());
    let mut line = || {
        let mut text = String::new();
        out.read_line(&mut text).unwrap();
        text.trim_end().to_owned()
    };
    assert_eq!(line(), format!("ids {uid_text} {gid_text} {gid_text}"));
    assert_eq!(line(), format!("processes {PROCESSES} {PROCESSES}"));
    let blocks = FILE_BYTES / 512;
    assert_eq!(line(), format!("file blocks {blocks} {blocks}"));

    let ready = ready(&folders, &mut supervisor);
    let job = u32::try_from(ready.job_pid).unwrap();
    assert_eq!(uids_of(job), (uid, uid, uid));
    assert_eq!(uids_of(supervisor.id()), (uid, uid, uid));
    let own = asid_of(i32::try_from(std::process::id()).unwrap());
    let session = asid_of(ready.job_pid);
    assert!(session.is_some(), "the job's session cannot be read");
    assert_eq!(ready.asid, session);
    assert_eq!(asid_of(i32::try_from(supervisor.id()).unwrap()), session);
    assert_ne!(session, own, "the job stayed in its starter's session");

    supervisor.stdin.take().unwrap().write_all(b"go\n").unwrap();
    let dd = line();
    assert!(dd.starts_with("dd "), "{dd}");
    assert_ne!(dd, "dd 0", "a write past the limit succeeded");
    let status = supervisor.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        fs::metadata(folders.work.join("big")).unwrap().len(),
        FILE_BYTES
    );
    let exit = parse_exit(&folders.record("exit.json")).unwrap();
    assert_eq!(exit.ended, Ended::Exited(0));
    assert_eq!(exit.job_pid, Some(ready.job_pid));
    assert_eq!(exit.asid, session);
    assert!(!folders.records.join("spec.json").exists());
}

/// As root, the job's group list is exactly the spec's group, with no supplementary group of
/// root's, even when that group is not the user's own primary group (`staff`, 20, for
/// `nobody`). Catches a drop that takes the groups from the user
/// database (`initgroups`) instead of the spec, which would hand the job groups of the slot
/// user's that the agent did not choose.
#[test]
#[ignore = "needs root on macOS: run by beta.yml's root step"]
fn as_root_the_job_holds_exactly_the_spec_s_group() {
    require_root();
    let (uid_text, uid) = nobody("-u");
    let gid = 20;
    let folders = Folders::new("supervisor-root-group", uid, gid);
    let spec = SuperviseSpec {
        uid,
        gid,
        new_session: false,
        profile: "(version 1)\n(allow default)\n".to_owned(),
        params: vec![],
        program: "/bin/sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "echo \"$(/usr/bin/id -u) $(/usr/bin/id -g) $(/usr/bin/id -G)\"".to_owned(),
        ],
        max_file_bytes: FILE_BYTES,
        max_processes: PROCESSES,
        record_dir: folders.records.display().to_string(),
    };
    let supervisor = start(&folders, &spec);
    let output = supervisor.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{uid_text} {gid} {gid}\n")
    );
    let ready = parse_ready(&folders.record("ready.json")).expect("a ready record");
    assert_eq!(ready.asid, None, "no session was asked for");
}
