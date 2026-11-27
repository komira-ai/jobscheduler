//! `cargo xtask coverage-shard`: the plan's assignment and the arguments of each run (through the
//! library), the checked-in plan `.config/coverage-shards.toml`, and the verb run as the built
//! `xtask` program with a stand-in program that logs its arguments.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;
use xtask::coverage_shard::{Plan, Shard, assign, runs};

/// A plan of two shards and four slices (50 seconds each); the tests are written out of order.
/// Weights times 4: `a` 400, `b` 200 (a tie with each slice, which it wins), `c` 40.
const SMALL: &str = r#"
shards = 2
slices = 4
light-seconds = 200

[[heavy]]
binary-id = "b::y"
test = "c"
seconds = 10

[[heavy]]
binary-id = "b::x"
test = "b"
seconds = 50

[[heavy]]
binary-id = "b::x"
test = "a"
seconds = 100
"#;

fn plan(text: &str) -> Plan {
    Plan::parse(Path::new("plan.toml"), text).unwrap()
}

fn refused(text: &str) -> String {
    Plan::parse(Path::new("plan.toml"), text)
        .unwrap_err()
        .to_string()
}

/// The checked-in plan.
fn real_plan_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.config/coverage-shards.toml")
}

/// Asserts that every named test and every slice from 1 to `slices` is in exactly one shard, and
/// that there are `shards` shards.
fn assert_dealt_once(plan: &Plan) {
    let shards = assign(plan);
    assert_eq!(shards.len(), usize::try_from(plan.shards).unwrap());
    let mut heavy: Vec<usize> = shards.iter().flat_map(|s| s.heavy.clone()).collect();
    heavy.sort_unstable();
    assert_eq!(heavy, (0..plan.heavy.len()).collect::<Vec<_>>());
    let mut slices: Vec<u32> = shards.iter().flat_map(|s| s.slices.clone()).collect();
    slices.sort_unstable();
    assert_eq!(slices, (1..=plan.slices).collect::<Vec<_>>());
}

/// Asserts that the named tests are sorted by binary id and name, whatever order the file has,
/// and that the work is dealt heaviest first, a tie to the named test and then the lowest shard:
/// `a` to shard 1, `b` to shard 2, the slices to the shard with the least load (2, 1, 2, 1), and
/// `c` last to shard 2.
#[test]
fn the_small_plan_is_dealt_heaviest_first_with_ties_to_the_lowest_shard() {
    let plan = plan(SMALL);
    let names: Vec<_> = plan
        .heavy
        .iter()
        .map(|h| (&*h.binary_id, &*h.test))
        .collect();
    assert_eq!(names, [("b::x", "a"), ("b::x", "b"), ("b::y", "c")]);
    assert_eq!(
        assign(&plan),
        vec![
            Shard {
                heavy: vec![0],
                slices: vec![2, 4],
            },
            Shard {
                heavy: vec![1, 2],
                slices: vec![1, 3],
            },
        ]
    );
}

/// Asserts that a plan whose items all weigh the same is dealt to shards 1, 2, 3 in turn, and
/// that the weights are compared exactly: with a named test of 4 seconds (12 times the slice
/// count of 3) and slices of 10/3 seconds each, the named test goes to shard 1, slices 1 and 2 to
/// shard 2 (10, then 20, both below shard 1's 12 at the time they are given) and slice 3 to
/// shard 1.
#[test]
fn equal_weights_go_to_the_lowest_shard_and_weights_are_exact() {
    let even = plan("shards = 3\nslices = 3\nlight-seconds = 9\n");
    let slices: Vec<_> = assign(&even).iter().map(|s| s.slices.clone()).collect();
    assert_eq!(slices, [vec![1], vec![2], vec![3]]);

    let exact = plan(
        "shards = 2\nslices = 3\nlight-seconds = 10\n\
         [[heavy]]\nbinary-id = \"p::b\"\ntest = \"t\"\nseconds = 4\n",
    );
    assert_eq!(
        assign(&exact),
        vec![
            Shard {
                heavy: vec![0],
                slices: vec![3],
            },
            Shard {
                heavy: vec![],
                slices: vec![1, 2],
            },
        ]
    );
}

