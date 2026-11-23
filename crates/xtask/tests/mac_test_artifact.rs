//! `cargo xtask mac-test-artifact`: the staging of a Mac root test binary and the `js-agent` it
//! starts, from cargo's build messages, over stand-in binaries in the test's own directory (the
//! first bytes of an arm64 Mach-O executable, or not).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::mac_test_artifact::{Args, Error, is_arm64_executable, run, test_executable};

/// The first sixteen bytes of an arm64 Mach-O executable: the 64-bit magic, the arm64 CPU
/// type, a subtype, the executable file type.
const ARM64_EXE: [u8; 16] = [
    0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
];

/// A scratch directory of its own for each test.
struct Rig {
    dir: PathBuf,
    _scratch: js_testkit::dir::TestDir,
}

impl Rig {
    fn new(name: &str) -> Self {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("mac-test-artifact-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        Self {
            dir,
            _scratch: scratch,
        }
    }

    /// A built tree: the test binary `deps/native_privileged-abc` and the supervisor, with
    /// `test` and `supervisor` as their bytes, and the build messages naming the test binary.
    fn built(&self, test: &[u8], supervisor: &[u8]) -> Args {
        let debug = self.dir.join("target/aarch64-apple-darwin/debug");
        fs::create_dir_all(debug.join("deps")).unwrap();
        let exe = debug.join("deps/native_privileged-abc");
        fs::write(&exe, test).unwrap();
        fs::write(debug.join("js-agent"), supervisor).unwrap();
        let messages = [
            message(
                "compiler-artifact",
                true,
                "native_privileged",
                "test",
                Some(&exe),
            ),
            message(
                "compiler-artifact",
                false,
                "js-agent",
                "bin",
                Some(&debug.join("js-agent")),
            ),
            "{\"reason\":\"build-finished\",\"success\":true}".to_owned(),
        ];
        fs::write(self.dir.join("build.json"), messages.join("\n") + "\n").unwrap();
        Args {
            test: "native_privileged".to_owned(),
            target: "aarch64-apple-darwin".to_owned(),
            build_json: self.dir.join("build.json"),
            out: self.dir.join("dist"),
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            summary: self.dir.join("summary.md"),
            github_env: self.dir.join("env"),
            target_dir: self.dir.join("target"),
        }
    }
}

/// One cargo message of a built target.
fn message(reason: &str, test: bool, name: &str, kind: &str, exe: Option<&Path>) -> String {
    serde_json::json!({
        "reason": reason,
        "profile": { "test": test },
        "target": { "name": name, "kind": [kind] },
        "executable": exe.map(|exe| exe.display().to_string()),
    })
    .to_string()
}

/// An arm64 Mach-O executable's bytes with `tail` after the header.
fn arm64(tail: &[u8]) -> Vec<u8> {
    [ARM64_EXE.as_slice(), tail].concat()
}

/// Asserts that the header check takes an arm64 Mach-O executable and nothing else: a file
/// too short, another magic, another CPU, another file type.
///
/// Catches an x86_64 or a library binary staged for the Mac root tests.
#[test]
fn only_an_arm64_mach_o_executable_passes_the_check() {
    assert!(is_arm64_executable(&arm64(b"rest")));
    assert!(!is_arm64_executable(&ARM64_EXE[..15]));
    let mut other = ARM64_EXE;
    other[0] = 0xce;
    assert!(!is_arm64_executable(&other));
    let mut x86 = ARM64_EXE;
    x86[4] = 0x07;
    assert!(!is_arm64_executable(&x86));
    let mut dylib = ARM64_EXE;
    dylib[12] = 0x06;
    assert!(!is_arm64_executable(&dylib));
}

/// Asserts that the one test executable of the target is taken from the build messages, and
/// that every other message is passed over: another reason, not built for tests, another
/// target, another kind, no kind, no executable, and a blank line.
///
/// Catches a binary chosen by file name or another test's binary staged.
#[test]
fn the_one_test_executable_of_the_target_is_found() {
    let path = Path::new("build.json");
    let wanted = message(
        "compiler-artifact",
        true,
        "t",
        "test",
        Some(Path::new("/b/t-1")),
    );
    let others = [
        message("build-script-executed", true, "t", "test", Some(Path::new("/b/x"))),
        message("compiler-artifact", false, "t", "test", Some(Path::new("/b/x"))),
        message("compiler-artifact", true, "u", "test", Some(Path::new("/b/x"))),
        message("compiler-artifact", true, "t", "bin", Some(Path::new("/b/x"))),
        "{\"reason\":\"compiler-artifact\",\"profile\":{\"test\":true},\"target\":{\"name\":\"t\"},\"executable\":\"/b/x\"}".to_owned(),
        message("compiler-artifact", true, "t", "test", None),
        String::new(),
    ];
    let text = [others.join("\n"), wanted.clone()].join("\n");
    assert_eq!(
        test_executable(&text, path, "t").unwrap(),
        PathBuf::from("/b/t-1")
    );
}

/// Asserts the refusals of the build messages: none of the target's executables, two of them,
/// and a line that is not JSON, each naming what it found.
///
/// Catches a staging that picks one of two binaries or none silently.
#[test]
fn not_exactly_one_executable_or_a_bad_line_is_refused() {
    let path = Path::new("build.json");
    let one = message(
        "compiler-artifact",
        true,
        "t",
        "test",
        Some(Path::new("/b/t-1")),
    );
    let two = message(
        "compiler-artifact",
        true,
        "t",
        "test",
        Some(Path::new("/b/t-2")),
    );
    let none = test_executable("", path, "t").unwrap_err();
    assert_eq!(
        none.to_string(),
        "expected exactly one t test executable, got []"
    );
    let both = test_executable(&format!("{one}\n{two}"), path, "t").unwrap_err();
    assert_eq!(
        both.to_string(),
        "expected exactly one t test executable, got [\"/b/t-1\", \"/b/t-2\"]"
    );
    let bad = test_executable(&format!("{one}\nnot json"), path, "t").unwrap_err();
    assert!(matches!(&bad, Error::Json { line: 2, .. }), "{bad}");
    assert!(
        bad.to_string()
            .starts_with("build.json: line 2 is not JSON: "),
        "{bad}"
    );
}

