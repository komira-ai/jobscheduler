//! `cargo xtask farm-workers` and `cargo xtask farm-table` end to end: each verb is run as a
//! process in a git repository of its own (tests/support), with `--buck2` naming the stand-in
//! (src/bin/xtask-fake.rs, linked as `buck2`), which answers buck2's audit from files of the
//! working directory and records the calls it got (`buck2.calls`), and `--buildrules` naming the
//! stand-in linked as `buildrules` beside it, which records its own in `buildrules.calls`. Each
//! test reads the verb's exit status and words, those calls, and for a stale table the patch and
//! the job summary it wrote. Nothing here reaches a network or a build farm: the lane's builds on
//! the farm are komira's buck2-farm action's, held to the workflow by
//! tests/apt_workflow/farm_steps.rs.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_testkit::dir::TestDir;
use sha2::Digest as _;
use support::Repo;
use xtask::farm::WORKERS_SET;
use xtask::farm::table::{FRESH, PATCH_ARTIFACT, PATCH_FILE, summary_markdown};

const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// What buck2 prints for a key that is set.
const SET: &str = r#"{"buildrules_re.linux_x86_64_properties":"OSFamily=Linux,ISA=x86-64"}"#;
/// The audit call.
const AUDIT: &str = "audit config buildrules_re.linux_x86_64_properties --style json";
/// The run the handoff names.
const RUN: &str = "42";
/// The repository the handoff names.
const REPO: &str = "komira-ai/jobscheduler";
/// The table of the stand-in, and its one other generated file.
const TABLE: &str = "buildrules.lock.bzl";
const ARCHIVES: &str = "third_party/buildrules/git_archives.tsv";
/// A dependency that gives the workspace a git source.
const GIT_DEP: &str =
    "\n[workspace.dependencies]\nr = { git = \"https://github.com/o/r\", rev = \"0123abc\" }\n";
/// The fix the real command line ends each of its lines with.
const FIX: &str = "run `buildrules sync` and commit the result";

/// A repository whose table is fresh and committed, the stand-ins in a directory beside it, and
/// a directory for the patch and the summary.
struct Rig {
    repo: Repo,
    bin: TestDir,
    out: TestDir,
}

impl Rig {
    /// A fresh rig named `name`, its workspace given a git dependency when `git`, with `answers`
    /// (file name, text) in the repository's working tree for the stand-in buck2.
    fn new(name: &str, git: bool, answers: &[(&str, &str)]) -> Rig {
        let repo = Repo::new(&format!("farm-{name}"));
        let bin = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("cli-farm-bin-{name}"));
        let out = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("cli-farm-out-{name}"));
        for program in ["buck2", "buildrules"] {
            std::fs::hard_link(FAKE, bin.join(program)).expect("link the stand-in");
        }
        let rig = Rig { repo, bin, out };
        if git {
            rig.append_manifest(GIT_DEP);
        }
        rig.buildrules(&rig.repo.dir, "sync");
        std::fs::remove_file(rig.repo.dir.join("buildrules.calls")).expect("calls");
        rig.repo.commit("the table");
        for (file, text) in answers {
            rig.repo.write(file, text);
        }
        rig
    }

    /// Appends `text` to the root manifest.
    fn append_manifest(&self, text: &str) {
        let manifest = std::fs::read_to_string(self.repo.dir.join("Cargo.toml")).expect("read");
        self.repo.write("Cargo.toml", &format!("{manifest}{text}"));
    }

    /// The stand-in buck2.
    fn buck2(&self) -> PathBuf {
        self.bin.join("buck2")
    }

    /// The patch file the handoff names.
    fn patch(&self) -> PathBuf {
        self.out.join("patch").join(PATCH_FILE)
    }

    /// The summary file the handoff names.
    fn summary(&self) -> PathBuf {
        self.out.join("summary.md")
    }

    /// Asks the stand-in buildrules for `verb` in `cwd`: its exit status.
    fn buildrules(&self, cwd: &Path, verb: &str) -> Option<i32> {
        Command::new(self.bin.join("buildrules"))
            .arg(verb)
            .current_dir(cwd)
            .output()
            .expect("run the stand-in")
            .status
            .code()
    }

    /// Asks for a failure of the stand-in buildrules.
    fn defect(&self, defect: &str) {
        std::fs::write(self.bin.join("defect"), defect).expect("defect");
    }

    /// The stand-in buildrules.
    fn tool(&self) -> PathBuf {
        self.bin.join("buildrules")
    }

    /// The table's check in the repository, with the handoff's flags.
    fn farm_table(&self) -> Output {
        self.farm_table_in(&self.repo.dir, &self.patch(), &self.summary())
    }

    /// The table's check in `cwd`, writing a stale table's fix to `patch` and `summary`.
    fn farm_table_in(&self, cwd: &Path, patch: &Path, summary: &Path) -> Output {
        xtask(cwd, &args(&self.buck2(), &self.tool(), patch, summary))
    }

    /// The worker check in the repository.
    fn farm_workers(&self) -> Output {
        xtask(&self.repo.dir, &workers_args(&self.buck2()))
    }
}

