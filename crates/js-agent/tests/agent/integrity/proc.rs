//! The Linux reader over recorded `/proc` text and recorded listings, and the machine's own
//! directory lister over a real directory in the target directory.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use js_agent::FileReader;
use js_agent::integrity::{
    DirLister, Entry, HostView, Listing, Mount, ProcHost, Process, SystemDirs, Watched,
};

use crate::support::Scratch;

/// Recorded files and listings. `/proc` is listed from `listings` in turn (the last one
/// again and again once they are used up), so that a process can end between two listings.
#[derive(Default)]
struct Recorded {
    files: BTreeMap<String, String>,
    listings: Vec<Result<Vec<&'static str>, &'static str>>,
    dirs: BTreeMap<PathBuf, Vec<Entry>>,
    listed: Cell<usize>,
}

#[derive(Clone)]
struct Host(Rc<Recorded>);

impl FileReader for Host {
    fn read(&self, path: &str) -> Result<String, String> {
        (self.0.files.get(path))
            .cloned()
            .ok_or_else(|| format!("{path}: No such file or directory"))
    }
}

impl DirLister for Host {
    fn list(&self, dir: &Path, cap: usize) -> Result<Listing, String> {
        let entries = match dir == Path::new("/proc") {
            true => {
                let at = self.0.listed.get();
                self.0.listed.set(at + 1);
                let listing = &self.0.listings[at.min(self.0.listings.len() - 1)];
                listing
                    .clone()
                    .map(|names| names.into_iter().map(|name| entry(name, 0, 0, 0)).collect())
                    .map_err(str::to_owned)
            }
            false => (self.0.dirs.get(dir))
                .cloned()
                .ok_or_else(|| format!("{}: No such file or directory", dir.display())),
        }?;
        let more = entries.len() > cap;
        Ok(Listing {
            entries: entries.into_iter().take(cap).collect(),
            more,
        })
    }
}

fn entry(name: &str, size: u64, mtime: i64, mode: u32) -> Entry {
    Entry {
        name: name.to_owned(),
        size,
        mtime: (mtime, 0),
        ctime: (mtime, 0),
        ino: 7,
        uid: 1000,
        gid: 1000,
        mode,
    }
}

/// The `stat` of a process as the kernel prints it (52 fields; only the name, the parent
/// and the start time are read).
fn stat(pid: u32, comm: &str, ppid: u32, start: u64) -> String {
    format!(
        "{pid} ({comm}) S {ppid} 1 1 0 -1 4194560 100 0 0 0 10 20 0 0 20 0 1 0 {start} \
         123456789 1000 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0\n"
    )
}

fn status(pid: u32, uid: u32) -> String {
    format!(
        "Name:\tx\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t{pid}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t0\t0\t0\t0\n"
    )
}

/// A host with the processes `(pid, comm, ppid, uid, start, cgroup)` recorded.
fn with_processes(procs: &[(u32, &str, u32, u32, u64, &str)]) -> Recorded {
    let mut recorded = Recorded::default();
    for (pid, comm, ppid, uid, start, cgroup) in procs {
        recorded
            .files
            .insert(format!("/proc/{pid}/stat"), stat(*pid, comm, *ppid, *start));
        recorded
            .files
            .insert(format!("/proc/{pid}/status"), status(*pid, *uid));
        recorded
            .files
            .insert(format!("/proc/{pid}/cgroup"), format!("0::{cgroup}\n"));
    }
    recorded
}

fn host(recorded: Recorded) -> ProcHost {
    let shared = Host(Rc::new(recorded));
    ProcHost::new(Box::new(shared.clone()), Box::new(shared))
}

