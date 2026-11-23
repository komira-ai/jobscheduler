//! The supervisor's privileged calls on macOS, executed for real by the real binary as root and
//! read back from the system: the new audit session (`macos/audit.rs`), the drop to the slot
//! user and the limits (`macos/sys.rs`). No fake stands in for any of them.
//!
//! Every test needs root, so each is `#[ignore]` with that reason: the ordinary test run lists
//! them as ignored, by name, and runs none. beta.yml's macOS job runs exactly this target as
//! root (`sudo ... cargo test -p js-agent --test mac_supervisor_root -- --ignored`). A test run
//! without root fails at its first line, saying so: it never passes without having run.
//!
//! The slot user is `nobody`, which every Mac has; the job is `/bin/sh` or js-testkit's
//! `kbf-act` and reports what the system says of it. Waits are for events (a line the job
//! prints, its end), never for a time.
//!
//! The job's groups are read from its own kernel credential, never from the directory: the
//! `groups` step of `kbf-act` prints the list `getgroups(2)` returns inside the job, and a
//! `try-read` of a file a group alone may read shows whether the kernel lets the job act as a
//! member. `id -G` is not used for this: on macOS it prints what the directory lists for the
//! user (`getgrouplist`), which a process that `setgroups` set does not hold; for `nobody`
//! that is `everyone` (12), `localaccounts` (61), `_lpoperator` (100) and, on a hosted
//! runner, 701, and not the group the spec gave.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use js_agent::mac_supervisor::macos::audit::asid_of;
use js_agent::mac_supervisor::record::{parse_exit, parse_ready};
use js_agent::mac_supervisor::{Ended, Ready, SuperviseSpec, VERB};
use js_testkit::dir::TestDir;

// This test binary answers as js-testkit's `kbf-act` when it is run through a copy of that
// name: the job that reads its own groups back.
js_testkit::act_test_hook!();

/// The binary under test.
const AGENT: &str = env!("CARGO_BIN_EXE_js-agent");

/// The mode of a file that its group alone may read: no bit for its owner (root, who reads
/// anything anyway) or for others.
const GROUP_READ_ONLY: u32 = 0o040;

/// `everyone`, a group the directory lists for every user, and the job must still not hold.
const EVERYONE: u32 = 12;

/// A group the directory lists for `nobody` on a hosted runner, which a job must not hold.
const RUNNER_GROUP: u32 = 701;

/// The largest file the job may write.
const FILE_BYTES: u64 = 64 * 1024;

/// The most processes the slot user may have.
const PROCESSES: u32 = 300;

/// The unit of `ulimit -f` in macOS's `/bin/sh`, which is bash 3.2: 1024 bytes, in POSIX mode
/// too (512-byte units in POSIX mode came with a later bash).
const SH_BLOCK: u64 = 1024;

/// What the job does: prints its ids (its group list from the kernel, through `act`), its
/// process limits and its file-size limits in [`SH_BLOCK`]s, waits for a line on its input (the
/// test looks at its audit session meanwhile), then writes twice the file limit and prints how
/// `dd` ended.
fn job(act: &str) -> String {
    format!(
        r#"echo "ids $(/usr/bin/id -u) $(/usr/bin/id -g) $('{act}' groups)"
echo "processes $(ulimit -S -u) $(ulimit -H -u)"
echo "file blocks $(ulimit -S -f) $(ulimit -H -f)"
read line
/bin/dd if=/dev/zero of=./big bs=1024 count=128 2>/dev/null
echo "dd $?"
"#
    )
}

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

