//! `cargo xtask release macos-leg`, run as the built binary against a scratch checkout (a
//! `Cargo.toml` with the workspace version and a `LICENSE`) with a stand-in `cargo` and a
//! stand-in `otool`, small shell scripts in the scratch directory. The stand-in `cargo` logs its
//! command lines and working directory, and writes a stand-in binary where the real build
//! would; the stand-in `otool` prints a load-command listing the test chooses. Nothing is built
//! and the real `cargo` and `otool` are never started.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use js_testkit::dir::TestDir;

const TARGET: &str = "aarch64-apple-darwin";
const VERSION: &str = "1.2.3";

/// What one run printed.
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
}

/// A scratch checkout, the stand-ins and an output directory.
struct Rig {
    dir: TestDir,
    /// What the stand-in `cargo` makes of the binary: the first line `--version` prints, with
    /// `%s` for the binary's name.
    version_line: String,
    /// The mode of the binaries the stand-in `cargo` writes.
    mode: u32,
    /// The binary the stand-in `cargo` fails to build.
    failing: String,
    /// The shell commands of the stand-in `otool`.
    otool: String,
}

fn minos(value: &str) -> String {
    otool_listing(&format!(
        "Load command 9\n      cmd LC_BUILD_VERSION\n  cmdsize 32\n platform 1\n    minos {value}\n      sdk 15.0\n"
    ))
}

fn otool_listing(text: &str) -> String {
    format!("cat <<'EOF'\n{text}EOF\n")
}

