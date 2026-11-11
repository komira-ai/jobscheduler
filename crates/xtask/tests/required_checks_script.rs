//! The held script `tools/held/required-checks/require_macos_checks.sh`, run by `bash` with a
//! stand-in `gh` first on `PATH`. The stand-in appends each call's arguments (one per line, then
//! a line `--`) and, for a write, the body it read on standard input; it answers the read of the
//! required status checks as it is told (no protection: GitHub's 404; a list of contexts; or
//! another failure). So each test pins what the script would ask GitHub to change: nothing on a
//! dry run, which is the default; on `--apply`, only the two macOS checks added, by the call
//! that keeps the checks already there. Nothing reaches GitHub: the real `gh` is never started.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A scratch directory of its own for each test, holding the stand-in `gh` in `bin/`.
struct Rig {
    dir: PathBuf,
    /// Removes the directory when the rig is dropped.
    _scratch: js_testkit::dir::TestDir,
}

/// What one run of the script did.
#[derive(Debug)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
    /// The calls of the stand-in `gh`, each the list of its arguments.
    calls: Vec<Vec<String>>,
    /// The bodies the stand-in read for writes, in order.
    bodies: Vec<String>,
}

/// How the stand-in answers the read of the required status checks.
enum Read<'a> {
    /// main has no protection of its status checks: GitHub's 404.
    Absent,
    /// These contexts are required.
    Present(&'a [&'a str]),
    /// Another failure.
    Error,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("required-checks-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).unwrap();
        let gh = dir.join("bin/gh");
        install_script(
            &gh,
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" -- >> \"$FAKE_LOG\"\n\
             case \" $* \" in\n\
             *' --method '*)\n\
               cat >> \"$FAKE_LOG.body\"; printf '%s\\n' '--' >> \"$FAKE_LOG.body\"\n\
               exit \"${FAKE_WRITE_STATUS:-0}\" ;;\n\
             esac\n\
             case \"$FAKE_READ\" in\n\
             absent) echo 'gh: Branch not protected (HTTP 404)' >&2; exit 1 ;;\n\
             error) echo 'gh: Server Error (HTTP 500)' >&2; exit 1 ;;\n\
             *) printf '%s' \"$FAKE_CONTEXTS\" ;;\n\
             esac\n",
            "755",
        );
        Rig {
            dir,
            _scratch: scratch,
        }
    }

    fn run(&self, args: &[&str], read: Read<'_>, write_status: i32) -> Ran {
        let log = self.dir.join("gh.log");
        let _ = fs::remove_file(&log);
        let _ = fs::remove_file(self.dir.join("gh.log.body"));
        let (mode, contexts) = match read {
            Read::Absent => ("absent", String::new()),
            Read::Error => ("error", String::new()),
            Read::Present(list) => (
                "present",
                list.iter().map(|c| format!("{c}\n")).collect::<String>(),
            ),
        };
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join("tools/held/required-checks/require_macos_checks.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_LOG", &log)
            .env("FAKE_READ", mode)
            .env("FAKE_CONTEXTS", contexts)
            .env("FAKE_WRITE_STATUS", write_status.to_string())
            .output()
            .unwrap();
        let split = |path: &Path| -> Vec<Vec<String>> {
            let text = fs::read_to_string(path).unwrap_or_default();
            let mut out = Vec::new();
            let mut cur = Vec::new();
            for line in text.lines() {
                if line == "--" {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.push(line.to_owned());
                }
            }
            out
        };
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            calls: split(&log),
            bodies: split(&self.dir.join("gh.log.body"))
                .into_iter()
                .map(|lines| lines.join("\n"))
                .collect(),
        }
    }
}

const BASE: &str = "repos/komira-ai/jobscheduler/branches/main/protection";

/// The read the script makes first, of `repo`'s main.
fn read_call(repo: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        format!("repos/{repo}/branches/main/protection/required_status_checks"),
        "--jq".to_owned(),
        ".contexts[]".to_owned(),
    ]
}

#[test]
fn a_dry_run_is_the_default_and_changes_nothing() {
    let rig = Rig::new("dry");
    let ran = rig.run(&[], Read::Absent, 0);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.calls, [read_call("komira-ai/jobscheduler")]);
    assert!(ran.bodies.is_empty());
    assert_eq!(
        ran.stdout,
        "repository: komira-ai/jobscheduler, branch main\n\
         required status checks now: none (main has no protection of its status checks)\n\
         to add: macos js-agent-macos-pkg\n\
         dry run: nothing changed; run again with --apply to add them\n"
    );
    // With checks already required, still only the read.
    let ran = rig.run(&[], Read::Present(&["test", "lint"]), 0);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.calls, [read_call("komira-ai/jobscheduler")]);
    assert!(
        ran.stdout
            .contains("required status checks now: test lint\n"),
        "{}",
        ran.stdout
    );
}