/// Asserts a staging: the test binary and the supervisor copied into the new output
/// directory, `SHA256SUMS` as `shasum -a 256` prints the two, the summary appended (an
/// earlier line kept) and `SHORT` with the commit's first twelve hex appended.
///
/// Catches a staged pair whose sums do not match, or an artifact named without the commit.
#[test]
fn the_test_binary_and_its_supervisor_are_staged_with_their_sums() {
    let rig = Rig::new("staged");
    let args = rig.built(&arm64(b"test"), &arm64(b"agent"));
    fs::write(&args.summary, "earlier\n").unwrap();
    let said = run(&args).unwrap();
    let sha = |bytes: &[u8]| {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(bytes))
    };
    let (test_sha, agent_sha) = (sha(&arm64(b"test")), sha(&arm64(b"agent")));
    assert_eq!(
        said,
        format!(
            "staged native_privileged ({test_sha}) and js-agent ({agent_sha}) in {}",
            args.out.display()
        )
    );
    assert_eq!(
        fs::read(args.out.join("native_privileged")).unwrap(),
        arm64(b"test")
    );
    assert_eq!(
        fs::read(args.out.join("js-agent")).unwrap(),
        arm64(b"agent")
    );
    assert_eq!(
        fs::read_to_string(args.out.join("SHA256SUMS")).unwrap(),
        format!("{test_sha}  native_privileged\n{agent_sha}  js-agent\n")
    );
    let summary = fs::read_to_string(&args.summary).unwrap();
    assert!(
        summary.starts_with("earlier\n### js-agent native_privileged test binary\n"),
        "{summary}"
    );
    assert!(
        summary.contains(&format!("- sha256: `{test_sha}`\n")),
        "{summary}"
    );
    assert!(
        summary.contains(&format!("- js-agent sha256: `{agent_sha}`\n")),
        "{summary}"
    );
    assert_eq!(
        fs::read_to_string(&args.github_env).unwrap(),
        "SHORT=0123456789ab\n"
    );
}

/// Asserts each failure of a staging, by its text: build messages that are not there, an
/// output directory that exists, a test binary that is not an arm64 executable, a supervisor
/// that is not there, and a summary file that cannot be written.
///
/// Catches a staging that uploads what it could not check.
#[test]
fn a_staging_that_cannot_be_done_says_why() {
    let rig = Rig::new("failed");
    let mut args = rig.built(&arm64(b"test"), &arm64(b"agent"));
    args.build_json = rig.dir.join("missing.json");
    let why = run(&args).unwrap_err().to_string();
    assert!(
        why.starts_with(&format!("{}: ", args.build_json.display())),
        "{why}"
    );

    let args = rig.built(&arm64(b"test"), &arm64(b"agent"));
    fs::create_dir(&args.out).unwrap();
    let why = run(&args).unwrap_err().to_string();
    assert!(
        why.starts_with(&format!("{}: ", args.out.display())),
        "{why}"
    );
    fs::remove_dir(&args.out).unwrap();

    let args = rig.built(b"#!/bin/sh\n", &arm64(b"agent"));
    let why = run(&args).unwrap_err().to_string();
    assert_eq!(
        why,
        format!(
            "{} is not an arm64 Mach-O executable",
            args.out.join("native_privileged").display()
        )
    );
    fs::remove_dir_all(&args.out).unwrap();

    let args = rig.built(&arm64(b"test"), &arm64(b"agent"));
    let supervisor = args.target_dir.join("aarch64-apple-darwin/debug/js-agent");
    fs::remove_file(&supervisor).unwrap();
    let why = run(&args).unwrap_err().to_string();
    assert!(
        why.starts_with(&format!("{}: ", supervisor.display())),
        "{why}"
    );
    fs::remove_dir_all(&args.out).unwrap();

    let mut args = rig.built(&arm64(b"test"), &arm64(b"agent"));
    args.summary = rig.dir.clone();
    let why = run(&args).unwrap_err().to_string();
    assert!(
        why.starts_with(&format!("{}: ", rig.dir.display())),
        "{why}"
    );
}

/// Asserts the verb through the built `xtask` program: a staging exits 0 and prints what it
/// staged; a failing one exits non-zero naming the verb.
///
/// Catches a verb the workflows call that the program does not answer.
#[test]
fn the_verb_runs_through_the_program() {
    let rig = Rig::new("program");
    let args = rig.built(&arm64(b"test"), &arm64(b"agent"));
    let call = |args: &Args| {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("mac-test-artifact")
            .args(["--test", &args.test, "--target", &args.target])
            .arg("--build-json")
            .arg(&args.build_json)
            .arg("--out")
            .arg(&args.out)
            .args(["--commit", &args.commit])
            .arg("--summary")
            .arg(&args.summary)
            .arg("--github-env")
            .arg(&args.github_env)
            .arg("--target-dir")
            .arg(&args.target_dir)
            .output()
            .unwrap()
    };
    let ok = call(&args);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(String::from_utf8_lossy(&ok.stdout).starts_with("staged native_privileged ("));
    let again = call(&args);
    assert!(!again.status.success());
    let err = String::from_utf8_lossy(&again.stderr);
    assert!(err.contains("mac-test-artifact: "), "{err}");
}
