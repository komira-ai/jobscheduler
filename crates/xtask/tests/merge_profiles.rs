//! `cargo xtask merge-profiles`, run as the built `xtask` program with a stand-in `rustc` and
//! a stand-in `llvm-profdata` in a stand-in sysroot (small shell scripts in the test's own
//! scratch directory). The stand-ins log their arguments, one per line. The stand-in
//! llvm-profdata judges each profile it is given by its first word: `refuse` it refuses,
//! `growing` it refuses and appends to (a process still writing it), `vanish` it refuses
//! and removes, `listed` it refuses only beside others (from a list file); every refusal is
//! a `warning: <file>: ...` line, and a merge with one ends `error: no profile can be merged`
//! with status 1, as llvm-profdata's does. With a status the test chooses it fails every
//! call. Nothing is merged for real.

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
                     [ {status} -eq 0 ] || {{ echo 'error: forced' >&2; exit {status}; }}\n\
                     if [ \"$3\" = -f ]; then files=$(cat \"$4\"); out=$6; else files=$3; out=$5; fi\n\
                     bad=0\n\
                     for f in $files; do\n\
                       [ -e \"$f\" ] || {{ echo \"warning: $f: No such file or directory\" >&2; bad=1; continue; }}\n\
                       case $(head -c 7 \"$f\") in\n\
                         refuse*) echo \"warning: $f: file header is corrupt\" >&2; bad=1;;\n\
                         growing) echo more >> \"$f\"; echo \"warning: $f: file header is corrupt\" >&2; bad=1;;\n\
                         vanish*) rm \"$f\"; echo \"warning: $f: file header is corrupt\" >&2; bad=1;;\n\
                         listed*) [ \"$3\" = -f ] && {{ echo \"warning: $f: hash mismatch\" >&2; bad=1; }};;\n\
                       esac\n\
                     done\n\
                     [ $bad -eq 0 ] || {{ echo 'error: no profile can be merged' >&2; exit 1; }}\n\
                     echo merged > \"$out\"\n",
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
        self.put_bytes(name, b"raw");
    }

    fn put_bytes(&self, name: &str, bytes: &[u8]) {
        let path = self.profiles().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn at(&self, name: &str) -> String {
        self.profiles().join(name).display().to_string()
    }

    fn left_out(&self) -> PathBuf {
        self.dir.join("shard-1.profraw.left-out")
    }

    fn check(&self) -> String {
        self.dir.join("shard-1.profraw.check").display().to_string()
    }

    /// The calls of a stand-in llvm-profdata given `file` alone.
    fn alone(&self, file: &str) -> String {
        format!(
            "== llvm-profdata\nmerge\n-sparse\n{file}\n-o\n{}\n",
            self.check()
        )
    }

    /// The calls of the merge of the list.
    fn merge(&self) -> String {
        format!(
            "== llvm-profdata\nmerge\n-sparse\n-f\n{}\n-o\n{}\n",
            self.list().display(),
            self.out().display()
        )
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
            stdout: format!("merged 2 raw profiles into {out}; left out 0 cut short\n"),
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

/// Asserts that an llvm-profdata that fails every call fails the run, naming it, its status
/// and each profile it then refuses alone, after passing on what it printed.
#[test]
fn a_failing_llvm_profdata_fails_the_run() {
    let toolchain = Toolchain {
        profdata: Some(3),
        ..Toolchain::default()
    };
    let rig = Rig::new("profdata-fails", toolchain);
    rig.put("a-1-1.profraw");
    let a = rig.at("a-1-1.profraw");
    assert_eq!(
        rig.run(),
        failed(format!(
            "error: forced\nxtask: merge-profiles: {} failed: exit status: 3; it refuses 1 of \
             the 1 raw profiles alone:\n  {a} (3 bytes, written by process 1): error: forced\n",
            rig.profdata()
        ))
    );
    assert!(!rig.out().exists());
}

/// A raw profile of LLVM 23 (raw version 11): the header of one data record, one counter and
/// eight bytes of names, then that many bytes, 240 in all.
fn whole_profile() -> Vec<u8> {
    let mut words = [0u64; 19];
    words[0] = 0xff6c_7072_6f66_7281;
    words[1] = 11;
    words[3] = 1;
    words[5] = 1;
    words[12] = 8;
    words[18] = 2;
    let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    bytes.resize(240, 7);
    bytes
}

/// Asserts that a profile cut short (one that ends before the sections its header gives) and
/// an empty one are left out of the merge, each named with its size and the process its name
/// gives, copied into `<out>.left-out`, and counted, while the whole profile and a file this
/// cannot size beside them are merged. Catches a merge that fails on a cut-short profile or
/// leaves one out without a word.
#[test]
fn a_cut_short_profile_is_left_out_by_name_and_the_rest_are_merged() {
    let rig = Rig::new("cut-short", Toolchain::default());
    let whole = whole_profile();
    rig.put_bytes("jobscheduler-17-5_0.profraw", &whole);
    rig.put_bytes("jobscheduler-4242-5_0.profraw", &whole[..200]);
    rig.put_bytes("empty.profraw", b"");
    rig.put("other.profraw");
    let ran = rig.run();
    let out = rig.out().display().to_string();
    let kept = rig.left_out().display().to_string();
    assert_eq!(
        ran,
        Ran {
            status: 0,
            stdout: format!(
                "merged 2 raw profiles into {out}; left out 2 cut short, kept in {kept}\n"
            ),
            stderr: format!(
                "xtask: merge-profiles: left out {}: cut short (empty), written by a process \
                 its name does not give\n\
                 xtask: merge-profiles: left out {}: cut short (200 of the 240 bytes its \
                 header gives), written by process 4242\n",
                rig.at("empty.profraw"),
                rig.at("jobscheduler-4242-5_0.profraw"),
            ),
        }
    );
    assert_eq!(
        fs::read_to_string(rig.list()).unwrap(),
        format!(
            "{}\n{}\n",
            rig.at("jobscheduler-17-5_0.profraw"),
            rig.at("other.profraw")
        )
    );
    assert_eq!(
        fs::read(rig.left_out().join("jobscheduler-4242-5_0.profraw")).unwrap(),
        &whole[..200]
    );
    assert_eq!(fs::read(rig.left_out().join("empty.profraw")).unwrap(), b"");
    assert_eq!(fs::read_dir(rig.left_out()).unwrap().count(), 2);
    assert_eq!(rig.calls(), format!("{}{}", rig.asked(), rig.merge()));
}

/// Asserts that a directory whose every profile is cut short is refused, counting them, after
/// they are named and kept, and before any program runs.
#[test]
fn a_directory_of_profiles_all_cut_short_is_refused() {
    let rig = Rig::new("all-cut-short", Toolchain::default());
    rig.put_bytes("a-1-1.profraw", &whole_profile()[..9]);
    let ran = rig.run();
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: left out {}: cut short (9 bytes, shorter than the 152 of a \
             header), written by process 1\n\
             xtask: merge-profiles: every raw profile in {} is cut short (1 of them)\n",
            rig.at("a-1-1.profraw"),
            rig.profiles().display()
        ))
    );
    assert_eq!(rig.calls(), "");
    assert!(rig.left_out().join("a-1-1.profraw").exists());
}