/// The path `p` as text.
fn path(p: &Path) -> String {
    p.to_str().expect("utf-8").to_owned()
}

/// The flags of the table's check with the command line `tool`.
fn args(buck2: &Path, tool: &Path, patch: &Path, summary: &Path) -> Vec<String> {
    [
        "farm-table",
        "--buck2",
        &path(buck2),
        "--buildrules",
        &path(tool),
        "--patch",
        &path(patch),
        "--summary",
        &path(summary),
        "--repo",
        REPO,
        "--run-id",
        RUN,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The flags of the worker check.
fn workers_args(buck2: &Path) -> Vec<String> {
    ["farm-workers", "--buck2", &path(buck2)]
        .map(str::to_owned)
        .to_vec()
}

/// `xtask <args>` in `cwd`.
fn xtask(cwd: &Path, args: &[String]) -> Output {
    Command::new(XTASK)
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

/// The calls a stand-in recorded in the file `name` of `dir`, one per line.
fn calls_in(dir: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The calls buck2 got in `dir`.
fn calls(dir: &Path) -> Vec<String> {
    calls_in(dir, "buck2.calls")
}

/// The calls buildrules got in `dir`.
fn tool_calls(dir: &Path) -> Vec<String> {
    calls_in(dir, "buildrules.calls")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The sha256 of `text`, in hex.
fn sha256(text: &str) -> String {
    hex::encode(sha2::Sha256::digest(text))
}

/// The command the summary gives.
fn apply() -> String {
    format!(
        "gh run download {RUN} -R {REPO} -n {PATCH_ARTIFACT} -D buck-out/{PATCH_ARTIFACT}-{RUN} \
         && git apply buck-out/{PATCH_ARTIFACT}-{RUN}/{PATCH_FILE}"
    )
}

/// The answers of a buck2 that gives buildrules the farm's worker property set.
const PASSING: [(&str, &str); 1] = [("buck2-audit.stdout", SET)];

#[test]
fn a_fresh_table_passes_with_status_0_and_hands_over_nothing() {
    // Catches: the check not run with the command line `--buildrules` names, buck2 run by the
    // check (the lane's builds are the workflow's, under the watchdog), a fresh table synced or
    // handed over as a patch, or the verb's line not printed.
    let d = Rig::new("ok", false, &PASSING);
    let o = d.farm_table();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FRESH}\n"));
    assert_eq!(stderr(&o), "");
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
    assert_eq!(tool_calls(&d.repo.dir), ["check"]);
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

/// The stale table of `d` handed over: the verb fails with the stale inputs `inputs` and the
/// changed files `files` in its words and in the summary (after what the summary held), and the
/// patch it wrote, applied to a clone of the stale commit, makes the table fresh and the check
/// pass there.
fn hands_over(d: &Rig, inputs: &[String], files: &[&str]) {
    std::fs::create_dir_all(d.out.path()).expect("out");
    std::fs::write(d.summary(), "earlier\n").expect("summary");
    let o = d.farm_table();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    let files: Vec<String> = files.iter().map(|f| (*f).to_owned()).collect();
    assert_eq!(
        stderr(&o),
        format!(
            "buildrules: wrote {TABLE}\nxtask: the generated build table is stale: {}. \
             buildrules.lock.bzl, buildrules.bzl and third_party/buildrules/ are generated from \
             the Cargo files by `buildrules sync`, which this job ran at the pinned version; what \
             it changed ({}) is the artifact {PATCH_ARTIFACT} of this run, made against this \
             run's merge of the branch into main. Apply it in the branch's checkout, then commit \
             and push (if it does not apply, merge main first): {}\n",
            inputs.join("; "),
            files.join(", "),
            apply()
        )
    );
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
    let buck2 = d.buck2();
    assert_eq!(
        tool_calls(&d.repo.dir),
        [
            "check".to_owned(),
            format!("sync --buck2 {}", buck2.display()),
            "check".to_owned()
        ]
    );
    assert_eq!(
        std::fs::read_to_string(d.summary()).expect("summary"),
        format!("earlier\n{}", summary_markdown(inputs, &files, &apply()))
    );

    // The patch, on the stale commit as the pull request's branch has it.
    let clone = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cli-farm-clone");
    let tree = clone.join("tree");
    d.repo
        .git(&["clone", "--quiet", ".", tree.to_str().expect("utf-8")]);
    assert_eq!(d.buildrules(&tree, "check"), Some(1), "the clone is stale");
    let applied = Command::new("git")
        .arg("apply")
        .arg(d.patch())
        .current_dir(&tree)
        .output()
        .expect("git apply");
    assert!(applied.status.success(), "{applied:?}");
    assert_eq!(
        d.buildrules(&tree, "check"),
        Some(0),
        "the patch makes it fresh"
    );
    let o = d.farm_table_in(&tree, &clone.join("p"), &clone.join("s"));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FRESH}\n"));
    assert!(!clone.join("p").exists());
}

/// The stale line of the stand-in for the file `input`, from the text `was` to the text `now`.
fn changed(input: &str, was: &str, now: &str) -> String {
    format!(
        "{input} changed since `buildrules sync` (recorded sha256 {}, now {})",
        sha256(was),
        sha256(now)
    )
}

#[test]
fn a_changed_manifest_hands_over_a_patch_that_changes_the_table_and_adds_a_file() {
    // Catches: a stale table built (or failed without a fix), sync not run or run without the
    // job's buck2, the second check skipped, a patch that leaves out a file sync made (git diff
    // without the new file in the index), the fix's words (STALE mark and `buildrules sync`
    // advice) left in the inputs, a summary that replaces what the job wrote before, a patch
    // that does not apply to the branch or leaves the table stale, and a patch that takes in a
    // change of the working tree that sync did not make (a tracked file outside the paths sync
    // writes, changed by an earlier step).
    let d = Rig::new("stale-add", false, &PASSING);
    let was = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    d.append_manifest(GIT_DEP);
    d.repo.commit("a git dependency, the table not synced");
    d.repo.write("README.md", "changed by an earlier step\n");
    let now = format!("{was}{GIT_DEP}");
    hands_over(&d, &[changed("Cargo.toml", &was, &now)], &[TABLE, ARCHIVES]);
}

#[test]
fn a_removed_dependency_hands_over_a_patch_that_deletes_a_file() {
    // Catches: a patch that keeps a file sync removed, and one that cannot be applied for it.
    let d = Rig::new("stale-remove", true, &PASSING);
    let was = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    let now = was.replace(GIT_DEP, "");
    d.repo.write("Cargo.toml", &now);
    d.repo
        .commit("the git dependency removed, the table not synced");
    hands_over(&d, &[changed("Cargo.toml", &was, &now)], &[TABLE, ARCHIVES]);
}

#[test]
fn a_generated_file_removed_by_hand_hands_over_a_patch_that_writes_it_again() {
    // Catches: a stale generated file (the inputs unchanged) not taken for a stale table, and a
    // patch that changes only files that were tracked.
    let d = Rig::new("stale-generated", true, &PASSING);
    d.repo.git(&["rm", "--quiet", ARCHIVES]);
    d.repo.commit("a generated file removed by hand");
    hands_over(
        &d,
        &[format!("{ARCHIVES} is not what sync writes")],
        &[ARCHIVES],
    );
}

#[test]
fn two_changed_inputs_are_both_named() {
    // Catches: only the first stale line of check taken as the stale inputs.
    let d = Rig::new("stale-two", false, &PASSING);
    let manifest = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    let lock = std::fs::read_to_string(d.repo.dir.join("Cargo.lock")).expect("read");
    d.repo.write("Cargo.toml", &format!("{manifest}# a\n"));
    d.repo.write("Cargo.lock", &format!("{lock}# b\n"));
    d.repo.commit("both changed");
    hands_over(
        &d,
        &[
            changed("Cargo.toml", &manifest, &format!("{manifest}# a\n")),
            changed("Cargo.lock", &lock, &format!("{lock}# b\n")),
        ],
        &[TABLE],
    );
}

/// Makes the table of `d` stale: a comment in the root manifest, committed.
fn make_stale(d: &Rig) {
    d.append_manifest("# a comment\n");
    d.repo.commit("the manifest changed, the table not synced");
}

/// Asserts that the run `o` of `d` failed with `words` after the log `log`, running no buck2 and
/// leaving no patch and no summary.
fn refused(d: &Rig, o: &Output, log: &str, words: &str) {
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(o), "");
    assert_eq!(stderr(o), format!("{log}xtask: {words}\n"));
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

#[test]
fn a_failed_sync_fails_with_its_output_and_hands_over_nothing() {
    // Catches: a failed sync taken for a fix, its words hidden, or a patch of a half-written
    // table handed over.
    let d = Rig::new("sync-fails", false, &PASSING);
    make_stale(&d);
    d.defect("sync-fails");
    let o = d.farm_table();
    refused(
        &d,
        &o,
        "buildrules: sync: CargoFailed\n",
        "the generated build table is stale, and `buildrules sync` failed (exit status: 1); its \
         output is above",
    );
    assert_eq!(tool_calls(&d.repo.dir).len(), 2);
}

#[test]
fn a_sync_that_leaves_the_table_stale_fails_with_what_check_still_says() {
    // Catches: a patch handed over without checking that it makes the table fresh.
    let d = Rig::new("sync-no-fix", false, &PASSING);
    let was = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    make_stale(&d);
    d.defect("sync-writes-nothing");
    let o = d.farm_table();
    let line = changed("Cargo.toml", &was, &format!("{was}# a comment\n"));
    refused(
        &d,
        &o,
        &format!("buildrules: wrote {TABLE}\n"),
        &format!(
            "`buildrules sync` ran, and `buildrules check` still fails (exit status: 1): \
             buildrules: STALE: {line}: {FIX}"
        ),
    );
    assert_eq!(tool_calls(&d.repo.dir).len(), 3);
}

#[test]
fn a_missing_generated_file_without_a_stale_mark_hands_over_a_patch_that_adds_it() {
    // Catches: a problem check prints without `STALE:` (a generated file deleted) taken for one
    // sync cannot fix, and a patch that leaves out a new file at the repository's root.
    let d = Rig::new("no-table", false, &PASSING);
    d.repo.git(&["rm", "--quiet", TABLE]);
    d.repo.commit("no table");
    hands_over(&d, &[format!("{TABLE} is missing")], &[TABLE]);
}

#[test]
fn a_check_that_cannot_judge_the_table_fails_with_its_words_and_runs_no_sync() {
    // Catches: a check that could not run (a usage error, status 2) taken for a fresh table or a
    // stale one, and check's words hidden.
    let d = Rig::new("check-usage", false, &PASSING);
    d.defect("check-usage");
    let o = d.farm_table();
    refused(
        &d,
        &o,
        "",
        "`buildrules check` could not judge the table (exit status: 2): buildrules: check: \
         unknown argument --nope",
    );
    assert_eq!(tool_calls(&d.repo.dir), ["check"]);
}

#[test]
fn a_command_line_that_cannot_start_fails_before_the_check() {
    // Catches: a `--buildrules` that names no program (its build in the step before failed to
    // write it) taken for a passing check.
    let d = Rig::new("tool-missing", false, &PASSING);
    let o = xtask(
        &d.repo.dir,
        &args(
            &d.buck2(),
            Path::new("/nonexistent/buildrules"),
            &d.patch(),
            &d.summary(),
        ),
    );
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buildrules\": "),
        "{}",
        stderr(&o)
    );
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

#[test]
fn a_patch_that_git_cannot_make_fails_and_hands_over_nothing() {
    // Catches: a failed git taken for an empty patch, and its words hidden.
    let d = Rig::new("git-fails", false, &PASSING);
    make_stale(&d);
    let o = Command::new(XTASK)
        .current_dir(&d.repo.dir)
        .args(args(&d.buck2(), &d.tool(), &d.patch(), &d.summary()))
        .env("GIT_DIR", d.out.join("no-repository"))
        .output()
        .expect("run xtask");
    assert_eq!(o.status.code(), Some(1));
    let words = stderr(&o);
    let start = "buildrules: wrote buildrules.lock.bzl\nxtask: `git ls-files --others \
                 --exclude-standard -z -- BUCK Cargo.lock buildrules.bzl buildrules.lock.bzl \
                 third_party/buildrules toolchains` failed (exit status: 128): fatal: ";
    assert!(words.starts_with(start), "{words}");
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

#[test]
fn a_patch_or_summary_that_cannot_be_written_fails() {
    // Catches: a fix the job could not write reported as handed over.
    let d = Rig::new("patch-unwritable", false, &PASSING);
    make_stale(&d);
    std::fs::write(d.out.join("patch"), "a file, not a directory").expect("block");
    let o = d.farm_table();
    assert_eq!(o.status.code(), Some(1));
    let patch = d.patch();
    assert_eq!(
        stderr(&o),
        format!(
            "buildrules: wrote {TABLE}\nxtask: cannot write {patch:?}: File exists (os error 17)\n"
        )
    );
    assert!(!d.summary().exists());

    let d = Rig::new("summary-unwritable", false, &PASSING);
    make_stale(&d);
    let o = d.farm_table_in(&d.repo.dir, &d.patch(), d.out.path());
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        format!(
            "buildrules: wrote {TABLE}\nxtask: cannot write {:?}: Is a directory (os error 21)\n",
            d.out.path()
        )
    );
    assert!(d.patch().exists());
}

#[test]
fn a_worker_property_set_passes_with_status_0_and_one_audit() {
    // Catches: a set key refused, the verb's line not printed, or a call of buck2 beyond its
    // audit (a build, which belongs to the workflow's watched steps).
    let d = Rig::new("workers-set", false, &PASSING);
    let o = d.farm_workers();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{WORKERS_SET}\n"));
    assert_eq!(stderr(&o), "");
    assert_eq!(calls(&d.repo.dir), [AUDIT]);
    assert_eq!(tool_calls(&d.repo.dir), Vec::<String>::new());
}

#[test]
fn no_worker_property_set_fails_with_status_1() {
    // Catches: a lane let on to its builds with no farm to run on (buck2 would then fail at
    // analysis, or a later change could fall back to this machine): the key unset (buck2's `{}`,
    // the stand-in's answer when it has no file), set to nothing, and set to blanks.
    for (name, audit) in [
        ("unset", None),
        (
            "empty",
            Some(r#"{"buildrules_re.linux_x86_64_properties":""}"#),
        ),
        (
            "blank",
            Some(r#"{"buildrules_re.linux_x86_64_properties":"  "}"#),
        ),
    ] {
        let answers: Vec<(&str, &str)> = audit
            .map(|text| ("buck2-audit.stdout", text))
            .into_iter()
            .collect();
        let d = Rig::new(&format!("workers-{name}"), false, &answers);
        let o = d.farm_workers();
        assert_eq!(o.status.code(), Some(1), "{name}");
        assert_eq!(stdout(&o), "", "{name}");
        assert_eq!(
            stderr(&o),
            "xtask: buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no \
             [komira_re] linux_x86_64_properties, so buck2 has no farm to build on\n",
            "{name}"
        );
        assert_eq!(calls(&d.repo.dir), [AUDIT], "{name}");
    }
}

#[test]
fn a_failed_audit_fails_with_status_1() {
    // Catches: an audit that failed (no .buckconfig, a broken cell) read as a set key, though it
    // printed one.
    let d = Rig::new(
        "audit-fails",
        false,
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-audit.stderr", "no .buckconfig\n"),
            ("buck2-audit.status", "3"),
        ],
    );
    let o = d.farm_workers();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert_eq!(
        stderr(&o),
        "xtask: `buck2 audit config` failed (exit status: 3): no .buckconfig\n"
    );
    assert_eq!(calls(&d.repo.dir), [AUDIT]);
}

/// Asserts that `all`, a verb's whole command line, is refused as a usage error (status 2,
/// naming a required flag) with each flag and its value left out in turn.
fn every_flag_is_required(dir: &Path, all: &[String]) {
    for flag in (1..all.len()).step_by(2) {
        let mut without = all.to_vec();
        without.drain(flag..flag + 2);
        let o = xtask(dir, &without);
        assert_eq!(o.status.code(), Some(2), "{without:?}");
        assert!(stderr(&o).contains("required"), "{}", stderr(&o));
    }
}

#[test]
fn a_buck2_that_cannot_start_fails_with_status_1_and_the_flags_are_required() {
    // Catches: a missing buck2 taken for a set worker property set, and either verb run without
    // its buck2, its command line or any of the handoff's flags.
    let d = Rig::new("no-buck2", false, &[]);
    let o = xtask(&d.repo.dir, &workers_args(Path::new("/nonexistent/buck2")));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buck2\": "),
        "{}",
        stderr(&o)
    );
    every_flag_is_required(&d.repo.dir, &workers_args(&d.buck2()));
    every_flag_is_required(
        &d.repo.dir,
        &args(&d.buck2(), &d.tool(), &d.patch(), &d.summary()),
    );
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
    assert_eq!(tool_calls(&d.repo.dir), Vec::<String>::new());
}