/// Asserts that the process table is read from recorded `/proc` text: entries that are not
/// numbers are not processes, a name with spaces and parentheses does not shift the fields,
/// the user is the real one, and the control groups of several lines are one text. It
/// catches a parser that miscounts the fields of `stat`.
#[test]
fn the_process_table_is_read_from_recorded_proc_text() {
    let mut recorded = with_processes(&[
        (1, "systemd", 0, 0, 1, "/init.scope"),
        (
            100,
            "(sd-pam) (x y)",
            1,
            1000,
            5000,
            "/user.slice/user-1000.slice",
        ),
    ]);
    recorded.files.insert(
        "/proc/100/cgroup".to_owned(),
        "1:name=systemd:/user.slice\n0::/user.slice/user-1000.slice\n".to_owned(),
    );
    recorded.listings = vec![Ok(vec![
        "1",
        "self",
        "100",
        "sys",
        "thread-self",
        "cpuinfo",
    ])];
    let processes = host(recorded).processes().unwrap();
    assert_eq!(
        processes,
        [
            Process {
                pid: 1,
                ppid: 0,
                uid: 0,
                start: 1,
                cgroup: "0::/init.scope".to_owned()
            },
            Process {
                pid: 100,
                ppid: 1,
                uid: 1000,
                start: 5000,
                cgroup: "1:name=systemd:/user.slice 0::/user.slice/user-1000.slice".to_owned()
            }
        ]
    );
}

/// Asserts that a process that ends while the table is read is left out when `/proc` no
/// longer lists it, and is an error when it is still listed (it is there and cannot be
/// read), when it is listed but its files make no sense, and when `/proc` cannot be listed
/// again to tell. It catches a table that skips what it cannot read, and one that fails on
/// the ordinary end of a process.
#[test]
fn a_process_that_ended_is_left_out_and_one_that_cannot_be_read_is_an_error() {
    let mut gone = with_processes(&[(1, "init", 0, 0, 1, "/")]);
    gone.listings = vec![Ok(vec!["1", "7"]), Ok(vec!["1"])];
    let processes = host(gone).processes().unwrap();
    assert_eq!(processes.iter().map(|p| p.pid).collect::<Vec<_>>(), [1]);

    let mut there = with_processes(&[(1, "init", 0, 0, 1, "/")]);
    there.listings = vec![Ok(vec!["1", "7"])];
    let why = host(there).processes().unwrap_err();
    assert_eq!(why, "/proc/7/stat: No such file or directory");

    let mut cannot_tell = with_processes(&[(1, "init", 0, 0, 1, "/")]);
    cannot_tell.listings = vec![Ok(vec!["1", "7"]), Err("/proc: Permission denied")];
    assert_eq!(
        host(cannot_tell).processes().unwrap_err(),
        "/proc: Permission denied"
    );

    let none = Recorded {
        listings: vec![Err("/proc: Permission denied")],
        ..Recorded::default()
    };
    assert_eq!(
        host(none).processes().unwrap_err(),
        "/proc: Permission denied"
    );

    let whole = stat(1, "x", 0, 1);
    let broken: [(&str, &str); 4] = [
        ("garbage", "Uid:\t1\t1\t1\t1\n"),
        ("1 (x) S 0 1 1 0 -1", "Uid:\t1\t1\t1\t1\n"),
        (whole.as_str(), "Name:\tx\n"),
        (whole.as_str(), "Uid:\tx\n"),
    ];
    for (stat_text, status_text) in broken {
        let mut one = with_processes(&[(1, "x", 0, 0, 1, "/")]);
        one.files
            .insert("/proc/1/stat".to_owned(), stat_text.to_owned());
        one.files
            .insert("/proc/1/status".to_owned(), status_text.to_owned());
        one.listings = vec![Ok(vec!["1"])];
        let why = host(one).processes().unwrap_err();
        assert_eq!(why, "/proc/1: cannot read the process entry", "{stat_text}");
    }

    let mut ended = with_processes(&[(1, "x", 0, 0, 1, "/")]);
    ended
        .files
        .insert("/proc/1/stat".to_owned(), "garbage".to_owned());
    ended.listings = vec![Ok(vec!["1"]), Ok(vec![])];
    assert!(host(ended).processes().unwrap().is_empty());
}