/// The groups the directory lists for `nobody` (`id -G nobody`): what a credential that
/// resolved the user's groups from the directory would hold.
fn nobody_s_directory_groups() -> BTreeSet<u32> {
    let out = Command::new("/usr/bin/id")
        .args(["-G", "nobody"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .map(id32)
        .collect()
}

/// What every slot user may reach: a root-owned folder, mode 0755, outside the runner's
/// home (which `nobody` may not search), holding a copy of this test binary named
/// `kbf-act` and, for each group to check, a file `group-<gid>` owned by root and that group,
/// [`GROUP_READ_ONLY`].
struct Open {
    dir: TestDir,
    act: String,
    groups: Vec<u32>,
}

impl Open {
    fn new(name: &str, groups: impl IntoIterator<Item = u32>) -> Self {
        // Not below CARGO_TARGET_TMPDIR: that is in the runner's home, which `nobody` may not
        // search, and the job must reach the copy of this binary and the group files.
        let dir = TestDir::under("/private/tmp", name);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let act = dir.join(js_testkit::act::NAME);
        fs::copy(std::env::current_exe().unwrap(), &act).unwrap();
        fs::set_permissions(&act, fs::Permissions::from_mode(0o755)).unwrap();
        let groups: Vec<u32> = groups
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let open = Self {
            act: act.display().to_string(),
            dir,
            groups,
        };
        for gid in &open.groups {
            let file = open.file(*gid);
            fs::write(&file, format!("only group {gid} may read this\n")).unwrap();
            std::os::unix::fs::chown(&file, Some(0), Some(*gid)).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(GROUP_READ_ONLY)).unwrap();
        }
        open
    }

    fn file(&self, gid: u32) -> String {
        self.dir.join(format!("group-{gid}")).display().to_string()
    }

    /// The `kbf-act` steps that print the job's group list and then try to read each group's
    /// file, in [`Self::groups`]' order.
    fn steps(&self) -> Vec<String> {
        let mut steps = vec!["groups".to_owned()];
        for gid in &self.groups {
            steps.extend([";".to_owned(), "try-read".to_owned(), self.file(*gid)]);
        }
        steps
    }

    /// What the job's output of [`Self::steps`] says: the groups its credential holds, and
    /// the groups whose file it read. A read refused for any reason but the permission bits
    /// fails the test, as does a line missing or out of place.
    fn held(&self, stdout: &str) -> Held {
        let mut lines = stdout.lines();
        let listed = lines
            .next()
            .unwrap_or_else(|| panic!("no group list: {stdout:?}"));
        let groups = listed.split_whitespace().map(id32).collect();
        let mut readable = BTreeSet::new();
        for gid in &self.groups {
            match lines.next() {
                Some("PROBE try-read ok") => {
                    readable.insert(*gid);
                }
                Some(line) if line == format!("PROBE try-read errno={}", libc::EACCES) => {}
                other => panic!("group {gid}'s file: {other:?} in {stdout:?}"),
            }
        }
        assert_eq!(lines.next(), None, "{stdout:?}");
        Held { groups, readable }
    }
}

/// What a job holds, by both measures.
#[derive(Debug, PartialEq, Eq)]
struct Held {
    /// Its kernel group list (`getgroups(2)` inside the job).
    groups: BTreeSet<u32>,
    /// The groups whose group-only file it could read.
    readable: BTreeSet<u32>,
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

/// As root, with a new audit session, the supervisor drops to the slot user and sets the limits:
/// the job runs as `nobody`, effective, real and saved, with that user's group as its only group
/// (its kernel list, read inside the job); the supervisor itself runs as `nobody` too, all three
/// ids (it dropped for good: root cannot be regained); the job and the supervisor are in one new
/// audit session, which is not this test's, and `asid_of` (the kernel's answer by pid, `auditon`)
/// gives the id the supervisor read back from its own session when it opened it (`getaudit_addr`,
/// in the ready record); the process and file-size limits are set soft and hard; and a write past
/// the file-size limit stops at exactly the limit. Catches a drop that keeps root's groups or its
/// real uid, a session that is not opened (the job would stay in the agent's), and limits that are
/// not set.
#[test]
#[ignore = "needs root on macOS: run by beta.yml's root step"]
fn as_root_the_supervisor_drops_opens_a_session_and_sets_the_limits() {
    require_root();
    let ((uid_text, uid), (gid_text, gid)) = (nobody("-u"), nobody("-g"));
    let folders = Folders::new("supervisor-root", uid, gid);
    let open = Open::new("supervisor-root-act", []);
    let spec = SuperviseSpec {
        uid,
        gid,
        new_session: true,
        profile: "(version 1)\n(allow default)\n".to_owned(),
        params: vec![],
        program: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), job(&open.act)],
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
    assert_eq!(line(), format!("ids {uid_text} {gid_text} {gid}"));
    assert_eq!(line(), format!("processes {PROCESSES} {PROCESSES}"));
    let blocks = FILE_BYTES / SH_BLOCK;
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

/// As root, the job holds exactly the spec's group, even when that group is not the user's
/// own primary group (`staff`, 20, for `nobody`): the kernel list read inside the job is that
/// group alone, and of files that one group alone may read, the job reads the spec's group's
/// and none of `everyone` (12), 701 or any other group the directory lists
/// for `nobody`. Catches a drop that takes the groups from the user database
/// (`initgroups`) instead of the spec, or keeps a supplementary group of root's, which would
/// hand the job groups the agent did not choose; and a credential whose list is right but that
/// the kernel still lets act as a member of the directory's groups.
#[test]
#[ignore = "needs root on macOS: run by beta.yml's root step"]
fn as_root_the_job_holds_exactly_the_spec_s_group() {
    require_root();
    let (_, uid) = nobody("-u");
    let gid = 20;
    let folders = Folders::new("supervisor-root-group", uid, gid);
    let mut groups = nobody_s_directory_groups();
    groups.extend([gid, EVERYONE, RUNNER_GROUP]);
    let open = Open::new("supervisor-root-group-act", groups);
    let spec = SuperviseSpec {
        uid,
        gid,
        new_session: false,
        profile: "(version 1)\n(allow default)\n".to_owned(),
        params: vec![],
        program: open.act.clone(),
        args: open.steps(),
        max_file_bytes: FILE_BYTES,
        max_processes: PROCESSES,
        record_dir: folders.records.display().to_string(),
    };
    let supervisor = start(&folders, &spec);
    let output = supervisor.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        open.held(&String::from_utf8_lossy(&output.stdout)),
        Held {
            groups: BTreeSet::from([gid]),
            readable: BTreeSet::from([gid]),
        }
    );
    let ready = parse_ready(&folders.record("ready.json")).expect("a ready record");
    assert_eq!(ready.asid, None, "no session was asked for");
}

/// As root, a job started as `nobody` by `sudo`, which gives it the groups the directory lists
/// for the user, is caught by both measures of the test above: its kernel list holds
/// `everyone` (12) and every other group the directory lists for `nobody`, and it reads each
/// of those groups' files and no other. Catches a measure that cannot see a group the job
/// does hold (the test above would pass whatever the drop did).
#[test]
#[ignore = "needs root on macOS: run by beta.yml's root step"]
fn as_root_a_job_given_the_directory_s_groups_is_caught_by_both_measures() {
    require_root();
    let directory = nobody_s_directory_groups();
    assert!(directory.contains(&EVERYONE), "{directory:?}");
    let mut groups = directory.clone();
    groups.extend([EVERYONE, RUNNER_GROUP, 20]);
    let open = Open::new("supervisor-root-control-act", groups);
    let output = Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "nobody", &open.act])
        .args(open.steps())
        .current_dir(open.dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let held = open.held(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(held.groups, directory);
    assert_eq!(held.readable, directory);
}