impl Rig {
    fn new(name: &str) -> Rig {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        fs::write(
            dir.join("Cargo.toml"),
            format!("[workspace.package]\nversion = \"{VERSION}\"\n"),
        )
        .unwrap();
        fs::write(dir.join("LICENSE"), "licence text\n").unwrap();
        Rig {
            dir,
            version_line: format!("%s {VERSION}"),
            mode: 0o755,
            failing: String::new(),
            otool: minos("11.0"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn out(&self) -> PathBuf {
        self.path("out")
    }

    fn cargo_log(&self) -> String {
        fs::read_to_string(self.path("cargo.log")).unwrap_or_default()
    }

    fn script(&self, name: &str, text: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, format!("#!/bin/sh\n{text}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Runs the verb with `builds` and `floor`, and `more` flags.
    fn run(&self, builds: &str, floor: &str, more: &[&str]) -> Ran {
        let log = self.path("cargo.log");
        let cargo = self.script(
            "cargo",
            &format!(
                "printf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{log}'\n\
                 while [ $# -gt 0 ]; do\n\
                   case \"$1\" in --target) t=$2;; --bin) b=$2;; esac\n\
                   shift\n\
                 done\n\
                 [ \"$b\" = '{failing}' ] && exit 3\n\
                 mkdir -p \"target/$t/release\"\n\
                 p=\"target/$t/release/$b\"\n\
                 line=\"{line}\"\n\
                 printf '#!/bin/sh\\nprintf \"%s\\\\n\" \"%s\"\\nexit 0\\n# {VERSION}\\nruntimes: docker\\n' \"$line\" > \"$p\"\n\
                 chmod {mode:o} \"$p\"\n",
                log = log.display(),
                failing = self.failing,
                line = self.version_line.replace("%s", "$b"),
                mode = self.mode,
            ),
        );
        let otool = self.script(
            "otool",
            &self.otool.replace("{log}", &log.display().to_string()),
        );
        let repo = self.dir.path().to_str().unwrap();
        let out = self.out();
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args([
                "release",
                "macos-leg",
                "--target",
                TARGET,
                "--builds",
                builds,
            ])
            .args(["--min-macos", floor, "--out", out.to_str().unwrap()])
            .args(["--repo-dir", repo]);
        if !more.contains(&"--cargo") {
            command.args(["--cargo", cargo.to_str().unwrap()]);
        }
        let output = command
            .args(["--otool", otool.to_str().unwrap()])
            .args(more)
            .output()
            .unwrap();
        Ran {
            status: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }
}

const BOTH: &str = "js-cli:js js-agent:js-agent";

fn failed(ran: &Ran) -> &str {
    assert_eq!(
        ran.status, 1,
        "stdout {:?} stderr {:?}",
        ran.stdout, ran.stderr
    );
    assert_eq!(ran.stdout, "");
    ran.stderr
        .strip_suffix('\n')
        .expect("a line")
        .strip_prefix("xtask: ")
        .expect("xtask:")
}

#[test]
fn the_whole_leg_builds_stages_and_checks_every_entry() {
    let rig = Rig::new("leg-passes");
    let ran = rig.run(BOTH, "11.0", &[]);
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    let out = rig.out();
    let stage = |bin: &str| {
        out.join(format!("stage-{bin}/{bin}-{VERSION}-{TARGET}"))
            .display()
            .to_string()
    };
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(
        lines,
        [
            "js built".to_owned(),
            format!("staged {}: js, LICENSE", stage("js")),
            "js minos 11.0, not above 11.0".to_owned(),
            "js prints \"js 1.2.3\"".to_owned(),
            "js-agent built".to_owned(),
            format!("staged {}: js-agent, LICENSE", stage("js-agent")),
            "js-agent minos 11.0, not above 11.0".to_owned(),
            "js-agent prints \"js-agent 1.2.3\"".to_owned(),
        ]
    );
    for bin in ["js", "js-agent"] {
        assert!(PathBuf::from(stage(bin)).join(bin).is_file());
        assert_eq!(
            fs::read_to_string(PathBuf::from(stage(bin)).join("LICENSE")).unwrap(),
            "licence text\n"
        );
    }
    let leftovers: Vec<String> = fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        leftovers.len(),
        2,
        "only the two archive directories are made: {leftovers:?}"
    );
    let repo = rig.dir.canonicalize().unwrap().display().to_string();
    assert_eq!(
        rig.cargo_log(),
        format!(
            "{repo}|build --locked --release --target {TARGET} -p js-cli --bin js\n\
             {repo}|build --locked --release --target {TARGET} -p js-agent --bin js-agent\n"
        )
    );
}

#[test]
fn a_malformed_builds_entry_is_refused_before_cargo_runs() {
    for entry in ["nocolon", ":bin", "package:"] {
        let rig = Rig::new("leg-bad-entry");
        let ran = rig.run(&format!("js-agent:js-agent {entry}"), "11.0", &[]);
        assert_eq!(
            failed(&ran),
            format!("--builds entry {entry:?} is not <package>:<bin>")
        );
        assert_eq!(rig.cargo_log(), "", "{entry}");
        assert!(!rig.out().exists());
    }
}

#[test]
fn a_floor_that_is_not_a_macos_version_is_refused_before_cargo_runs() {
    let rig = Rig::new("leg-bad-floor");
    let ran = rig.run(BOTH, "eleven", &[]);
    assert_eq!(
        failed(&ran),
        "not a macOS version of dotted numbers: \"eleven\""
    );
    assert_eq!(rig.cargo_log(), "");
}

#[test]
fn a_checkout_without_a_readable_workspace_version_is_refused_before_cargo_runs() {
    let rig = Rig::new("leg-no-version");
    fs::write(rig.path("Cargo.toml"), "[package]\n").unwrap();
    let ran = rig.run(BOTH, "11.0", &[]);
    assert!(failed(&ran).starts_with("Cargo.toml: "), "{}", ran.stderr);
    fs::remove_file(rig.path("Cargo.toml")).unwrap();
    let ran = rig.run(BOTH, "11.0", &[]);
    let why = failed(&ran);
    assert!(
        why.starts_with(&format!("{}/Cargo.toml: No such file", rig.dir.display())),
        "{why}"
    );
    assert_eq!(rig.cargo_log(), "");
}

#[test]
fn a_failing_build_ends_the_run_naming_the_command() {
    let mut rig = Rig::new("leg-build-fails");
    rig.failing = "js-agent".to_owned();
    let ran = rig.run(BOTH, "11.0", &[]);
    assert_eq!(
        failed(&ran),
        format!(
            "{} build --locked --release --target {TARGET} -p js-agent --bin js-agent failed (exit status: 3)",
            rig.path("cargo").display()
        )
    );
    // The first entry was done; the second was not staged.
    assert!(
        rig.out()
            .join(format!("stage-js/js-{VERSION}-{TARGET}"))
            .is_dir()
    );
    assert!(!rig.out().join("stage-js-agent").exists());
}

#[test]
fn a_cargo_that_cannot_be_started_is_named() {
    let rig = Rig::new("leg-no-cargo");
    let ran = rig.run(BOTH, "11.0", &["--cargo", "/nonexistent/cargo"]);
    assert_eq!(
        failed(&ran),
        "cannot run /nonexistent/cargo: No such file or directory (os error 2)"
    );
}

#[test]
fn an_archive_directory_that_exists_already_fails_the_leg() {
    let rig = Rig::new("leg-stage-fails");
    let top = rig.out().join(format!("stage-js/js-{VERSION}-{TARGET}"));
    fs::create_dir_all(&top).unwrap();
    let ran = rig.run(BOTH, "11.0", &[]);
    assert_eq!(
        failed(&ran),
        format!("{}: File exists (os error 17)", top.display())
    );
}

#[test]
fn an_otool_that_fails_ends_the_run_with_its_message() {
    let mut rig = Rig::new("leg-otool-fails");
    rig.otool = "echo 'no such file' >&2\nexit 1\n".to_owned();
    let ran = rig.run(BOTH, "11.0", &[]);
    assert_eq!(
        failed(&ran),
        format!(
            "{} -l {}/target/{TARGET}/release/js failed (exit status: 1): no such file",
            rig.path("otool").display(),
            rig.dir.display()
        )
    );
}

#[test]
fn a_listing_without_a_minos_after_lc_build_version_is_refused() {
    for listing in [
        "Load command 0\n      cmd LC_SEGMENT_64\n",
        "    minos 10.0\n      cmd LC_BUILD_VERSION\n  platform 1\n",
        "      cmd LC_BUILD_VERSION\n    minos\n",
    ] {
        let mut rig = Rig::new("leg-no-minos");
        rig.otool = otool_listing(listing);
        let ran = rig.run(BOTH, "11.0", &[]);
        assert_eq!(
            failed(&ran),
            "js has no LC_BUILD_VERSION minos to check",
            "{listing}"
        );
    }
}

#[test]
fn a_minos_that_is_not_a_number_is_refused() {
    let mut rig = Rig::new("leg-minos-text");
    rig.otool = minos("sonoma");
    let ran = rig.run(BOTH, "11.0", &[]);
    assert_eq!(
        failed(&ran),
        "not a macOS version of dotted numbers: \"sonoma\""
    );
}

#[test]
fn a_minos_above_the_floor_fails_and_one_at_or_below_it_passes_compared_as_numbers() {
    // (minos, floor, passes)
    let cases = [
        ("12.0", "11.0", false),
        ("11.1", "11.0", false),
        ("11.0.1", "11.0", false),
        ("11.0", "11.0", true),
        ("11", "11.0", true),
        ("11.0", "11", true),
        ("10.15", "11.0", true),
        ("10.15", "10.9", false),
        ("10.9", "10.15", true),
    ];
    for (found, floor, passes) in cases {
        let mut rig = Rig::new("leg-minos-compare");
        rig.otool = minos(found);
        let ran = rig.run(BOTH, floor, &[]);
        if passes {
            assert_eq!(ran.status, 0, "{found} vs {floor}: {}", ran.stderr);
        } else {
            assert_eq!(
                failed(&ran),
                format!("js needs macOS {found}, above the pinned {floor}")
            );
        }
    }
}

#[test]
fn a_version_line_that_does_not_end_with_the_version_is_refused() {
    for (line, shown) in [
        ("%s 1.2.30", "js 1.2.30"),
        ("%s 1.2.4", "js 1.2.4"),
        ("%s-1.2.3", "js-1.2.3"),
        ("", ""),
    ] {
        let mut rig = Rig::new("leg-version-line");
        rig.version_line = line.to_owned();
        let ran = rig.run(BOTH, "11.0", &[]);
        assert_eq!(
            failed(&ran),
            format!("js prints {shown:?}, not the version {VERSION}")
        );
    }
}

#[test]
fn a_binary_that_cannot_be_run_is_named() {
    let mut rig = Rig::new("leg-not-runnable");
    rig.mode = 0o644;
    let ran = rig.run(BOTH, "11.0", &[]);
    assert_eq!(
        failed(&ran),
        format!(
            "cannot run {}/target/{TARGET}/release/js: Permission denied (os error 13)",
            rig.dir.display()
        )
    );
}