/// Asserts that more processes than the cap are an error and not a table cut short. It
/// catches a bound that makes a host with many processes pass unchecked.
#[test]
fn more_processes_than_the_cap_are_an_error() {
    let mut recorded = with_processes(&[(1, "a", 0, 0, 1, "/"), (2, "b", 1, 0, 2, "/")]);
    recorded.listings = vec![Ok(vec!["1", "2"])];
    let shared = Host(Rc::new(recorded));
    let capped = ProcHost::new(Box::new(shared.clone()), Box::new(shared)).with_caps(1, 1);
    assert_eq!(
        capped.processes().unwrap_err(),
        "/proc: more than 1 processes"
    );
}

/// Asserts that the mounts are read from a recorded `/proc/self/mountinfo` with and without
/// optional fields and with an escaped space, that the kernel's own numbers are left out,
/// and that a line that is not a mount is an error. It catches a parser that depends on the
/// optional fields, and one that skips a line it cannot read.
#[test]
fn mounts_are_read_from_recorded_mountinfo() {
    let text = "\
22 28 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
28 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw,errors=remount-ro

35 28 0:30 / /mnt/with\\040space rw,relatime - tmpfs tmpfs rw,size=1k
40 28 0:31 /sub /bind rw master:2 shared:3 - ext4 /dev/sdb1 rw
";
    let mut recorded = Recorded::default();
    recorded
        .files
        .insert("/proc/self/mountinfo".to_owned(), text.to_owned());
    let mounts = host(recorded).mounts().unwrap();
    let mount = |root: &str, point: &str, fstype: &str, source: &str| Mount {
        root: root.to_owned(),
        point: point.to_owned(),
        fstype: fstype.to_owned(),
        source: source.to_owned(),
    };
    assert_eq!(
        mounts,
        [
            mount("/", "/sys", "sysfs", "sysfs"),
            mount("/", "/", "ext4", "/dev/sda1"),
            mount("/", "/mnt/with\\040space", "tmpfs", "tmpfs"),
            mount("/sub", "/bind", "ext4", "/dev/sdb1"),
        ]
    );
    for bad in ["not a mount line", "1 2 - ext4", "1 2 0:1 / /x rw - ext4"] {
        let mut recorded = Recorded::default();
        recorded
            .files
            .insert("/proc/self/mountinfo".to_owned(), format!("{bad}\n"));
        let why = host(recorded).mounts().unwrap_err();
        assert!(
            why.starts_with("/proc/self/mountinfo: cannot read the line"),
            "{why}"
        );
    }
    assert_eq!(
        host(Recorded::default()).mounts().unwrap_err(),
        "/proc/self/mountinfo: No such file or directory"
    );
}

/// Asserts that the user and group databases are read as sets of trimmed lines without
/// comments and blank lines, that a line that is no entry is an error naming the file, and
/// that a file that is missing is an error. It catches a database read that tolerates
/// garbage.
#[test]
fn the_user_and_group_databases_are_read_from_recorded_text() {
    let mut recorded = Recorded::default();
    recorded.files.insert(
        "/etc/passwd".to_owned(),
        "# comment\n\nroot:x:0:0:root:/root:/bin/bash\n  daemon:x:1:1::/usr/sbin:/nologin  \n"
            .to_owned(),
    );
    recorded
        .files
        .insert("/etc/group".to_owned(), "root:x:0:\nbad line\n".to_owned());
    let view = host(recorded);
    let users: Vec<String> = view.users().unwrap().into_iter().collect();
    assert_eq!(
        users,
        [
            "daemon:x:1:1::/usr/sbin:/nologin",
            "root:x:0:0:root:/root:/bin/bash"
        ]
    );
    assert_eq!(
        view.groups().unwrap_err(),
        "/etc/group: a line is not an entry"
    );
    assert_eq!(
        host(Recorded::default()).users().unwrap_err(),
        "/etc/passwd: No such file or directory"
    );
}