#[test]
fn apply_without_a_protection_sets_exactly_the_two_checks() {
    let ran = Rig::new("absent").run(&["--apply"], Read::Absent, 0);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        ran.calls,
        [
            read_call("komira-ai/jobscheduler"),
            vec![
                "api".to_owned(),
                "--method".to_owned(),
                "PUT".to_owned(),
                BASE.to_owned(),
                "--input".to_owned(),
                "-".to_owned(),
            ],
        ]
    );
    assert_eq!(
        ran.bodies,
        [
            r#"{"required_status_checks":{"strict":false,"contexts":["macos","js-agent-macos-pkg"]},"enforce_admins":false,"required_pull_request_reviews":null,"restrictions":null}"#
        ]
    );
    assert!(ran.stdout.ends_with("added: macos js-agent-macos-pkg\n"));
}

#[test]
fn apply_with_a_protection_adds_only_the_missing_checks_and_keeps_the_others() {
    let ran = Rig::new("present").run(&["--apply"], Read::Present(&["test"]), 0);
    assert_eq!(ran.status, 0, "{ran:?}");
    let add = |checks: &[&str]| {
        let mut call = vec![
            "api".to_owned(),
            "--method".to_owned(),
            "POST".to_owned(),
            format!("{BASE}/required_status_checks/contexts"),
        ];
        for c in checks {
            call.push("-f".to_owned());
            call.push(format!("contexts[]={c}"));
        }
        call
    };
    assert_eq!(
        ran.calls,
        [
            read_call("komira-ai/jobscheduler"),
            add(&["macos", "js-agent-macos-pkg"])
        ]
    );
    // One already there: only the other is added (a name that only contains it does not count).
    let ran = Rig::new("one").run(&["--apply"], Read::Present(&["macos", "macos-probe"]), 0);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.calls[1], add(&["js-agent-macos-pkg"]));
    assert!(ran.stdout.ends_with("added: js-agent-macos-pkg\n"));
}

#[test]
fn nothing_is_written_when_both_checks_are_required_already() {
    let ran = Rig::new("done").run(
        &["--apply"],
        Read::Present(&["js-agent-macos-pkg", "test", "macos"]),
        0,
    );
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.calls, [read_call("komira-ai/jobscheduler")]);
    assert!(
        ran.stdout
            .ends_with("nothing to do: macos js-agent-macos-pkg are required already\n")
    );
}

#[test]
fn a_failed_read_or_write_fails_the_script_and_a_failed_read_writes_nothing() {
    let ran = Rig::new("readerr").run(&["--apply"], Read::Error, 0);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.calls, [read_call("komira-ai/jobscheduler")]);
    assert_eq!(
        ran.stderr,
        "reading the protection of main in komira-ai/jobscheduler failed:\ngh: Server Error (HTTP 500)\n"
    );
    let ran = Rig::new("writeerr").run(&["--apply"], Read::Absent, 1);
    assert_ne!(ran.status, 0, "{ran:?}");
    assert!(!ran.stdout.contains("added:"), "{}", ran.stdout);
}

#[test]
fn another_repository_is_named_in_every_call() {
    let ran = Rig::new("repo").run(&["--repo", "someone/fork", "--apply"], Read::Absent, 0);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.calls[0], read_call("someone/fork"));
    assert_eq!(
        ran.calls[1][3],
        "repos/someone/fork/branches/main/protection"
    );
}

#[test]
fn a_bad_argument_is_refused_before_gh_runs() {
    let rig = Rig::new("bad");
    for args in [
        &["--force"][..],
        &["--repo"][..],
        &["--repo", ""][..],
        &["--repo", "noslash"][..],
        &["apply"][..],
    ] {
        let ran = rig.run(args, Read::Absent, 0);
        assert_eq!(ran.status, 2, "{args:?}");
        assert!(ran.calls.is_empty(), "{args:?}");
        assert!(
            ran.stderr.starts_with("usage: require_macos_checks.sh"),
            "{args:?}"
        );
    }
}