/// Asserts the exact arguments appended for a shard with named tests (a run of just those, then
/// a run of each slice with the complement of every named test of the plan) and for one with
/// none of its own.
#[test]
fn a_shard_runs_its_named_tests_then_each_slice_of_the_rest() {
    let plan = plan(SMALL);
    let shards = assign(&plan);
    let all = "(binary_id(=b::x) & test(=a)) | (binary_id(=b::x) & test(=b)) | \
               (binary_id(=b::y) & test(=c))";
    let rest = format!("not ({all})");
    let strings = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        runs(&plan, &shards[1]),
        vec![
            strings(&[
                "-E",
                "(binary_id(=b::x) & test(=b)) | (binary_id(=b::y) & test(=c))"
            ]),
            strings(&["-E", &rest, "--partition", "slice:1/4"]),
            strings(&["-E", &rest, "--partition", "slice:3/4"]),
        ]
    );
    let lone = Shard {
        heavy: vec![],
        slices: vec![2],
    };
    assert_eq!(
        runs(&plan, &lone),
        vec![strings(&["-E", &rest, "--partition", "slice:2/4"])]
    );
}

/// Asserts that a plan that names no test runs every slice with the filterset `all()`.
#[test]
fn a_plan_naming_no_test_runs_each_slice_of_everything() {
    let plan = plan("shards = 2\nslices = 2\nlight-seconds = 10\n");
    let shards = assign(&plan);
    let all = |j: &str| {
        vec![
            "-E".to_owned(),
            "all()".to_owned(),
            "--partition".to_owned(),
            format!("slice:{j}/2"),
        ]
    };
    assert_eq!(runs(&plan, &shards[0]), vec![all("1")]);
    assert_eq!(runs(&plan, &shards[1]), vec![all("2")]);
}

/// Asserts that every named test and every slice is in exactly one shard, for small plans of
/// each shape: more slices than shards, as many, one shard, and named tests fewer and more than
/// the shards.
#[test]
fn every_named_test_and_slice_is_in_exactly_one_shard_for_small_plans() {
    for text in [
        SMALL,
        "shards = 1\nslices = 1\nlight-seconds = 0\n",
        "shards = 5\nslices = 5\nlight-seconds = 7\n",
        "shards = 2\nslices = 7\nlight-seconds = 70\n\
         [[heavy]]\nbinary-id = \"p::a\"\ntest = \"x\"\nseconds = 500\n",
    ] {
        assert_dealt_once(&plan(text));
    }
}

/// Asserts that the checked-in plan is a plan of 12 shards and 48 slices, which `ci.yml`'s
/// `coverage-shard` job runs, naming 26 tests of more than 100 seconds each, and that every
/// named test and every slice is in exactly one of its shards.
#[test]
fn the_checked_in_plan_deals_every_test_and_slice_to_exactly_one_shard() {
    let plan = Plan::load(&real_plan_path()).unwrap();
    assert_eq!((plan.shards, plan.slices), (12, 48));
    assert_eq!(plan.light_seconds, 4983);
    assert_eq!(plan.heavy.len(), 26);
    assert!(plan.heavy.iter().all(|h| h.seconds > 100));
    assert_dealt_once(&plan);
    let all = assign(&plan);
    let busiest = all
        .iter()
        .map(|s| {
            let named: u64 = s.heavy.iter().map(|&i| plan.heavy[i].seconds).sum();
            named * u64::from(plan.slices)
                + u64::try_from(s.slices.len()).unwrap() * plan.light_seconds
        })
        .max()
        .unwrap();
    // No shard is dealt more than the longest named test, 1562 s, plus a slice of the light tests.
    assert!(busiest <= (1562 + 104) * u64::from(plan.slices));
}

/// Asserts that every test the checked-in plan names is a test function of the source of its
/// binary: `crates/<package>/tests/<binary>.rs` or `crates/<package>/tests/<binary>/main.rs`.
#[test]
fn every_test_the_checked_in_plan_names_exists_in_its_binary() {
    let plan = Plan::load(&real_plan_path()).unwrap();
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for h in &plan.heavy {
        let (package, binary) = h.binary_id.split_once("::").unwrap();
        let tests = crates.join(package).join("tests");
        let source = fs::read_to_string(tests.join(format!("{binary}.rs")))
            .or_else(|_| fs::read_to_string(tests.join(binary).join("main.rs")))
            .unwrap_or_else(|e| panic!("no source for {}: {e}", h.binary_id));
        assert!(
            source.contains(&format!("fn {}(", h.test)),
            "{} has no test {}",
            h.binary_id,
            h.test
        );
    }
}