/// Asserts that the modules are the first word of each line of a recorded `/proc/modules`,
/// that a kernel with no modules is an empty set and not an error, and that a missing file
/// is an error. It catches a modules list that skips what it cannot read.
#[test]
fn modules_are_read_from_recorded_text() {
    let mut recorded = Recorded::default();
    recorded.files.insert(
        "/proc/modules".to_owned(),
        "ext4 1048576 1 - Live 0x0000000000000000\nnf_tables 2 3 a,b, Live 0x1\n\n".to_owned(),
    );
    let modules: Vec<String> = host(recorded).modules().unwrap().into_iter().collect();
    assert_eq!(modules, ["ext4", "nf_tables"]);
    let mut empty = Recorded::default();
    empty
        .files
        .insert("/proc/modules".to_owned(), String::new());
    assert!(host(empty).modules().unwrap().is_empty());
    assert_eq!(
        host(Recorded::default()).modules().unwrap_err(),
        "/proc/modules: No such file or directory"
    );
}

/// The digest of the recorded directory `/w` holding `entries`.
fn digest_of(entries: Vec<Entry>) -> Watched {
    let mut recorded = Recorded::default();
    recorded.dirs.insert(PathBuf::from("/w"), entries);
    host(recorded).watched(Path::new("/w")).unwrap()
}

/// Asserts that the digest of a watched directory does not depend on the order the
/// entries are listed in, changes with each of the name, size, modification time and mode
/// of an entry, and with an entry added; that it is a SHA-256 in hex; that too many entries
/// are no digest; and that a directory that cannot be listed is an error. It catches a
/// digest that leaves out a field a job can change.
#[test]
fn the_digest_of_a_directory_follows_every_field() {
    let base = vec![entry("a", 1, 10, 0o100644), entry("b", 2, 20, 0o100600)];
    let reference = digest_of(base.clone());
    let Watched::Digest(text) = &reference else {
        panic!("{reference:?}");
    };
    assert_eq!(text.len(), 64);
    assert!(text.chars().all(|c| c.is_ascii_hexdigit()));
    let reordered = vec![base[1].clone(), base[0].clone()];
    assert_eq!(digest_of(reordered), reference);
    let mut nanos = base.clone();
    nanos[0].mtime = (10, 1);
    let with = |change: &dyn Fn(&mut Entry)| {
        let mut entries = base.clone();
        change(&mut entries[0]);
        entries
    };
    let changed: Vec<Vec<Entry>> = vec![
        with(&|e| e.ctime.0 += 1),
        with(&|e| e.ctime.1 += 1),
        with(&|e| e.ino += 1),
        with(&|e| e.uid += 1),
        with(&|e| e.gid += 1),
        vec![entry("c", 1, 10, 0o100644), base[1].clone()],
        vec![entry("a", 3, 10, 0o100644), base[1].clone()],
        vec![entry("a", 1, 11, 0o100644), base[1].clone()],
        vec![entry("a", 1, 10, 0o100755), base[1].clone()],
        nanos,
        vec![base[0].clone()],
        [base.clone(), vec![entry("c", 0, 0, 0)]].concat(),
    ];
    for entries in changed {
        assert_ne!(digest_of(entries.clone()), reference, "{entries:?}");
    }

    let mut recorded = Recorded::default();
    recorded.dirs.insert(
        PathBuf::from("/w"),
        vec![
            entry("a", 1, 1, 1),
            entry("b", 1, 1, 1),
            entry("c", 1, 1, 1),
        ],
    );
    let shared = Host(Rc::new(recorded));
    let small = ProcHost::new(Box::new(shared.clone()), Box::new(shared)).with_caps(8, 2);
    assert_eq!(small.watched(Path::new("/w")), Ok(Watched::TooMany));
    assert_eq!(
        small.watched(Path::new("/missing")),
        Err("/missing: No such file or directory".to_owned())
    );
}

