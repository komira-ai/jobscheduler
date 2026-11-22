//! The recovery of a refused start, on a directory that imitates the cgroup tree (the cgroup
//! files are plain files there) and on a host of fakes for the failures.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use js_testkit::dir::TestDir;

use super::{
    Host, PAUSES, VERB, dispatch, list_dirs, read_file, result_word, run, systemctl_version,
    write_file,
};

/// The unit's cgroup below the mount, and the leaf `ExecStopPost=` runs in.
const UNIT: &str = "system.slice/js-agent-crun.service";

fn args(result: &str) -> Vec<OsString> {
    ["js-agent", VERB, "--result", result]
        .map(OsString::from)
        .to_vec()
}

fn same(code: ExitCode, status: u8) -> bool {
    format!("{code:?}") == format!("{:?}", ExitCode::from(status))
}

/// A cgroup tree with two runs left: a supervisor leaf and `actions` with two leases.
struct Tree {
    _dir: TestDir,
    mount: PathBuf,
    own: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let mount = dir.path().join("cgroup");
        let own = dir.path().join("self_cgroup");
        fs::write(&own, format!("0::/{UNIT}/.control\n")).unwrap();
        let tree = Self {
            _dir: dir,
            mount,
            own,
        };
        tree.set("cgroup.subtree_control", "cpu memory pids\n");
        tree.set("supervisor/cgroup.events", "populated 1\nfrozen 0\n");
        tree.set("actions/cgroup.events", "populated 1\n");
        tree.set("actions/lease-a/cgroup.events", "populated 1\n");
        tree.set("actions/lease-b/cgroup.events", "populated 1\n");
        tree.set("actions/lease-c/cgroup.events", "populated 0\n");
        tree
    }

    fn unit(&self) -> PathBuf {
        self.mount.join(UNIT)
    }

    /// Writes `text` to `name` below the unit's cgroup.
    fn set(&self, name: &str, text: &str) {
        let path = self.unit().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn get(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.unit().join(name)).ok()
    }

    /// Imitates the kernel: a cgroup that was killed is empty one pause later. `seen` collects
    /// which kill files exist at each pause.
    fn kernel(&self, seen: &RefCell<Vec<Vec<&'static str>>>) {
        let mut killed = Vec::new();
        for name in ["supervisor", "actions"] {
            if self.unit().join(name).join("cgroup.kill").exists() {
                killed.push(name);
                self.set(&format!("{name}/cgroup.events"), "populated 0\n");
            }
        }
        seen.borrow_mut().push(killed);
    }
}

/// Runs the verb on `tree` with real file adapters, a systemd version printed as `systemctl`
/// and the kernel's imitation as the pause; returns (status, stdout, stderr, kills seen at each
/// pause).
fn run_on(
    tree: &Tree,
    systemctl: Result<&str, &str>,
    result: &str,
) -> (ExitCode, String, String, Vec<Vec<&'static str>>) {
    let seen = RefCell::new(Vec::new());
    let printed = systemctl.map(str::to_owned).map_err(str::to_owned);
    let host = Host {
        read: &read_file,
        write: &write_file,
        list: &list_dirs,
        version: &|_| printed.clone(),
        pause: &|_| tree.kernel(&seen),
        mount: &tree.mount,
        self_cgroup: &tree.own,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(result), &mut out, &mut err, &host);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
        seen.into_inner(),
    )
}

const NEW: Result<&str, &str> = Ok("systemd 259 (259.5-0ubuntu3.4)\n+PAM\n");

/// The runs left are ended, the supervisor's cgroup before the lease cgroups, and the one line
/// names the version, the leases that were populated and that the next start places the agent.
#[test]
fn the_runs_left_are_ended_supervisors_first() {
    let tree = Tree::new("start-refused-ends");
    let (code, out, err, seen) = run_on(&tree, NEW, "resources");
    assert!(same(code, 0), "{err}");
    assert_eq!(err, "");
    assert_eq!(
        out,
        format!(
            "crun-start-refused: systemd 259 refused the start: ended the runs left in {}: \
             lease-a, lease-b; the next start places the agent\n",
            tree.unit().join("actions").display()
        )
    );
    assert_eq!(tree.get("supervisor/cgroup.kill").as_deref(), Some("1"));
    assert_eq!(tree.get("actions/cgroup.kill").as_deref(), Some("1"));
    // The first pause is the supervisors' wait: only their kill had been written then.
    assert_eq!(
        seen,
        vec![vec!["supervisor"], vec!["supervisor", "actions"]]
    );
    assert_eq!(tree.get("actions/lease-c/cgroup.kill"), None);
}

