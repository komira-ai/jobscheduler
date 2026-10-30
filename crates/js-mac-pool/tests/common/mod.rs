//! Helpers the test binaries share. Not every binary uses every helper.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_bundle::DiffId;
use js_mac_pool::seatbelt::{
    AbsPath, Assurance, Canaries, Fixture, MacosMajor, Outcome, PinnedTree, ReadGrant, Role,
    SelfTest, SlotLayout,
};
use js_mac_pool::xcode::{Catalog, HostXcode, Question, Reply, Unanswered, XcodeFlag};

pub fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).expect("a good path")
}

pub fn diff_id(digit: char) -> DiffId {
    DiffId::parse(&format!("sha256:{}", digit.to_string().repeat(64))).expect("a diff id")
}

pub fn tree(store: &str, digit: char) -> PinnedTree {
    PinnedTree::new(&path(store), &diff_id(digit)).expect("a tree")
}

/// A directory of this test's own under the build's target directory, emptied first and
/// removed when dropped. Its path is canonical.
pub struct Scratch {
    pub root: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("mac-pool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch directory");
        Self {
            root: base.canonicalize().expect("canonical scratch directory"),
        }
    }

    /// A scratch directory below `parent`, which the caller vouches for.
    pub fn under(parent: &Path, tag: &str) -> Self {
        let base = parent.join(format!("mac-pool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch directory");
        Self {
            root: base.canonicalize().expect("canonical scratch directory"),
        }
    }

    pub fn dir(&self, relative: &str) -> PathBuf {
        let dir = self.root.join(relative);
        std::fs::create_dir_all(&dir).expect("directory");
        dir
    }

    pub fn file(&self, relative: &str, content: &str) -> PathBuf {
        let file = self.root.join(relative);
        std::fs::create_dir_all(file.parent().expect("a parent")).expect("parent directory");
        std::fs::write(&file, content).expect("file");
        file
    }

    pub fn abs(&self, relative: &str) -> AbsPath {
        AbsPath::new(&self.root.join(relative)).expect("an acceptable path")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What `xcodebuild -version` prints for the fixture Xcode.
pub const VERSION_TEXT: &str = "Xcode 15.4\nBuild version 15F31d\n";

/// The uid that owns the directories the tests make.
pub fn owner() -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(env!("CARGO_TARGET_TMPDIR"))
        .expect("the target directory")
        .uid()
}

/// The answers of a fixture Xcode: every question is yes, `xcrun --find` prints a path
/// inside the Xcode, and `metal` is not found.
pub fn fixture_answers(question: &Question<'_>) -> Reply {
    match question.args {
        ["-version"] => Ok(VERSION_TEXT.to_owned()),
        ["--find", "metal"] => Err(Unanswered::Refused("not found".to_owned())),
        ["--find", tool] => Ok(format!(
            "{}/usr/bin/{tool}\n",
            question.developer_dir.as_str()
        )),
        _ => Ok(String::new()),
    }
}

/// A ready Xcode at `<scratch>/<name>/` (an app directory made here), build `15F31d`.
pub fn xcode_host(scratch: &Scratch, name: &str) -> HostXcode {
    scratch.dir(&format!("{name}/Contents/Developer"));
    let flag = XcodeFlag::parse(&format!("15F31d={}", scratch.abs(name).as_str())).expect("flag");
    let catalog = Catalog::survey(&[flag], owner(), fixture_answers).expect("catalog");
    catalog.host("15F31d").expect("a ready Xcode")
}

pub fn layout() -> SlotLayout {
    SlotLayout {
        run: path("/Volumes/work/slot1/run"),
        uid_entry: Some(path("/private/var/folders/ab/cdef0123")),
        inputs: vec![path("/Volumes/work/slot1/run/in")],
        read_denies: vec![
            path("/Volumes/work/state"),
            path("/Library/Keychains/System.keychain"),
        ],
    }
}

pub fn canaries() -> Canaries {
    Canaries {
        other_home: path("/Users/other/secret"),
        input_store: path("/Volumes/work/inputs/secret"),
        unpinned_tree: path("/Volumes/work/store/v1/objects/trees/ffff/secret"),
        users_shared: path("/Users/Shared/secret"),
        private_var_tmp: path("/private/var/tmp/secret"),
        system_volumes_data: path("/System/Volumes/Data/Users/other/secret"),
        sibling_run: path("/Volumes/work/slot2/run/secret"),
        agent_state: path("/Volumes/work/state/agent/token"),
        helper_state: path("/Volumes/work/state/helper/db"),
    }
}

pub fn fixture(grants: Vec<ReadGrant>) -> Fixture {
    Fixture {
        layout: layout(),
        grants,
        granted_file: path("/Volumes/work/store/v1/objects/trees/aaaa/data"),
        canaries: canaries(),
        privileged_found: js_mac_pool::seatbelt::scan_privileged(&[]),
    }
}

/// The assurance of a self-test that went right, for tests of what an assurance allows.
pub fn assurance() -> Assurance {
    let test = SelfTest::new(MacosMajor::Macos14, fixture(Vec::new())).expect("steps");
    let ran: Vec<Outcome> = test
        .steps()
        .iter()
        .map(|step| match step.role() {
            Role::Denied => Outcome::Refused,
            Role::Control | Role::Allowed => Outcome::Success,
        })
        .collect();
    test.judge(&ran).expect("every step did what its role says")
}

/// Asks the real Xcode at `app` its questions: runs each with `DEVELOPER_DIR` set.
pub fn real_answers(question: &Question<'_>) -> Reply {
    let out = Command::new(&question.program)
        .args(question.args)
        .env_clear()
        .envs(question.env())
        .output()
        .map_err(|e| Unanswered::NoAnswer(e.to_string()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(Unanswered::Refused(
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The build of the Xcode at `app`, as its own `xcodebuild -version` prints it.
pub fn build_of(app: &str) -> String {
    let out = Command::new(format!("{app}/Contents/Developer/usr/bin/xcodebuild"))
        .arg("-version")
        .env_clear()
        .env("DEVELOPER_DIR", format!("{app}/Contents/Developer"))
        .output()
        .expect("xcodebuild runs");
    let shown = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = shown
        .lines()
        .find_map(|line| line.strip_prefix("Build version "))
        .unwrap_or_else(|| panic!("no build in {shown:?}"));
    line.trim().to_owned()
}

/// What running one command left.
pub struct Ran {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// What the caller of the self-test reports for this run.
    pub fn outcome(&self) -> Outcome {
        Outcome::from_exit(self.code, &self.stderr)
    }
}

/// Runs `argv` with exactly `env`, in `cwd` when given, as `ids` (a user and group) when
/// given.
pub fn run(
    argv: &[OsString],
    env: &[(String, String)],
    cwd: Option<&str>,
    ids: Option<(u32, u32)>,
) -> Ran {
    use std::os::unix::process::CommandExt as _;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some((uid, gid)) = ids {
        command.gid(gid).uid(uid);
    }
    let out = command.output().expect("the command starts");
    Ran {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

pub mod mac;
