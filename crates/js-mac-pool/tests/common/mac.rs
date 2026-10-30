//! A fixture for the tests that run the generated profile under the real `sandbox-exec`:
//! a store with a pinned tree and an unpinned one, a run directory, the canaries, and the
//! real Xcode of the host. Used by the macOS runner tests and by the privileged tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use js_mac_pool::seatbelt::{
    AbsPath, Canaries, Fixture, PinnedTree, ReadGrant, SelfTest, SlotLayout,
};
use js_mac_pool::xcode::{Catalog, HostXcode, State, XcodeFlag};

use super::{Ran, Scratch, build_of, diff_id, real_answers, run};

pub const DATA: &str = "fixture-data\n";

/// The Xcode the host's tools use by default.
pub const XCODE_APP: &str = "/Applications/Xcode.app";

pub struct Mac {
    pub scratch: Scratch,
    pub run: PathBuf,
    pub uid_entry: PathBuf,
    pub pinned: PinnedTree,
    pub unpinned: PinnedTree,
    pub xcode: HostXcode,
    pub canaries: Canaries,
    /// Files outside the scratch directory, removed with the fixture.
    outside: Vec<PathBuf>,
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::new(path).expect("an acceptable path")
}

impl Mac {
    /// Places the fixture under `scratch`. `uid_entry` is the directory the run may write
    /// besides its run directory (the user's own `/var/folders` entry, or a stand-in).
    pub fn build(scratch: Scratch, uid_entry: PathBuf, tag: &str) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let store = abs(&scratch.dir("store"));
        let pinned = PinnedTree::new(&store, &diff_id('a')).expect("pinned");
        let unpinned = PinnedTree::new(&store, &diff_id('b')).expect("unpinned");
        let mine = PathBuf::from(pinned.path().as_str());
        let theirs = PathBuf::from(unpinned.path().as_str());
        std::fs::create_dir_all(mine.join("bin")).expect("tree");
        std::fs::create_dir_all(&theirs).expect("tree");
        std::fs::write(mine.join("data.txt"), DATA).expect("data");
        std::fs::write(mine.join("bin/read"), "#!/bin/sh\nexec /bin/cat \"$@\"\n").expect("tool");
        std::fs::set_permissions(
            mine.join("bin/read"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
        std::fs::write(theirs.join("secret.txt"), "secret\n").expect("secret");

        let run_dir = scratch.dir("run");
        std::fs::write(run_dir.join("stub.c"), "int main(void) { return 0; }\n").expect("stub");
        let other_home = scratch.file("home-other/secret.txt", "secret\n");
        let input_store = scratch.file("inputs/secret.txt", "secret\n");
        let on_data = scratch.file("sysdata/secret.txt", "secret\n");

        let pid = std::process::id();
        let shared = PathBuf::from(format!("/Users/Shared/js-mac-pool-{tag}-{pid}"));
        let var_tmp = PathBuf::from(format!("/private/var/tmp/js-mac-pool-{tag}-{pid}"));
        for file in [&shared, &var_tmp] {
            std::fs::write(file, "secret\n").expect("a world-writable directory");
        }
        let canaries = Canaries {
            other_home: abs(&other_home),
            input_store: abs(&input_store),
            unpinned_tree: abs(&theirs.join("secret.txt")),
            users_shared: abs(&shared),
            private_var_tmp: abs(&var_tmp),
            system_volumes_data: AbsPath::parse(&format!(
                "/System/Volumes/Data{}",
                on_data.to_str().expect("text")
            ))
            .expect("a path through the data volume"),
        };

        let app = XCODE_APP;
        let flag = XcodeFlag::parse(&format!("{}={app}", build_of(app))).expect("flag");
        let catalog = Catalog::survey(&[flag], real_answers).expect("catalog");
        let state = (catalog.all()[0].state, catalog.all()[0].reason.clone());
        let xcode = catalog.host(&build_of(app));
        let xcode = xcode.unwrap_or_else(|| panic!("Xcode is not ready: {state:?}"));
        assert_eq!(state.0, State::Ready);

        Self {
            scratch,
            run: run_dir,
            uid_entry,
            pinned,
            unpinned,
            xcode,
            canaries,
            outside: vec![shared, var_tmp],
        }
    }

    pub fn layout(&self) -> SlotLayout {
        SlotLayout {
            run: abs(&self.run),
            uid_entry: abs(&self.uid_entry),
            inputs: Vec::new(),
            read_denies: Vec::new(),
        }
    }

    pub fn grants(&self) -> Vec<ReadGrant> {
        vec![
            ReadGrant::Tree(self.pinned.clone()),
            ReadGrant::Xcode(self.xcode.clone()),
        ]
    }

    pub fn fixture(&self) -> Fixture {
        Fixture {
            layout: self.layout(),
            grants: self.grants(),
            granted_file: abs(&PathBuf::from(self.pinned.path().as_str()).join("data.txt")),
            canaries: self.canaries.clone(),
        }
    }

    pub fn pinned_file(&self, name: &str) -> PathBuf {
        PathBuf::from(self.pinned.path().as_str()).join(name)
    }

    pub fn unpinned_file(&self) -> PathBuf {
        PathBuf::from(self.unpinned.path().as_str()).join("secret.txt")
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        for file in &self.outside {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// Runs every step of `test`, as `ids` when given, and returns what each left.
pub fn run_steps(test: &SelfTest, ids: Option<(u32, u32)>) -> Vec<Ran> {
    test.steps()
        .iter()
        .map(|step| run(&step.argv(), step.env(), Some(step.cwd()), ids))
        .collect()
}

/// The steps with their results, for a failure message, and the kernel's sandbox log.
pub fn describe(test: &SelfTest, ran: &[Ran]) -> String {
    let mut out = String::new();
    for (step, ran) in test.steps().iter().zip(ran) {
        out.push_str(&format!(
            "{} {:?} ok={} {:?} {}\n",
            step.class(),
            step.role(),
            ran.ok,
            step.command(),
            ran.stderr.trim()
        ));
    }
    out.push_str(&sandbox_log());
    out
}

/// The sandbox's denials of the last minutes, best effort: the log needs privileges that
/// not every runner has.
pub fn sandbox_log() -> String {
    let attempts: [&[&str]; 2] = [&["/usr/bin/log"], &["/usr/bin/sudo", "-n", "/usr/bin/log"]];
    for program in attempts {
        let out = Command::new(program[0])
            .args(&program[1..])
            .args([
                "show",
                "--last",
                "3m",
                "--style",
                "compact",
                "--predicate",
                "sender == \"Sandbox\" AND eventMessage CONTAINS \"deny\"",
            ])
            .output();
        if let Ok(out) = out {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && !text.trim().is_empty() {
                // Every sandboxed process asks for /dev/dtracehelper and is refused; and
                // other tests run beside this one. Keep the rest of the tools' denials.
                let tools = [
                    " cc(",
                    " xcrun(",
                    " xcodebuild(",
                    " codesign(",
                    " sh(",
                    " bash(",
                    " clang(",
                    " ld(",
                ];
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|line| !line.contains("dtracehelper"))
                    .filter(|line| tools.iter().any(|tool| line.contains(tool)))
                    .collect();
                // The compiler's own lines first: the rest is the Xcode tools' fallbacks.
                let (own, rest): (Vec<&str>, Vec<&str>) = lines
                    .into_iter()
                    .partition(|line| line.contains(" cc(") || line.contains(" xcrun("));
                let shown: Vec<&str> = own
                    .into_iter()
                    .chain(rest.into_iter().rev())
                    .take(100)
                    .collect();
                return format!("sandbox log:\n{}\n", shown.join("\n"));
            }
        }
    }
    "sandbox log: unavailable\n".to_owned()
}