/// A supervisor cgroup that holds nothing is not written to; the leases still are ended.
#[test]
fn an_empty_supervisor_cgroup_is_left_alone() {
    let tree = Tree::new("start-refused-no-supervisor");
    tree.set("supervisor/cgroup.events", "populated 0\n");
    let (code, out, _, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 0));
    assert!(out.contains("lease-a, lease-b"), "{out}");
    assert_eq!(tree.get("supervisor/cgroup.kill"), None);
    assert_eq!(tree.get("actions/cgroup.kill").as_deref(), Some("1"));
}

/// A supervisor cgroup that is not there holds nothing: it gets no kill write, and the leases
/// are still ended. Catches a missing `supervisor` read as an error that ends nothing.
#[test]
fn a_supervisor_cgroup_that_is_not_there_is_left_alone() {
    let tree = Tree::new("start-refused-supervisor-gone");
    fs::remove_dir_all(tree.unit().join("supervisor")).unwrap();
    let (code, out, err, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 0), "{err}");
    assert!(out.contains("lease-a, lease-b"), "{out}");
    assert!(!tree.unit().join("supervisor").exists());
    assert_eq!(tree.get("actions/cgroup.kill").as_deref(), Some("1"));
}

/// A result other than `resources` is no refused start: nothing is read, nothing is said.
#[test]
fn another_result_does_nothing_and_says_nothing() {
    let tree = Tree::new("start-refused-other");
    for result in ["success", "exit-code", "timeout", ""] {
        let (code, out, err, seen) = run_on(&tree, NEW, result);
        assert!(same(code, 0), "{result}");
        assert_eq!((out.as_str(), err.as_str()), ("", ""), "{result}");
        assert!(seen.is_empty());
    }
    assert_eq!(tree.get("supervisor/cgroup.kill"), None);
}

/// A missing `--result` is a usage error, status 2.
#[test]
fn a_missing_result_is_a_usage_error() {
    let host = Host::system();
    let mut err = Vec::new();
    let argv = ["js-agent", VERB].map(OsString::from);
    let code = run(&argv, &mut Vec::new(), &mut err, &host);
    assert!(same(code, 2));
    assert!(String::from_utf8(err).unwrap().contains("--result"));
}

/// The word of a command line is its `--result`; a flag the verb does not take is refused.
#[test]
fn the_result_word_is_read_from_the_command_line() {
    assert_eq!(result_word(&args("resources")), Ok("resources".to_owned()));
    let mut extra = args("success");
    extra.push(OsString::from("--other"));
    assert!(result_word(&extra).unwrap_err().contains("--other"));
}

/// On systemd 257 or older, or when its version cannot be read, nothing is ended and the line
/// says why; the exit status is 0.
#[test]
fn nothing_is_ended_on_an_older_or_unreadable_systemd() {
    let tree = Tree::new("start-refused-version");
    let cases = [
        (
            Ok("systemd 257 (257.9)\n"),
            "systemd 257 starts the agent in its subgroup, so the runs left are not why the \
             start was refused: nothing is ended",
        ),
        (
            Ok("launchd 1\n"),
            "the systemd version cannot be read (`systemctl --version` printed no version): \
             nothing is ended",
        ),
        (
            Err("no such file"),
            "the systemd version cannot be read (`systemctl --version`: no such file): nothing \
             is ended",
        ),
    ];
    for (systemctl, said) in cases {
        let (code, out, err, _) = run_on(&tree, systemctl, "resources");
        assert!(same(code, 0), "{err}");
        assert_eq!(out, format!("crun-start-refused: {said}\n"));
    }
    assert_eq!(tree.get("supervisor/cgroup.kill"), None);
    assert_eq!(tree.get("actions/cgroup.kill"), None);
}