/// Asserts that the machine's host sees a file replaced by another of the same size, the
/// same modification time and the same mode, which is what a job that restores the time
/// leaves behind. It catches a digest of the fields a job can set and nothing else.
#[test]
fn a_replaced_file_with_the_same_stamp_changes_the_digest() {
    let dir = Scratch::new("integrity-replaced");
    let stamp = UNIX_EPOCH + Duration::from_secs(1_000_000);
    let write = |name: &str, text: &str| {
        let path = dir.path(name);
        fs::write(&path, text).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        path
    };
    let target = write("target", "aa");
    let host = ProcHost::system();
    let before = host.watched(&dir.path("")).unwrap();
    let other = write("other", "bb");
    fs::rename(other, target).unwrap();
    assert_ne!(host.watched(&dir.path("")).unwrap(), before);
}

/// Asserts that the machine's directory lister reports names, sizes, modification times
/// and modes of a real directory without following a link, stops at the cap and says there
/// is more only when there is, and names the directory when it cannot list it. It catches
/// a lister that follows a link out of the directory, and an off-by-one at the cap.
#[test]
fn the_system_lister_reads_a_real_directory() {
    let dir = Scratch::new("integrity-lister");
    fs::write(dir.path("one"), "1").unwrap();
    fs::write(dir.path("three"), "333").unwrap();
    let stamped = fs::File::options()
        .write(true)
        .open(dir.path("one"))
        .unwrap();
    stamped
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    fs::create_dir(dir.path("sub")).unwrap();
    symlink("/definitely/not/there", dir.path("link")).unwrap();

    let listing = SystemDirs.list(&dir.path(""), 10).unwrap();
    assert!(!listing.more);
    let mut entries = listing.entries;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["link", "one", "sub", "three"]);
    let by = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
    assert_eq!((by("one").size, by("three").size), (1, 3));
    assert_eq!(by("one").mtime.0, 1_000_000);
    let meta = fs::metadata(dir.path("one")).unwrap();
    assert_eq!(by("one").ctime, (meta.ctime(), meta.ctime_nsec()));
    assert_eq!(
        (by("one").ino, by("one").uid, by("one").gid),
        (meta.ino(), meta.uid(), meta.gid())
    );
    assert_eq!(by("sub").mode & 0o170_000, 0o040_000, "a directory");
    assert_eq!(by("link").mode & 0o170_000, 0o120_000, "the link itself");

    let exact = SystemDirs.list(&dir.path(""), 4).unwrap();
    assert!((exact.entries.len(), exact.more) == (4, false));
    let cut = SystemDirs.list(&dir.path(""), 3).unwrap();
    assert!((cut.entries.len(), cut.more) == (3, true));
    let empty = SystemDirs.list(&dir.path(""), 0).unwrap();
    assert!((empty.entries.len(), empty.more) == (0, true));

    let missing = dir.path("missing");
    let why = SystemDirs.list(&missing, 4).unwrap_err();
    assert!(why.starts_with(&format!("{}:", missing.display())), "{why}");
    let why = SystemDirs.list(&dir.path("one"), 4).unwrap_err();
    assert!(why.contains("one"), "{why}");
}

/// Asserts that the machine's own reader is wired to the machine's own directories: a watched
/// real directory gets a digest that follows its content. It catches a constructor that
/// reads nothing.
#[test]
fn the_system_host_digests_a_real_directory() {
    let dir = Scratch::new("integrity-system");
    let system = ProcHost::system();
    let before = system.watched(&dir.path("")).unwrap();
    fs::write(dir.path("new"), "x").unwrap();
    let after = system.watched(&dir.path("")).unwrap();
    assert_ne!(before, after);
}
