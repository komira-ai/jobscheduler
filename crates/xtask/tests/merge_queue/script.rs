//! The held script `tools/held/merge-queue/merge_queue.sh`, run by `bash` with a stand-in `gh`
//! first on `PATH`. The stand-in logs each call's arguments, answers the reads from files of the
//! test (the repository, ci.yml on main, the rulesets and the rules of main), applies a settings
//! call to its repository file, keeps the body a ruleset call sends and answers it back with an
//! id, and fails a call or answers wrongly when the test tells it to. So each test pins the
//! calls the script makes, that the ruleset it writes is the one of `docs/ci.md`, that a dry run
//! is the default and writes nothing, and what it refuses before any write. Nothing reaches
//! GitHub.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;
use serde_json::{Value, json};

use crate::check::ruleset;
use crate::read;

/// The held script, from the repository root.
const SCRIPT: &str = "tools/held/merge-queue/merge_queue.sh";

/// The stand-in `gh`. Its files are in `$FAKE_DIR`; a file `fail-patch` or `fail-write` makes
/// that call fail, `ignore-patch` makes a settings call change nothing, and `drop-queue` keeps
/// a written ruleset without its merge queue rule.
const GH: &str = r##"#!/bin/sh
d="$FAKE_DIR"
printf '%s\n' "$*" >> "$d/gh.log"
case "$*" in
'auth status') [ ! -f "$d/logged-out" ] || exit 1 ;;
'api --method PATCH '*)
  [ ! -f "$d/fail-patch" ] || { echo 'gh: Validation Failed (HTTP 422)' >&2; exit 1; }
  [ ! -f "$d/ignore-patch" ] || exit 0
  for a in "$@"; do
    case $a in
    squash_merge_commit_title=*) t=${a#*=} ;;
    squash_merge_commit_message=*) m=${a#*=} ;;
    esac
  done
  jq --arg t "$t" --arg m "$m" '.squash_merge_commit_title = $t | .squash_merge_commit_message = $m' "$d/repo.json" > "$d/repo.next"
  mv "$d/repo.next" "$d/repo.json" ;;
'api --method POST '*|'api --method PUT '*)
  cat > "$d/sent.json"
  [ ! -f "$d/fail-write" ] || { echo 'gh: Resource not accessible by integration (HTTP 403)' >&2; exit 1; }
  id=42
  [ "$3" = POST ] || id=${4##*/}
  if [ -f "$d/drop-queue" ]; then
    jq --argjson id "$id" '. + {id: $id} | .rules |= map(select(.type != "merge_queue"))' "$d/sent.json" > "$d/ruleset-$id.json"
  else
    jq --argjson id "$id" '. + {id: $id}' "$d/sent.json" > "$d/ruleset-$id.json"
  fi
  cat "$d/ruleset-$id.json" ;;
'api -H Accept: application/vnd.github.raw+json repos/komira-ai/jobscheduler/contents/.github/workflows/ci.yml?ref=main') cat "$d/ci.yml" ;;
'api repos/komira-ai/jobscheduler') cat "$d/repo.json" ;;
'api repos/komira-ai/jobscheduler/rulesets') cat "$d/rulesets.json" ;;
'api repos/komira-ai/jobscheduler/rules/branches/main') cat "$d/rules.json" ;;
'api repos/komira-ai/jobscheduler/rulesets/'*) cat "$d/ruleset-${2##*/}.json" ;;
*) echo "unexpected: gh $*" >&2; exit 9 ;;
esac
"##;

/// The settings call the script makes.
const PATCH: &str = "api --method PATCH repos/komira-ai/jobscheduler -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=BLANK --silent";
/// The call that creates the ruleset.
const POST: &str = "api --method POST repos/komira-ai/jobscheduler/rulesets --input -";
/// The back-out of the settings from the repository's defaults.
const BACK_SETTINGS: &str = "gh api --method PATCH repos/komira-ai/jobscheduler -f squash_merge_commit_title=COMMIT_OR_PR_TITLE -f squash_merge_commit_message=COMMIT_MESSAGES";
/// The back-out of a ruleset the script created.
const BACK_CREATED: &str = "gh api --method DELETE repos/komira-ai/jobscheduler/rulesets/42";
/// The last line of an apply that passed.
const DONE: &str =
    "DONE: main merges through the merge queue (ruleset 42), the required check is all-passed";

/// The repository's root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