/// A unit cgroup with no controller enabled is held by nothing; one with no populated lease, or
/// no `actions` at all, has no run to end.
#[test]
fn a_cgroup_nothing_holds_is_left_alone() {
    let tree = Tree::new("start-refused-free");
    tree.set("cgroup.subtree_control", "\n");
    let (code, out, _, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 0));
    assert_eq!(
        out,
        format!(
            "crun-start-refused: no controller is enabled in {}: nothing holds the cgroup\n",
            tree.unit().display()
        )
    );
    tree.set("cgroup.subtree_control", "cpu\n");
    tree.set("actions/lease-a/cgroup.events", "populated 0\n");
    tree.set("actions/lease-b/cgroup.events", "populated 0\n");
    let (code, out, _, _) = run_on(&tree, NEW, "resources");
    let none = format!(
        "crun-start-refused: no run is left in {}\n",
        tree.unit().join("actions").display()
    );
    assert!(same(code, 0));
    assert_eq!(out, none);
    fs::remove_dir_all(tree.unit().join("actions")).unwrap();
    let (code, out, _, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 0));
    assert_eq!(out, none);
    assert_eq!(tree.get("supervisor/cgroup.kill"), None);
}

/// A cgroup that stays populated after the bound of pauses is reported with exit status 1,
/// naming it and the runs that were left.
#[test]
fn a_cgroup_that_stays_populated_is_reported() {
    let tree = Tree::new("start-refused-stuck");
    let pauses = RefCell::new(0_u32);
    let host = Host {
        read: &read_file,
        write: &write_file,
        list: &list_dirs,
        version: &|_| Ok("systemd 258\n".to_owned()),
        pause: &|_| *pauses.borrow_mut() += 1,
        mount: &tree.mount,
        self_cgroup: &tree.own,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args("resources"), &mut out, &mut err, &host);
    assert!(same(code, 1));
    assert_eq!(out, b"");
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!(
            "crun-start-refused: {} is still populated after {PAUSES} s; the runs left: \
             lease-a, lease-b\n",
            tree.unit().join("supervisor").display()
        )
    );
    assert_eq!(*pauses.borrow(), PAUSES);
}

/// A cgroup that empties at the very last look is not a failure.
#[test]
fn a_cgroup_that_empties_at_the_last_look_is_ended() {
    let tree = Tree::new("start-refused-late");
    let looks = RefCell::new(0_u32);
    let events = tree.unit().join("supervisor/cgroup.events");
    let read = |path: &Path| {
        if path == events {
            *looks.borrow_mut() += 1;
            // Populated for the first look, the check before the kill, and the PAUSES looks of
            // the loop; empty at the look after it.
            let populated = *looks.borrow() <= PAUSES + 1;
            return Ok(format!("populated {}\n", u8::from(populated)));
        }
        read_file(path)
    };
    let host = Host {
        read: &read,
        write: &write_file,
        list: &list_dirs,
        version: &|_| Ok("systemd 258\n".to_owned()),
        pause: &|_| {},
        mount: &tree.mount,
        self_cgroup: &tree.own,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args("resources"), &mut out, &mut err, &host);
    // The actions cgroup never empties here, so the run fails there, after the supervisors.
    assert!(same(code, 1));
    assert!(String::from_utf8(err).unwrap().contains(&format!(
        "{} is still populated",
        tree.unit().join("actions").display()
    )));
    assert_eq!(tree.get("supervisor/cgroup.kill").as_deref(), Some("1"));
}

/// A kill that cannot be written, a file that cannot be read and a directory that cannot be
/// listed each end the verb with status 1 naming the path.
#[test]
fn a_step_that_fails_is_named() {
    // The kill file is a directory.
    let tree = Tree::new("start-refused-write");
    tree.set("supervisor/cgroup.kill/x", "");
    let (code, _, err, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 1));
    assert!(
        err.starts_with(&format!(
            "crun-start-refused: {}: ",
            tree.unit().join("supervisor/cgroup.kill").display()
        )),
        "{err}"
    );
    assert!(
        err.ends_with("; the runs left: lease-a, lease-b\n"),
        "{err}"
    );

    // The subtree_control is not a file.
    let tree = Tree::new("start-refused-control");
    fs::remove_file(tree.unit().join("cgroup.subtree_control")).unwrap();
    let (code, _, err, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 1));
    assert!(err.contains("cgroup.subtree_control"), "{err}");

    // `actions` is a file.
    let tree = Tree::new("start-refused-list");
    fs::remove_dir_all(tree.unit().join("actions")).unwrap();
    tree.set("actions", "");
    let (code, _, err, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 1));
    assert!(
        err.contains(&format!("{}: ", tree.unit().join("actions").display())),
        "{err}"
    );

    // A lease's events are a directory.
    let tree = Tree::new("start-refused-events");
    fs::remove_file(tree.unit().join("actions/lease-a/cgroup.events")).unwrap();
    tree.set("actions/lease-a/cgroup.events/x", "");
    let (code, _, err, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 1));
    assert!(err.contains("actions/lease-a/cgroup.events"), "{err}");
}

