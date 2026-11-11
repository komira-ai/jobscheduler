//! `cargo xtask merge-profiles`, run as the built `xtask` program with a stand-in `rustc` and
//! a stand-in `llvm-profdata` in a stand-in sysroot (small shell scripts in the test's own
//! scratch directory). The stand-ins log their arguments, one per line, and end with the
//! status the test chooses; nothing is merged for real.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;

/// The host the stand-in rustc names.
const HOST: &str = "x86_64-test-host";

/// What the stand-in rustc answers, and how the stand-ins end.
struct Toolchain {
    /// What `--print sysroot` prints; `None` prints the rig's sysroot.
    sysroot: Option<&'static str>,
    /// What `--print host-tuple` prints.
    host: &'static str,
    /// rustc's exit status.
    rustc: i32,
    /// llvm-profdata's exit status; `None` installs no llvm-profdata.
    profdata: Option<i32>,
}

impl Default for Toolchain {
    fn default() -> Self {
        Toolchain {
            sysroot: None,
            host: HOST,
            rustc: 0,
            profdata: Some(0),
        }
    }
}

/// A scratch directory of its own for each test: the stand-ins, the profiles directory
/// (`profiles/`) and the output.
struct Rig {
    dir: PathBuf,
    /// Removes the directory when the rig is dropped.
    _scratch: js_testkit::dir::TestDir,
}

/// What one run did.
#[derive(Debug, PartialEq)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Rig {
    fn new(name: &str, toolchain: Toolchain) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("merge-profiles-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        let sysroot = dir.join("sysroot");
        let bin = sysroot.join("lib/rustlib").join(HOST).join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(dir.join("profiles")).unwrap();
        let calls = dir.join("calls");
        let answer = toolchain
            .sysroot
            .map_or_else(|| sysroot.display().to_string(), str::to_owned);
        install_script(
            &dir.join("rustc"),
            &format!(
                "#!/bin/sh\n{{ echo '== rustc'; printf '%s\\n' \"$@\"; }} >> \"{calls}\"\n\
                 case \"$2\" in sysroot) printf '%s\\n' '{answer}';; host-tuple) printf '{host}\\n';; esac\n\
                 exit {status}\n",
                calls = calls.display(),
                host = toolchain.host,
                status = toolchain.rustc,
            ),
            "755",
        );
        if let Some(status) = toolchain.profdata {
            install_script(
                &bin.join("llvm-profdata"),
                &format!(
                    "#!/bin/sh\n{{ echo '== llvm-profdata'; printf '%s\\n' \"$@\"; }} >> \"{calls}\"\n\
                     [ {status} -eq 0 ] && echo merged > \"$6\"\nexit {status}\n",
                    calls = calls.display(),
                ),
                "755",
            );
        }
        Rig {
            dir,
            _scratch: scratch,
        }
    }

    fn profiles(&self) -> PathBuf {
        self.dir.join("profiles")
    }

    fn out(&self) -> PathBuf {
        self.dir.join("shard-1.profraw")
    }

    fn list(&self) -> PathBuf {
        self.dir.join("shard-1.profraw.inputs")
    }

    fn put(&self, name: &str) {
        let path = self.profiles().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "raw").unwrap();
    }

    /// The log of the stand-ins: each call as `== <program>` and its arguments; empty when
    /// none ran.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
    }

    fn run_with(&self, dir: &Path, out: &Path) -> Ran {
        let ran = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("merge-profiles")
            .arg("--dir")
            .arg(dir)
            .arg("--out")
            .arg(out)
            .arg("--rustc")
            .arg(self.dir.join("rustc"))
            .output()
            .unwrap();
        Ran {
            status: ran.status.code().unwrap(),
            stdout: String::from_utf8(ran.stdout).unwrap(),
            stderr: String::from_utf8(ran.stderr).unwrap(),
        }
    }

    fn run(&self) -> Ran {
        self.run_with(&self.profiles(), &self.out())
    }

    fn rustc(&self) -> String {
        self.dir.join("rustc").display().to_string()
    }

    fn profdata(&self) -> String {
        let bin = self.dir.join("sysroot/lib/rustlib").join(HOST).join("bin");
        bin.join("llvm-profdata").display().to_string()
    }

    /// The calls of a run that asked rustc both questions.
    fn asked(&self) -> String {
        "== rustc\n--print\nsysroot\n== rustc\n--print\nhost-tuple\n".to_owned()
    }
}

fn failed(stderr: String) -> Ran {
    Ran {
        status: 1,
        stdout: String::new(),
        stderr,
    }
}