/// Asserts the exact message of each plan that is refused: no shards, no slices, fewer slices
/// than shards, a named test with no seconds, a test named twice (with the same seconds and with
/// others), an unknown key (at the top and in a test), a missing key and text that is not TOML.
#[test]
fn a_plan_that_is_no_plan_is_refused() {
    let test = |seconds: u32, name: &str| {
        format!("[[heavy]]\nbinary-id = \"p::b\"\ntest = \"{name}\"\nseconds = {seconds}\n")
    };
    let head = "shards = 2\nslices = 2\nlight-seconds = 1\n";
    assert_eq!(
        refused("shards = 0\nslices = 2\nlight-seconds = 1\n"),
        "the plan plan.toml is invalid: shards is 0"
    );
    assert_eq!(
        refused("shards = 2\nslices = 0\nlight-seconds = 1\n"),
        "the plan plan.toml is invalid: slices is 0"
    );
    assert_eq!(
        refused("shards = 3\nslices = 2\nlight-seconds = 1\n"),
        "the plan plan.toml is invalid: slices is 2, fewer than its 3 shards"
    );
    assert_eq!(
        refused(&format!("{head}{}", test(0, "t"))),
        "the plan plan.toml is invalid: p::b t has seconds = 0"
    );
    let twice = "the plan plan.toml is invalid: p::b t is named twice";
    assert_eq!(refused(&format!("{head}{0}{0}", test(5, "t"))), twice);
    assert_eq!(
        refused(&format!(
            "{head}{}{}{}",
            test(5, "t"),
            test(6, "u"),
            test(7, "t")
        )),
        twice
    );
    // The same name in another binary is another test.
    let other = format!(
        "{head}{}[[heavy]]\nbinary-id = \"p::c\"\ntest = \"t\"\nseconds = 5\n",
        test(5, "t")
    );
    assert_eq!(plan(&other).heavy.len(), 2);

    for bad in [
        format!("{head}bogus = 1\n"),
        format!("{head}[[heavy]]\nbinary-id = \"p::b\"\ntest = \"t\"\nseconds = 1\nbogus = 1\n"),
    ] {
        let message = refused(&bad);
        assert!(
            message.starts_with("cannot parse the plan plan.toml: ")
                && message.contains("unknown field `bogus`"),
            "{message}"
        );
    }
    for bad in ["shards = 2\nslices = 2\n", "this is not toml"] {
        let message = refused(bad);
        assert!(
            message.starts_with("cannot parse the plan plan.toml: "),
            "{message}"
        );
    }
}

/// Asserts that a plan file that does not exist is refused with its path and the reason.
#[test]
fn a_missing_plan_file_is_refused() {
    let rig = Rig::new("missing-plan");
    let path = rig.dir.join("none.toml");
    let message = Plan::load(&path).unwrap_err().to_string();
    assert!(
        message.starts_with(&format!("cannot read the plan {}: ", path.display())),
        "{message}"
    );
}

/// A scratch directory of its own for each test: the plan file, the stand-in program and its log.
struct Rig {
    dir: PathBuf,
    /// Removes the directory when the rig is dropped.
    _scratch: js_testkit::dir::TestDir,
}

/// What one run of the verb did.
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("coverage-shard-{name}"),
        );
        Rig {
            dir: scratch.path().to_path_buf(),
            _scratch: scratch,
        }
    }

    /// Writes the plan `text` as `plan.toml`.
    fn plan(&self, text: &str) -> PathBuf {
        let path = self.dir.join("plan.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// Installs the stand-in program `prog`: it appends `==` and its arguments, one per line, to
    /// `calls`, and ends with status 3 when its arguments, joined by spaces, contain `fail_on`
    /// (never when that is empty), else 0.
    fn program(&self, fail_on: &str) -> PathBuf {
        let path = self.dir.join("prog");
        let test = match fail_on.is_empty() {
            true => "false".to_owned(),
            false => format!("case \"$*\" in *'{fail_on}'*) true;; *) false;; esac"),
        };
        install_script(
            &path,
            &format!(
                "#!/bin/sh\n{{ echo '=='; printf '%s\\n' \"$@\"; }} >> \"{calls}\"\n\
                 if {test}; then exit 3; fi\nexit 0\n",
                calls = self.dir.join("calls").display(),
            ),
            "755",
        );
        path
    }

    /// The log of the stand-in program: each run as `==` and its arguments; empty when none ran.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
    }

    fn run(&self, plan: &Path, shard: &str, of: &str, command: &[&OsStr]) -> Ran {
        let ran = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["coverage-shard", "--plan"])
            .arg(plan)
            .args(["--shard", shard, "--of", of, "--"])
            .args(command)
            .output()
            .unwrap();
        Ran {
            status: ran.status.code().unwrap(),
            stdout: String::from_utf8(ran.stdout).unwrap(),
            stderr: String::from_utf8(ran.stderr).unwrap(),
        }
    }
}