/// The ruleset `docs/ci.md` gives.
fn documented() -> Value {
    ruleset(&read("docs/ci.md")).expect("the ruleset of docs/ci.md")
}

/// The repository as `GET /repos/{repo}` answers, with its squash settings.
fn repo(visibility: &str, title: &str, message: &str) -> String {
    json!({
        "full_name": "komira-ai/jobscheduler",
        "visibility": visibility,
        "squash_merge_commit_title": title,
        "squash_merge_commit_message": message,
    })
    .to_string()
}

/// A scratch directory of its own for each test (outside the checkout), holding the stand-in
/// `gh` in `bin/` and its answers.
struct Rig {
    dir: PathBuf,
    _scratch: js_testkit::dir::TestDir,
}

/// What one run of the script did.
#[derive(Debug)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
    /// Each call of the stand-in `gh`, its arguments joined by spaces.
    gh: Vec<String>,
}

impl Ran {
    /// The calls that write.
    fn writes(&self) -> Vec<&str> {
        self.gh
            .iter()
            .map(String::as_str)
            .filter(|c| c.contains("--method"))
            .collect()
    }

    /// The lines of the output.
    fn lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }

    /// The lines after the line `header`, up to the next line that starts with `==` or the
    /// `DONE:` line.
    fn section(&self, header: &str) -> Vec<&str> {
        let lines = self.lines();
        let at = lines
            .iter()
            .position(|l| *l == header)
            .unwrap_or_else(|| panic!("no {header:?} in {}", self.stdout));
        lines[at + 1..]
            .iter()
            .take_while(|l| !l.starts_with("==") && !l.starts_with("DONE:"))
            .copied()
            .collect()
    }
}

impl Rig {
    /// A public repository with GitHub's default squash settings, the real `ci.yml`, no
    /// ruleset and no rule on main.
    fn new(name: &str) -> Rig {
        let scratch =
            js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("mq-{name}"));
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).expect("bin");
        install_script(&dir.join("bin/gh"), GH, "755");
        let rig = Rig {
            dir,
            _scratch: scratch,
        };
        rig.set(
            "repo.json",
            &repo("public", "COMMIT_OR_PR_TITLE", "COMMIT_MESSAGES"),
        );
        rig.set("ci.yml", &read(".github/workflows/ci.yml"));
        rig.set("rulesets.json", "[]");
        rig.set("rules.json", "[]");
        rig
    }

    /// Writes a file of the stand-in.
    fn set(&self, file: &str, text: &str) {
        fs::write(self.dir.join(file), text).expect("write");
    }

    /// A ruleset `main` with id 7: `body` with what GitHub adds to a read.
    fn ruleset_7(&self, mut body: Value) {
        body["id"] = json!(7);
        body["source_type"] = json!("Repository");
        body["source"] = json!("komira-ai/jobscheduler");
        body["current_user_can_bypass"] = json!("never");
        body["node_id"] = json!("RRS_7");
        self.set(
            "rulesets.json",
            &json!([{ "id": 7, "name": "main", "target": "branch", "source_type": "Repository" }])
                .to_string(),
        );
        self.set("ruleset-7.json", &body.to_string());
    }

    /// The body the last ruleset call sent.
    fn sent(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.dir.join("sent.json")).expect("sent"))
            .expect("the body is JSON")
    }

    fn run(&self, args: &[&str]) -> Ran {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join(SCRIPT))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_DIR", &self.dir)
            .output()
            .expect("bash runs");
        let gh = fs::read_to_string(self.dir.join("gh.log")).unwrap_or_default();
        Ran {
            status: out.status.code().expect("an exit code"),
            stdout: String::from_utf8(out.stdout).expect("utf-8"),
            stderr: String::from_utf8(out.stderr).expect("utf-8"),
            gh: gh.lines().map(str::to_owned).collect(),
        }
    }
}

