//! `cargo xtask agent-handover-check`: the verb runs the check of `js-handover-check` (whose own
//! tests pin the check) on the built binary, through a stand-in `js` given with `--js`, for the
//! verb's exit statuses. Nothing reaches a server.

use std::path::Path;
use std::process::Command;

use js_testkit::dir::TestDir;
use js_testkit::spawn::install_script;

/// The built `cargo xtask` run with `args`: its status, stdout and stderr.
fn verb(args: &[String]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("agent-handover-check")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// Asserts the verb's exit statuses: 1 with the verdict on stderr when the check did not pass
/// (host n1 moved from 0.1.316 to 0.1.418 with no job running before, so it is inconclusive),
/// and 2 with the reason on stderr for a usage error of the check (a host given twice). It
/// catches a verb whose usage errors exit as failures, or whose failure exits 0.
#[test]
fn the_verb_exits_by_its_verdict() {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-handover-check"),
        "verb",
    );
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let js = dir.path().join("js");
    let flag = dir.path().join("listed");
    // The first `host list` answers before, every later one after; no job runs or ended.
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n*'host list'*) if [ -f '{flag}' ]; then echo '{new}'; else touch '{flag}'; echo '{old}'; fi ;;\n*) echo '{{\"jobs\":[]}}' ;;\nesac\n",
        flag = flag.display(),
        new = r#"{"hosts":[{"id":"n1","state":"HOST_STATE_LIVE","agentVersion":"0.1.418"}]}"#,
        old = r#"{"hosts":[{"id":"n1","state":"HOST_STATE_LIVE","agentVersion":"0.1.316"}]}"#,
    );
    install_script(&js, &script, "755");
    let mut args: Vec<String> = [
        "--server",
        "https://jobs.example:7300",
        "--token-file",
        "token",
        "--out-dir",
    ]
    .map(str::to_owned)
    .to_vec();
    args.push(out.display().to_string());
    args.extend(["--js".to_owned(), js.display().to_string()]);
    args.extend(["--host", "n1"].map(str::to_owned));

    let (status, stdout, stderr) = verb(&args);
    assert_eq!(status, 1, "{stdout}{stderr}");
    assert_eq!(
        stderr,
        "xtask: INCONCLUSIVE: every host moved and lost nothing, but n1 ran no job before, so its hand-over of running jobs was not shown\n"
    );

    args.extend(["--host", "n1"].map(str::to_owned));
    let (status, _, stderr) = verb(&args);
    assert_eq!(status, 2);
    assert_eq!(stderr, "xtask: --host n1 is given twice\n");
}