/// The unit's cgroup is the parent of this process's leaf; a process that is in no unit's leaf
/// is refused, and nothing is ended.
#[test]
fn the_unit_is_found_from_this_process_own_cgroup() {
    let tree = Tree::new("start-refused-own");
    for (text, said) in [
        (
            "0::/.control\n",
            "this process is in /.control, which is no leaf of a unit",
        ),
        ("0::/\n", "this process is in /, which is no leaf of a unit"),
        ("1:name=x:/a\n", "names no cgroup of version 2"),
    ] {
        fs::write(&tree.own, text).unwrap();
        let (code, _, err, _) = run_on(&tree, NEW, "resources");
        assert!(same(code, 1), "{text}");
        assert!(err.contains(said), "{err}");
    }
    fs::remove_file(&tree.own).unwrap();
    let (code, _, err, _) = run_on(&tree, NEW, "resources");
    assert!(same(code, 1));
    assert!(err.contains("self_cgroup"), "{err}");
    assert_eq!(tree.get("supervisor/cgroup.kill"), None);
}

/// The adapters of a machine read, write and list real files, run a program for the version and
/// say when it cannot be run.
#[test]
fn the_system_adapters_work_on_files_and_programs() {
    let dir = TestDir::under(std::env::temp_dir(), "start-refused-adapters");
    let file = dir.path().join("f");
    write_file(&file, "text").unwrap();
    assert_eq!(read_file(&file).unwrap(), "text");
    fs::create_dir(dir.path().join("b")).unwrap();
    fs::create_dir(dir.path().join("a")).unwrap();
    assert_eq!(list_dirs(dir.path()).unwrap(), ["a", "b"]);
    assert_eq!(
        list_dirs(&dir.path().join("none")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let program = dir.path().join("systemctl");
    fs::write(&program, "#!/bin/sh\necho 'systemd 259 (259.5)'\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        systemctl_version(program.to_str().unwrap()).unwrap(),
        "systemd 259 (259.5)\n"
    );
    assert!(systemctl_version(dir.path().join("none").to_str().unwrap()).is_err());
    let system = Host::system();
    (system.pause)(std::time::Duration::ZERO);
    assert_eq!(system.mount, Path::new("/sys/fs/cgroup"));
    assert_eq!(system.self_cgroup, Path::new("/proc/self/cgroup"));
    assert_eq!(
        (system.version)(program.to_str().unwrap()).unwrap(),
        "systemd 259 (259.5)\n"
    );
}

/// The verb is answered only for its own word; `main_with` runs the rest as usual.
#[test]
fn only_the_verb_is_dispatched() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let other = ["js-agent", "crun-gpu-dropin"].map(OsString::from);
    assert!(dispatch(&other, &mut out, &mut err).is_none());
    assert!(dispatch(&[], &mut out, &mut err).is_none());
    let code = dispatch(&args("success"), &mut out, &mut err).expect("the verb is named");
    assert!(same(code, 0));
    assert_eq!((out, err), (Vec::new(), Vec::new()));
}

/// The program asked for systemd's version is `/usr/bin/systemctl`, or the one `--systemctl`
/// names. Catches a flag that is parsed and not used, which would read another program's
/// version.
#[test]
fn the_version_is_asked_of_the_program_the_command_line_names() {
    let tree = Tree::new("start-refused-systemctl");
    let asked = RefCell::new(Vec::new());
    let host = Host {
        read: &read_file,
        write: &write_file,
        list: &list_dirs,
        version: &|program: &str| {
            asked.borrow_mut().push(program.to_owned());
            Ok("systemd 257\n".to_owned())
        },
        pause: &|_| {},
        mount: &tree.mount,
        self_cgroup: &tree.own,
    };
    for extra in [&[][..], &["--systemctl", "/opt/bin/systemctl"][..]] {
        let mut argv = args("resources");
        argv.extend(extra.iter().map(OsString::from));
        let code = run(&argv, &mut Vec::new(), &mut Vec::new(), &host);
        assert!(same(code, 0));
    }
    assert_eq!(
        *asked.borrow(),
        ["/usr/bin/systemctl", "/opt/bin/systemctl"]
    );
}
