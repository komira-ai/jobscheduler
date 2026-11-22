//! The agent's start on a Mac mounts the scratch volume the host setup made on the scratch root
//! while the scratch root is on the state root's file system, as it is after every boot (no file
//! system table names the volume): `lifecycle::scratch_volume::mount_at_start`, over the fake
//! Mac of the host setup's tests.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use js_agent::flags_file::FileFacts;
use js_agent::lifecycle::scratch_volume::{DISKUTIL, mount_at_start};
use js_agent::{CommandRunner, FileReader};

use super::fake_mac::{DEVICE, FakeMac, Volume};

/// The fake Mac as the agent's start sees it: its commands, each recorded, and the devices of
/// its paths.
struct Start {
    mac: Rc<FakeMac>,
    ran: RefCell<Vec<String>>,
}

impl CommandRunner for Start {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.ran
            .borrow_mut()
            .push(format!("{program} {}", args.join(" ")));
        self.mac
            .run(program, args)
            .unwrap_or_else(|| Err(format!("{program}: not found")))
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        self.run(program, args)
    }
}

impl FileReader for Start {
    fn read(&self, path: &str) -> Result<String, String> {
        Err(format!("{path}: not read here"))
    }

    fn link(&self, path: &str) -> Result<String, String> {
        Err(format!("{path}: not read here"))
    }

    fn device(&self, path: &Path) -> Result<u64, String> {
        self.mac
            .look(path)
            .map(|facts| facts.device)
            .map_err(|why| why.to_string())
    }

    fn is_tmpfs(&self, _path: &Path) -> Result<bool, String> {
        Ok(false)
    }

    fn facts(&self, _path: &Path) -> io::Result<FileFacts> {
        Err(io::Error::other("not looked at here"))
    }

    fn entries(&self, _dir: &Path) -> Result<Vec<String>, io::Error> {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }
}

/// A Mac whose state root and scratch root are directories of a test directory named `name`,
/// one file system until the scratch volume is mounted on the scratch root.
struct Rig {
    _tmp: js_testkit::dir::TestDir,
    state: PathBuf,
    scratch: PathBuf,
    start: Start,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let tmp = js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let (state, scratch) = (tmp.path().join("state"), tmp.path().join("scratch"));
        fs::create_dir(&state).unwrap();
        fs::create_dir(&scratch).unwrap();
        Rig {
            _tmp: tmp,
            state,
            scratch,
            start: Start {
                mac: FakeMac::new(),
                ran: RefCell::new(Vec::new()),
            },
        }
    }

    fn mount(&self) -> Result<(), String> {
        mount_at_start(&self.start, &self.start, &self.state, &self.scratch)
    }

    fn mount_line(&self) -> String {
        format!(
            "{DISKUTIL} mount nobrowse -mountPoint {} {DEVICE}",
            self.scratch.display()
        )
    }
}

/// Asserts that a start whose scratch root shares the state root's file system mounts the
/// volume there, `nobrowse`, so that the scratch root is then on a device of its own; and that
/// a volume macOS mounted in `/Volumes` by itself at the boot is unmounted first. Catches an
/// agent that refuses to start after every reboot of a Mac.
#[test]
fn a_start_mounts_the_scratch_volume_on_the_scratch_root() {
    let rig = Rig::new("start-volume-unmounted");
    rig.start.mac.with_volume(Volume {
        mount: None,
        owners: true,
    });
    assert_eq!(rig.mount(), Ok(()));
    assert_eq!(rig.start.mac.changes(), [rig.mount_line()]);
    assert_ne!(rig.start.device(&rig.scratch), rig.start.device(&rig.state));

    let rig = Rig::new("start-volume-elsewhere");
    rig.start.mac.with_volume(Volume {
        mount: Some(PathBuf::from("/Volumes/js-agent-scratch")),
        owners: true,
    });
    assert_eq!(rig.mount(), Ok(()));
    assert_eq!(
        rig.start.mac.changes(),
        [format!("{DISKUTIL} unmount {DEVICE}"), rig.mount_line()]
    );
}

/// Asserts that a start whose scratch root is on a file system of its own already, or whose
/// roots cannot be looked at, runs no command and is left to the agent's check of its file
/// systems.
#[test]
fn a_start_with_the_roots_apart_or_unlooked_runs_nothing() {
    let rig = Rig::new("start-volume-mounted");
    rig.start.mac.with_volume(Volume {
        mount: Some(rig.scratch.clone()),
        owners: true,
    });
    assert_eq!(rig.mount(), Ok(()));
    assert!(rig.start.ran.borrow().is_empty());

    for unlooked in ["state", "scratch"] {
        let rig = Rig::new(&format!("start-volume-unlooked-{unlooked}"));
        let path = if unlooked == "state" {
            rig.state.clone()
        } else {
            rig.scratch.clone()
        };
        rig.start.mac.look_fails.borrow_mut().insert(path);
        assert_eq!(rig.mount(), Ok(()), "{unlooked}");
        assert!(rig.start.ran.borrow().is_empty(), "{unlooked}");
    }
}

/// Asserts that a start that must mount the volume and cannot ends with the reason, starting
/// `--scratch-root:`: the Data volume's container holds no scratch volume (the update hook's
/// prepare was not run), `diskutil` cannot be read, or the mount fails.
#[test]
fn a_start_that_cannot_mount_the_volume_says_why() {
    let rig = Rig::new("start-volume-absent");
    assert_eq!(
        rig.mount(),
        Err(format!(
            "--scratch-root: {} is on the file system of the --state-root and {DISKUTIL} lists \
             no js-agent-scratch in disk3, the Data volume's container: the update hook's \
             prepare makes it",
            rig.scratch.display()
        ))
    );
    assert!(rig.start.mac.changes().is_empty());

    let rig = Rig::new("start-volume-unread");
    let info = format!("{DISKUTIL} info -plist /System/Volumes/Data");
    rig.start.mac.answer(&info, Err("exit status: 1"));
    assert_eq!(
        rig.mount(),
        Err(format!("--scratch-root: {info}: exit status: 1"))
    );

    let rig = Rig::new("start-volume-unmountable");
    rig.start.mac.with_volume(Volume {
        mount: None,
        owners: true,
    });
    rig.start
        .mac
        .answer(&rig.mount_line(), Err("exit status: 1"));
    assert_eq!(
        rig.mount(),
        Err(format!(
            "--scratch-root: {}: exit status: 1",
            rig.mount_line()
        ))
    );
}