/// The arguments the small plan appends for shard 2, as the stand-in program logs them.
const SHARD_2_CALLS: &str = "==\n--fixed\ntwo words\n\nit's\n-E\n\
    (binary_id(=b::x) & test(=b)) | (binary_id(=b::y) & test(=c))\n\
    ==\n--fixed\ntwo words\n\nit's\n-E\n\
    not ((binary_id(=b::x) & test(=a)) | (binary_id(=b::x) & test(=b)) | (binary_id(=b::y) & test(=c)))\n\
    --partition\nslice:1/4\n\
    ==\n--fixed\ntwo words\n\nit's\n-E\n\
    not ((binary_id(=b::x) & test(=a)) | (binary_id(=b::x) & test(=b)) | (binary_id(=b::y) & test(=c)))\n\
    --partition\nslice:3/4\n";

/// The command: the stand-in program and its own arguments, which every run starts with. One is
/// empty and one has a quote and a space in it.
fn command(prog: &Path) -> Vec<&OsStr> {
    vec![
        prog.as_os_str(),
        OsStr::new("--fixed"),
        OsStr::new("two words"),
        OsStr::new(""),
        OsStr::new("it's"),
    ]
}

/// Asserts that the verb runs the program once for shard 2's named tests and once for each of its
/// slices, each time with the program's own arguments (kept exactly, an empty one and one with a
/// quote too) and then the run's, prints each command line before its run (quoted as a shell
/// would need), and ends with status 0 and a line that counts the runs.
#[test]
fn the_verb_runs_the_program_once_per_group_with_its_arguments_appended() {
    let rig = Rig::new("runs");
    let plan = rig.plan(SMALL);
    let prog = rig.program("");
    let ran = rig.run(&plan, "2", "2", &command(&prog));
    assert_eq!(ran.status, 0, "{}", ran.stderr);
    assert_eq!(ran.stderr, "");
    assert_eq!(rig.calls(), SHARD_2_CALLS);
    let lines: Vec<_> = ran.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{}", ran.stdout);
    let own = "(binary_id(=b::x) & test(=b)) | (binary_id(=b::y) & test(=c))";
    let shown = format!("prog --fixed 'two words' '' 'it'\\''s' -E '{own}'");
    assert!(lines[0].starts_with("+ "), "{}", lines[0]);
    assert!(lines[0].ends_with(&shown), "{}", lines[0]);
    assert!(lines[1].ends_with("--partition slice:1/4"), "{}", lines[1]);
    assert!(lines[2].ends_with("--partition slice:3/4"), "{}", lines[2]);
    assert_eq!(lines[3], "shard 2 of 2: 3 runs passed");
}

/// Asserts that shard 1 of the small plan, whose named test is a different one, runs its own
/// named test first and its slices after.
#[test]
fn another_shard_runs_its_own_named_tests() {
    let rig = Rig::new("shard-one");
    let plan = rig.plan(SMALL);
    let prog = rig.program("");
    let ran = rig.run(&plan, "1", "2", &[prog.as_os_str()]);
    assert_eq!(ran.status, 0, "{}", ran.stderr);
    let calls = rig.calls();
    let runs: Vec<_> = calls.split("==\n").skip(1).collect();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0], "-E\n(binary_id(=b::x) & test(=a))\n");
    assert!(runs[1].ends_with("--partition\nslice:2/4\n"));
    assert!(runs[2].ends_with("--partition\nslice:4/4\n"));
}