#[test]
fn the_script_parses() {
    let out = Command::new("bash")
        .arg("-n")
        .arg(root().join(SCRIPT))
        .output()
        .expect("bash runs");
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn the_doc_and_the_held_scripts_table_name_the_script() {
    assert!(read("docs/ci.md").contains(SCRIPT));
    assert!(
        read("tools/held/README.md").contains("| `merge-queue/merge_queue.sh` |"),
        "a row in the table of tools/held/README.md"
    );
}

#[test]
fn a_dry_run_prints_the_settings_the_ruleset_and_the_calls_and_writes_nothing() {
    let rig = Rig::new("dry");
    let ran = rig.run(&[]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert!(ran.writes().is_empty(), "{:?}", ran.gh);
    assert_eq!(
        ran.lines()[..3],
        [
            "mode: DRY RUN (run again with --apply to write)",
            "== komira-ai/jobscheduler is public: its plan allows a merge queue",
            "== ci.yml on main starts on merge_group and has the job all-passed",
        ]
    );
    assert_eq!(ran.section("== the rules in effect on main now:"), ["[]"]);
    assert!(ran.lines().contains(&r#"== squash message settings now:   {"squash_merge_commit_title":"COMMIT_OR_PR_TITLE","squash_merge_commit_message":"COMMIT_MESSAGES"}"#));
    assert!(ran.lines().contains(
        &r#"== squash message settings after: {"squash_merge_commit_title":"PR_TITLE","squash_merge_commit_message":"BLANK"}"#
    ));
    assert_eq!(ran.section(r#"== ruleset "main" now:"#), ["none"]);
    let after: Value = serde_json::from_str(&ran.section(r#"== ruleset "main" after:"#).join("\n"))
        .expect("the ruleset after is JSON");
    assert_eq!(after, documented());
    assert_eq!(
        ran.section("== the calls:"),
        [
            "gh api --method PATCH repos/komira-ai/jobscheduler -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=BLANK",
            r#"gh api --method POST repos/komira-ai/jobscheduler/rulesets --input -   (the body: ruleset "main" after, above)"#,
            "DRY RUN: nothing changed.",
        ]
    );
}

#[test]
fn apply_writes_the_settings_then_the_documented_ruleset_and_prints_the_back_out() {
    let rig = Rig::new("apply");
    let dry = rig.run(&[]);
    let ran = rig.run(&["--apply"]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(ran.lines()[0], "mode: APPLY");
    assert_eq!(ran.writes(), [PATCH, POST]);
    assert_eq!(rig.sent(), documented());
    // The calls a dry run prints are the ones an apply makes.
    let printed: Vec<String> = dry.section("== the calls:")[..2]
        .iter()
        .map(|l| l.split_once("   (").map_or(*l, |(call, _)| call).to_owned())
        .collect();
    let made: Vec<String> = ran
        .writes()
        .iter()
        .map(|c| format!("gh {}", c.trim_end_matches(" --silent")))
        .collect();
    assert_eq!(printed, made);
    assert_eq!(
        ran.section("== to back out:"),
        [BACK_SETTINGS, BACK_CREATED]
    );
    assert_eq!(ran.lines().last(), Some(&DONE));
}

#[test]
fn an_existing_ruleset_main_keeps_its_other_rules_and_bypass_actors() {
    let rig = Rig::new("existing");
    let bypass =
        json!([{ "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always" }]);
    let mut old = documented();
    old["bypass_actors"] = bypass.clone();
    old["rules"] = json!([
        { "type": "deletion" },
        { "type": "required_status_checks", "parameters": {
            "strict_required_status_checks_policy": true,
            "do_not_enforce_on_create": false,
            "required_status_checks": [{ "context": "lint" }] } },
        { "type": "non_fast_forward" },
    ]);
    rig.ruleset_7(old);
    rig.set(
        "rules.json",
        &json!([{ "type": "required_status_checks", "ruleset_id": 7 }]).to_string(),
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(
        ran.writes(),
        [
            PATCH,
            "api --method PUT repos/komira-ai/jobscheduler/rulesets/7 --input -"
        ]
    );
    let mut want = documented();
    let rules = want["rules"].as_array().expect("rules").clone();
    want["rules"] =
        json!([{ "type": "deletion" }, { "type": "non_fast_forward" }, rules[0], rules[1]]);
    want["bypass_actors"] = bypass;
    assert_eq!(rig.sent(), want);
    assert_eq!(
        ran.section("== to back out:"),
        [
            BACK_SETTINGS,
            r#"gh api --method PUT repos/komira-ai/jobscheduler/rulesets/7 --input -   (the body: ruleset "main" now, above)"#
        ]
    );
    assert_eq!(
        ran.lines().last(),
        Some(
            &"DONE: main merges through the merge queue (ruleset 7), the required check is all-passed"
        )
    );
}

#[test]
fn a_queue_already_on_as_documented_is_left_alone() {
    let rig = Rig::new("on");
    rig.set("repo.json", &repo("public", "PR_TITLE", "BLANK"));
    rig.ruleset_7(documented());
    rig.set(
        "rules.json",
        &json!([
            { "type": "required_status_checks", "ruleset_id": 7 },
            { "type": "merge_queue", "ruleset_id": 7 }
        ])
        .to_string(),
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert!(ran.writes().is_empty(), "{:?}", ran.gh);
    assert_eq!(
        ran.section("== the calls:"),
        ["none: the merge queue is already on as docs/ci.md gives it"]
    );
}

#[test]
fn settings_already_set_write_the_ruleset_alone() {
    let rig = Rig::new("ruleset-only");
    rig.set("repo.json", &repo("public", "PR_TITLE", "BLANK"));
    let ran = rig.run(&["--apply"]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(ran.writes(), [POST]);
    assert_eq!(ran.section("== to back out:"), [BACK_CREATED]);
    assert_eq!(ran.lines().last(), Some(&DONE));
}

#[test]
fn a_ruleset_already_as_documented_writes_the_settings_alone() {
    let rig = Rig::new("settings-only");
    rig.ruleset_7(documented());
    let ran = rig.run(&["--apply"]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(ran.writes(), [PATCH]);
    assert_eq!(ran.section("== to back out:"), [BACK_SETTINGS]);
    assert_eq!(
        ran.lines().last(),
        Some(
            &"DONE: main merges through the merge queue (ruleset 7), the required check is all-passed"
        )
    );
}

#[test]
fn what_is_refused_before_any_write_even_with_apply() {
    let ci = read(".github/workflows/ci.yml");
    let cases: [(&str, &dyn Fn(&Rig), &str); 5] = [
        (
            "logged-out",
            &|r: &Rig| r.set("logged-out", ""),
            "STOP: gh is not logged in\n",
        ),
        (
            "private",
            &|r: &Rig| r.set("repo.json", &repo("private", "PR_TITLE", "BLANK")),
            "STOP: komira-ai/jobscheduler is private: a private repository has a merge queue only on GitHub Enterprise Cloud; nothing changed\n",
        ),
        (
            "no-merge-group",
            &|r: &Rig| {
                r.set(
                    "ci.yml",
                    &ci.replacen("\n  merge_group:\n", "\n  workflow_dispatch:\n", 1),
                )
            },
            "STOP: ci.yml on main does not start on merge_group: the queue would wait for a check that never reports; nothing changed\n",
        ),
        (
            "no-aggregate",
            &|r: &Rig| {
                r.set(
                    "ci.yml",
                    &ci.replacen("\n  all-passed:\n", "\n  all-green:\n", 1),
                )
            },
            "STOP: ci.yml on main has no job all-passed, the check the ruleset requires; nothing changed\n",
        ),
        (
            "other-ruleset",
            &|r: &Rig| {
                r.ruleset_7(documented());
                r.set(
                    "rules.json",
                    &json!([
                        { "type": "merge_queue", "ruleset_id": 7 },
                        { "type": "deletion", "ruleset_id": 99 },
                        { "type": "required_status_checks", "ruleset_id": 99 }
                    ])
                    .to_string(),
                );
            },
            "STOP: another ruleset already puts rules on main: required_status_checks (ruleset 99); nothing changed\n",
        ),
    ];
    for (name, setup, stderr) in cases {
        let rig = Rig::new(name);
        setup(&rig);
        let ran = rig.run(&["--apply"]);
        assert_eq!((ran.status, ran.stderr.as_str()), (1, stderr), "{name}");
        assert!(ran.writes().is_empty(), "{name}: {:?}", ran.gh);
    }
    // The edits above changed one line each of the real ci.yml.
    assert_eq!(ci.matches("\n  merge_group:\n").count(), 1);
    assert_eq!(ci.matches("\n  all-passed:\n").count(), 1);
}

#[test]
fn a_queue_of_another_ruleset_is_refused_when_main_has_no_ruleset() {
    let rig = Rig::new("org-queue");
    rig.set(
        "rules.json",
        &json!([{ "type": "merge_queue", "ruleset_id": 99 }]).to_string(),
    );
    let ran = rig.run(&[]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            1,
            "STOP: another ruleset already puts rules on main: merge_queue (ruleset 99); nothing changed\n"
        )
    );
}

#[test]
fn an_unknown_flag_and_force_are_refused_before_any_call() {
    for args in [
        &["--force"][..],
        &["--apply", "--force"],
        &["apply"],
        &["--dry-run"],
    ] {
        let rig = Rig::new("usage");
        let ran = rig.run(args);
        assert_eq!(
            (ran.status, ran.stderr.as_str()),
            (
                2,
                "usage: merge_queue.sh [--apply]   (a dry run without --apply; there is no --force)\n"
            ),
            "{args:?}"
        );
        assert!(ran.gh.is_empty(), "{args:?}: {:?}", ran.gh);
    }
}

#[test]
fn a_failed_settings_call_stops_before_the_ruleset() {
    let rig = Rig::new("fail-patch");
    rig.set("fail-patch", "");
    let ran = rig.run(&["--apply"]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            1,
            "gh: Validation Failed (HTTP 422)\nSTOP: the squash settings call failed; nothing changed\n"
        )
    );
    assert_eq!(ran.writes(), [PATCH]);
}

#[test]
fn a_failed_ruleset_call_says_how_to_back_out_the_settings() {
    let rig = Rig::new("fail-write");
    rig.set("fail-write", "");
    let ran = rig.run(&["--apply"]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            1,
            format!(
                "gh: Resource not accessible by integration (HTTP 403)\nthe squash message settings were written; to back them out: {BACK_SETTINGS}\nSTOP: the ruleset call failed; the ruleset is as it was\n"
            )
            .as_str()
        )
    );
    assert_eq!(ran.writes(), [PATCH, POST]);
    assert!(!ran.stdout.contains("DONE"), "{}", ran.stdout);
}

#[test]
fn a_ruleset_that_reads_back_otherwise_fails_after_the_back_out_is_printed() {
    let rig = Rig::new("drop-queue");
    rig.set("drop-queue", "");
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 1, "{}", ran.stdout);
    assert_eq!(
        ran.section("== to back out:"),
        [BACK_SETTINGS, BACK_CREATED]
    );
    let read_back = ran
        .stderr
        .strip_prefix("STOP: ruleset 42 reads back as ")
        .and_then(|s| s.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("{}", ran.stderr));
    let read_back: Value = serde_json::from_str(read_back).expect("the read-back is JSON");
    assert_eq!(
        read_back["rules"],
        json!([documented()["rules"][0]]),
        "the rules read back, without the queue"
    );
    assert!(!ran.stdout.contains("DONE"), "{}", ran.stdout);
}

#[test]
fn settings_that_read_back_otherwise_fail_the_apply() {
    let rig = Rig::new("ignore-patch");
    rig.set("ignore-patch", "");
    let ran = rig.run(&["--apply"]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            1,
            r#"STOP: the squash message settings read back as {"squash_merge_commit_title":"COMMIT_OR_PR_TITLE","squash_merge_commit_message":"COMMIT_MESSAGES"}
"#
        )
    );
    assert_eq!(ran.writes(), [PATCH, POST]);
    assert_eq!(
        ran.section("== to back out:"),
        [BACK_SETTINGS, BACK_CREATED]
    );
}

#[test]
fn a_ruleset_main_of_the_organization_is_another_ruleset() {
    let rig = Rig::new("org-main");
    rig.set(
        "rulesets.json",
        &json!([{ "id": 99, "name": "main", "target": "branch", "source_type": "Organization" }])
            .to_string(),
    );
    rig.set(
        "rules.json",
        &json!([{ "type": "merge_queue", "ruleset_id": 99 }]).to_string(),
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!(
        (ran.status, ran.stderr.as_str()),
        (
            1,
            "STOP: another ruleset already puts rules on main: merge_queue (ruleset 99); nothing changed\n"
        )
    );
    assert!(ran.writes().is_empty(), "{:?}", ran.gh);
    assert!(
        !ran.gh.iter().any(|c| c.ends_with("/rulesets/99")),
        "the organization's ruleset is never read as this one: {:?}",
        ran.gh
    );
}

#[test]
fn an_organization_ruleset_main_without_rules_on_main_leaves_a_new_one_to_create() {
    let rig = Rig::new("org-main-quiet");
    rig.set(
        "rulesets.json",
        &json!([{ "id": 99, "name": "main", "target": "branch", "source_type": "Organization" }])
            .to_string(),
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(ran.writes(), [PATCH, POST]);
    assert_eq!(rig.sent(), documented());
}