/// Asserts that every raw profile at the top of the directory, and nothing else, is listed,
/// sorted, in `<out>.inputs`, and that the toolchain's llvm-profdata merges that list into
/// `<out>`: not a file of another ending, not a profile in a subdirectory (the build's), and
/// not a directory whose name ends `.profraw`.
#[test]
fn every_raw_profile_at_the_top_and_nothing_else_is_merged() {
    let rig = Rig::new("merged", Toolchain::default());
    rig.put("b-2-1.profraw");
    rig.put("a-1-1.profraw");
    rig.put("notes.txt");
    rig.put("coverage/deps/c-3-1.profraw");
    fs::create_dir_all(rig.profiles().join("d.profraw")).unwrap();
    let ran = rig.run();
    let out = rig.out().display().to_string();
    assert_eq!(
        ran,
        Ran {
            status: 0,
            stdout: format!("merged 2 raw profiles into {out}\n"),
            stderr: String::new(),
        }
    );
    let profiles = rig.profiles().display().to_string();
    assert_eq!(
        fs::read_to_string(rig.list()).unwrap(),
        format!("{profiles}/a-1-1.profraw\n{profiles}/b-2-1.profraw\n")
    );
    assert_eq!(
        rig.calls(),
        format!(
            "{}== llvm-profdata\nmerge\n-sparse\n-f\n{}\n-o\n{out}\n",
            rig.asked(),
            rig.list().display()
        )
    );
    assert_eq!(fs::read_to_string(rig.out()).unwrap(), "merged\n");
}

/// Asserts that a directory with no raw profile at its top level is refused, naming it,
/// before any list is written or any program runs.
#[test]
fn a_directory_without_a_raw_profile_is_refused_before_anything_runs() {
    let rig = Rig::new("none", Toolchain::default());
    rig.put("coverage/deps/c-3-1.profraw");
    let ran = rig.run();
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: no *.profraw file in {}\n",
            rig.profiles().display()
        ))
    );
    assert_eq!(rig.calls(), "");
    assert!(!rig.list().exists());
}

/// Asserts that a directory that cannot be read is refused, naming it.
#[test]
fn a_missing_directory_is_refused_naming_it() {
    let rig = Rig::new("missing", Toolchain::default());
    let dir = rig.dir.join("absent");
    let ran = rig.run_with(&dir, &rig.out());
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: cannot read {}: No such file or directory (os error 2)\n",
            dir.display()
        ))
    );
    assert_eq!(rig.calls(), "");
}

/// Asserts that a list that cannot be written (its directory is missing) fails naming it,
/// before any program runs.
#[test]
fn a_list_that_cannot_be_written_fails_naming_it() {
    let rig = Rig::new("unwritable", Toolchain::default());
    rig.put("a-1-1.profraw");
    let out = rig.dir.join("absent/shard-1.profraw");
    let ran = rig.run_with(&rig.profiles(), &out);
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: cannot write {}.inputs: No such file or directory (os error 2)\n",
            out.display()
        ))
    );
    assert_eq!(rig.calls(), "");
}

/// Asserts that a rustc that fails stops the run, naming it and its status, before
/// llvm-profdata runs.
#[test]
fn a_failing_rustc_stops_the_run() {
    let toolchain = Toolchain {
        rustc: 3,
        ..Toolchain::default()
    };
    let rig = Rig::new("rustc-fails", toolchain);
    rig.put("a-1-1.profraw");
    let ran = rig.run();
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: {} failed: exit status: 3\n",
            rig.rustc()
        ))
    );
    assert_eq!(rig.calls(), "== rustc\n--print\nsysroot\n");
}

/// Asserts that an empty sysroot, or a host of two lines, is refused, quoting what rustc
/// printed, before llvm-profdata runs.
#[test]
fn an_answer_that_is_not_one_line_is_refused() {
    let empty = Toolchain {
        sysroot: Some(""),
        ..Toolchain::default()
    };
    let rig = Rig::new("empty-sysroot", empty);
    rig.put("a-1-1.profraw");
    assert_eq!(
        rig.run(),
        failed(format!(
            "xtask: merge-profiles: {} --print sysroot printed \"\\n\", not one line\n",
            rig.rustc()
        ))
    );
    assert_eq!(rig.calls(), "== rustc\n--print\nsysroot\n");

    let two = Toolchain {
        host: "x86_64\\nsecond",
        ..Toolchain::default()
    };
    let rig = Rig::new("two-line-host", two);
    rig.put("a-1-1.profraw");
    assert_eq!(
        rig.run(),
        failed(format!(
            "xtask: merge-profiles: {} --print host-tuple printed \"x86_64\\nsecond\\n\", not one line\n",
            rig.rustc()
        ))
    );
    assert_eq!(rig.calls(), rig.asked());
}

/// Asserts that a sysroot without llvm-profdata (no `llvm-tools` component) fails naming the
/// path it looked for.
#[test]
fn a_sysroot_without_llvm_profdata_fails_naming_the_path() {
    let toolchain = Toolchain {
        profdata: None,
        ..Toolchain::default()
    };
    let rig = Rig::new("no-profdata", toolchain);
    rig.put("a-1-1.profraw");
    assert_eq!(
        rig.run(),
        failed(format!(
            "xtask: merge-profiles: cannot run {}: No such file or directory (os error 2)\n",
            rig.profdata()
        ))
    );
    assert_eq!(rig.calls(), rig.asked());
}

/// Asserts that an llvm-profdata that fails fails the run, naming it and its status.
#[test]
fn a_failing_llvm_profdata_fails_the_run() {
    let toolchain = Toolchain {
        profdata: Some(1),
        ..Toolchain::default()
    };
    let rig = Rig::new("profdata-fails", toolchain);
    rig.put("a-1-1.profraw");
    assert_eq!(
        rig.run(),
        failed(format!(
            "xtask: merge-profiles: {} failed: exit status: 1\n",
            rig.profdata()
        ))
    );
    assert!(!rig.out().exists());
}