/// Asserts that a run that fails does not stop the later ones, and that the verb then exits 1
/// naming the failed run and how it ended, and not the runs that passed.
#[test]
fn a_failing_run_fails_the_verb_after_the_later_runs_started() {
    let rig = Rig::new("fails");
    let plan = rig.plan(SMALL);
    let prog = rig.program("slice:1/4");
    let ran = rig.run(&plan, "2", "2", &command(&prog));
    assert_eq!(ran.status, 1, "{}", ran.stderr);
    assert_eq!(rig.calls(), SHARD_2_CALLS);
    assert!(
        ran.stderr
            .starts_with("xtask: coverage-shard: shard 2 of 2: 1 of 3 runs failed: `")
    );
    assert!(
        ran.stderr
            .contains("--partition slice:1/4` ended with exit status: 3\n")
    );
    assert!(!ran.stderr.contains("slice:3/4"), "{}", ran.stderr);
    assert!(!ran.stdout.contains("runs passed"));
}

/// Asserts that every run that fails is named, when all of them do.
#[test]
fn every_failed_run_is_named() {
    let rig = Rig::new("all-fail");
    let plan = rig.plan(SMALL);
    let prog = rig.program("-E");
    let ran = rig.run(&plan, "2", "2", &command(&prog));
    assert_eq!(ran.status, 1);
    assert!(
        ran.stderr.contains("3 of 3 runs failed: "),
        "{}",
        ran.stderr
    );
    assert_eq!(ran.stderr.matches("ended with exit status: 3").count(), 3);
    assert_eq!(ran.stderr.matches("; ").count(), 2);
}

/// Asserts that a program that cannot be started is reported for each of the runs, and fails the
/// verb.
#[test]
fn a_program_that_cannot_be_started_is_reported() {
    let rig = Rig::new("no-program");
    let plan = rig.plan(SMALL);
    let missing = rig.dir.join("missing");
    let ran = rig.run(&plan, "2", "2", &[missing.as_os_str()]);
    assert_eq!(ran.status, 1);
    assert!(
        ran.stderr.contains("3 of 3 runs failed: "),
        "{}",
        ran.stderr
    );
    assert_eq!(ran.stderr.matches("could not be started: ").count(), 3);
    assert_eq!(rig.calls(), "");
}

/// Asserts the exit status and exact message of each flag that does not fit the plan: `--of`
/// other than the plan's shards, `--shard` 0 and above the shards, and an empty program (all
/// usage errors, status 2, before any run).
#[test]
fn flags_that_do_not_fit_the_plan_are_usage_errors() {
    let rig = Rig::new("usage");
    let plan = rig.plan(SMALL);
    let prog = rig.program("");
    let command = [prog.as_os_str()];
    let ran = rig.run(&plan, "1", "3", &command);
    assert_eq!(ran.status, 2);
    assert_eq!(
        ran.stderr,
        format!(
            "xtask: --of 3 differs from the 2 shards of the plan {}\n",
            plan.display()
        )
    );
    for shard in ["0", "3"] {
        let ran = rig.run(&plan, shard, "2", &command);
        assert_eq!(ran.status, 2);
        assert_eq!(
            ran.stderr,
            format!("xtask: --shard {shard} is not from 1 to the plan's 2 shards\n")
        );
    }
    let ran = rig.run(&plan, "1", "2", &[OsStr::new("")]);
    assert_eq!(ran.status, 2);
    assert_eq!(
        ran.stderr,
        "xtask: coverage-shard needs a program after `--`\n"
    );
    assert_eq!(rig.calls(), "");
}

/// Asserts that a plan that is refused ends the verb with status 1 and its message, and a plan
/// file that is missing too, and that no run starts.
#[test]
fn a_refused_plan_ends_the_verb_before_any_run() {
    let rig = Rig::new("refused");
    let prog = rig.program("");
    let command = [prog.as_os_str()];
    let plan = rig.plan("shards = 0\nslices = 2\nlight-seconds = 1\n");
    let ran = rig.run(&plan, "1", "1", &command);
    assert_eq!(ran.status, 1);
    assert_eq!(
        ran.stderr,
        format!(
            "xtask: coverage-shard: the plan {} is invalid: shards is 0\n",
            plan.display()
        )
    );
    let none = rig.dir.join("none.toml");
    let ran = rig.run(&none, "1", "1", &command);
    assert_eq!(ran.status, 1);
    assert!(
        ran.stderr.starts_with(&format!(
            "xtask: coverage-shard: cannot read the plan {}",
            none.display()
        )),
        "{}",
        ran.stderr
    );
    assert_eq!(rig.calls(), "");
}