/// Asserts that a directory for the profiles left out that cannot be made (its parent is
/// missing) fails naming it, before any program runs.
#[test]
fn a_left_out_directory_that_cannot_be_made_fails_naming_it() {
    let rig = Rig::new("left-out-unwritable", Toolchain::default());
    rig.put_bytes("a-1-1.profraw", b"");
    let out = rig.dir.join("absent/shard-1.profraw");
    let ran = rig.run_with(&rig.profiles(), &out);
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: left out {}: cut short (empty), written by process 1\n\
             xtask: merge-profiles: cannot write {}.left-out: No such file or directory (os \
             error 2)\n",
            rig.at("a-1-1.profraw"),
            out.display()
        ))
    );
    assert_eq!(rig.calls(), "");
}

/// Asserts that a raw profile that cannot be read fails the run naming it, before any
/// program runs.
#[test]
fn a_profile_that_cannot_be_read_fails_naming_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = Rig::new("unreadable", Toolchain::default());
    rig.put("a-1-1.profraw");
    let path = rig.profiles().join("a-1-1.profraw");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let ran = rig.run();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        ran,
        failed(format!(
            "xtask: merge-profiles: cannot read {}: Permission denied (os error 13)\n",
            path.display()
        ))
    );
    assert_eq!(rig.calls(), "");
}

/// Asserts that when llvm-profdata refuses the merge, what it printed is passed on and each
/// listed profile is given to it alone; the error names every one it refuses with its size,
/// the process that wrote it and what it said, says when a profile grew or went after it was
/// listed, and leaves out none of them. The check's output is removed.
#[test]
fn a_merge_llvm_profdata_refuses_names_each_profile_it_refuses_alone() {
    let rig = Rig::new("refused", Toolchain::default());
    rig.put_bytes("js-31-9_0.profraw", b"refuse");
    rig.put_bytes("js-32-9_0.profraw", b"growing");
    rig.put_bytes("js-33-9_0.profraw", b"vanish");
    rig.put("js-34-9_0.profraw");
    let [a, b, c, d] = ["31", "32", "33", "34"].map(|pid| rig.at(&format!("js-{pid}-9_0.profraw")));
    let ran = rig.run();
    assert_eq!(
        ran,
        failed(format!(
            "warning: {a}: file header is corrupt\n\
             warning: {b}: file header is corrupt\n\
             warning: {c}: file header is corrupt\n\
             error: no profile can be merged\n\
             xtask: merge-profiles: {} failed: exit status: 1; it refuses 3 of the 4 raw \
             profiles alone:\n  \
             {a} (6 bytes, written by process 31): warning: {a}: file header is corrupt / \
             error: no profile can be merged\n  \
             {b} (7 bytes when listed and 17 now, still being written, written by process 32): \
             warning: {b}: file header is corrupt / error: no profile can be merged\n  \
             {c} (6 bytes when listed and gone now, written by process 33): warning: {c}: No \
             such file or directory / error: no profile can be merged\n",
            rig.profdata()
        ))
    );
    assert_eq!(
        rig.calls(),
        format!(
            "{}{}{}{}{}{}",
            rig.asked(),
            rig.merge(),
            rig.alone(&a),
            rig.alone(&b),
            rig.alone(&c),
            rig.alone(&d)
        )
    );
    assert!(!rig.out().exists());
    assert!(!Path::new(&rig.check()).exists());
    assert!(!rig.left_out().exists());
}

/// Asserts that when llvm-profdata refuses the merge but accepts each profile alone, the
/// error says so.
#[test]
fn a_merge_refused_while_each_profile_is_accepted_alone_says_so() {
    let rig = Rig::new("refused-together", Toolchain::default());
    rig.put_bytes("js-41-9_0.profraw", b"listed");
    rig.put("js-42-9_0.profraw");
    let a = rig.at("js-41-9_0.profraw");
    assert_eq!(
        rig.run(),
        failed(format!(
            "warning: {a}: hash mismatch\nerror: no profile can be merged\n\
             xtask: merge-profiles: {} failed: exit status: 1; it accepts each of the 2 raw \
             profiles alone\n",
            rig.profdata()
        ))
    );
    assert!(!Path::new(&rig.check()).exists());
}
