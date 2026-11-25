//! `cargo xtask pkg build-macos`, run as the built binary in a scratch checkout (a `LICENSE` and
//! the stand-in programs, small shell scripts in the scratch directory). Every stand-in appends
//! its name and arguments to `commands.log` in the checkout, so a test reads the commands in the
//! order they ran. The stand-in `cargo` copies the stand-in programs to `target/release`; the
//! stand-in `codesign` prints the cdhash lines the test chooses; the stand-in `pkgbuild` and
//! `productbuild` write their last argument; the stand-in `pkgutil` and `lsbom` make and list
//! the expanded package the test chooses. Nothing is built and the real tools are never started.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::dir::TestDir;
use js_testkit::spawn::install_script;
use xtask::macpkg::{Kind, entries};

const VERSION: &str = "1.2.3";
const PKG: &str = "js-agent-1.2.3-macos-arm64.pkg";
const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
const STAGED_AGENT: &str = "stage/root/Library/Application Support/js-agent/bin/js-agent";

/// What one run printed.
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
}

/// A scratch checkout and the stand-ins, with the choices a test changes.
struct Rig {
    dir: TestDir,
    /// The stand-in that fails (exits 3 with `boom` on its standard error), by name.
    fail: &'static str,
    /// The first `--version` line of `js-mac-session`.
    session_line: String,
    /// What the stand-in `codesign` writes for the built `js-agent`.
    built_out: String,
    /// What it writes for the staged `js-agent`.
    staged_out: String,
    /// The shell commands of the stand-in `pkgutil` after it is asked to expand into `$3`.
    expand: String,
    /// The shell commands of the stand-in `lsbom`.
    list: String,
}

fn codesign_out(hash: &str) -> String {
    format!("Executable=/x/js-agent\nIdentifier=js-agent\nCDHash={hash}\n")
}

/// The listing `lsbom` prints for the package's entries, as the package check reads it.
fn bom() -> String {
    let mut text = String::from(".\t40755\t0/0\n");
    for e in entries() {
        let kind = match e.kind {
            Kind::Dir => 0o040_000,
            Kind::File => 0o100_000,
        };
        text.push_str(&format!("./{}\t{:o}\t0/0\n", e.path, kind | e.mode));
    }
    text
}

const PACKAGE_INFO: &str = "<pkg-info identifier=\"com.komira.js-agent\" version=\"1.2.3\"/>\n";

impl Rig {
    fn new(name: &str) -> Rig {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        fs::write(dir.join("LICENSE"), "licence text\n").unwrap();
        fs::write(dir.join("bom.txt"), bom()).unwrap();
        fs::write(dir.join("pkginfo.txt"), PACKAGE_INFO).unwrap();
        fs::create_dir(dir.join("fixtures")).unwrap();
        Rig {
            dir,
            fail: "",
            session_line: format!("js-mac-session {VERSION}"),
            built_out: codesign_out(HASH),
            staged_out: codesign_out(HASH),
            expand: "mkdir -p \"$3\"\ncp pkginfo.txt \"$3/PackageInfo\"\n: > \"$3/Bom\"\n"
                .to_owned(),
            list: "cat bom.txt\n".to_owned(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.path("commands.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn script(&self, at: &str, text: &str) -> PathBuf {
        let path = self.path(at);
        install_script(&path, &format!("#!/bin/sh\n{text}"), "755");
        path
    }

    /// A stand-in that logs `name|<shown>` and then fails or runs `body`.
    fn stand_in(&self, name: &str, shown: &str, body: &str) -> PathBuf {
        let fail = if self.fail == name {
            "echo boom >&2\nexit 3\n"
        } else {
            ""
        };
        self.script(
            name,
            &format!("printf '%s|%s\\n' {name} \"{shown}\" >> commands.log\n{fail}{body}"),
        )
    }

    /// The program the stand-in `cargo` copies to `target/release`.
    fn program(&self, bin: &str, body: &str) {
        self.script(
            &format!("fixtures/{bin}"),
            &format!("printf '%s|%s\\n' {bin} \"$*\" >> commands.log\n{body}"),
        );
    }

    /// Runs the verb with `more` flags after the stand-ins, in the scratch checkout.
    fn run_with(&self, more: &[&str]) -> Ran {
        self.program("js-agent", "printf 'js-agent 1.2.3\\nsecond line\\n'\n");
        self.program(
            "js-mac-session",
            &format!("printf '%s\\n' '{}'\n", self.session_line),
        );
        self.program(
            "js-mac-provision",
            "case \"$1\" in\n\
               --version) echo 'js-mac-provision 1.2.3';;\n\
               agent-plist) echo agent-plist-body;;\n\
               session-plist) echo session-plist-body;;\n\
               pf-pool-plist) echo pf-pool-plist-body;;\n\
             esac\n",
        );
        let cargo = self.stand_in(
            "cargo",
            "$*",
            "for b; do :; done\nmkdir -p target/release\ncp \"fixtures/$b\" \"target/release/$b\"\n",
        );
        let codesign = self.stand_in(
            "codesign",
            "$*",
            &format!(
                "case \"$*\" in\n*stage/root*) printf '%s' '{}' >&2;;\n*) printf '%s' '{}' >&2;;\nesac\n",
                self.staged_out, self.built_out
            ),
        );
        let plutil = self.stand_in("plutil", "$*", "");
        let last = "for last; do :; done\n";
        let pkgbuild = self.stand_in(
            "pkgbuild",
            "$*",
            &format!("{last}printf 'component' > \"$last\"\n"),
        );
        let productbuild = self.stand_in(
            "productbuild",
            "$*",
            &format!("{last}printf 'package bytes' > \"$last\"\n"),
        );
        let pkgutil = self.stand_in("pkgutil", "$1 $2", &self.expand);
        let lsbom = self.stand_in("lsbom", "", &self.list);
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args(["pkg", "build-macos", "--version", VERSION])
            .current_dir(self.dir.path());
        for (flag, program) in [
            ("--cargo", &cargo),
            ("--codesign", &codesign),
            ("--plutil", &plutil),
            ("--pkgbuild", &pkgbuild),
            ("--productbuild", &productbuild),
            ("--pkgutil", &pkgutil),
            ("--lsbom", &lsbom),
        ] {
            if !more.contains(&flag) {
                command.arg(flag).arg(program);
            }
        }
        let output = command.args(more).output().unwrap();
        Ran {
            status: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }

    fn run(&self) -> Ran {
        self.run_with(&[])
    }

    /// The message of a run that ended with status 1.
    fn failed(&self) -> String {
        let ran = self.run();
        failed(&ran)
    }

    fn stand_in_path(&self, name: &str) -> String {
        self.path(name).display().to_string()
    }
}

fn failed(ran: &Ran) -> String {
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
        .to_owned()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// The commands of a run that gets as far as `last`, `<name>|<arguments>` each, in order.
fn commands(through: usize) -> Vec<String> {
    let all = [
        "cargo|build --locked --release -p js-agent --bin js-agent",
        "cargo|build --locked --release -p js-mac-session --bin js-mac-session",
        "cargo|build --locked --release -p js-mac-provision --bin js-mac-provision",
        "js-agent|--version",
        "js-mac-session|--version",
        "js-mac-provision|--version",
        "codesign|--display --verbose=4 target/release/js-agent",
        "js-mac-provision|agent-plist",
        "js-mac-provision|session-plist --daemon-requirement cdhash H\"0123456789abcdef0123456789abcdef01234567\"",
        "js-mac-provision|pf-pool-plist",
        "plutil|-lint inputs/agent.plist inputs/session.plist inputs/pf-pool.plist",
        "codesign|--display --verbose=4 stage/root/Library/Application Support/js-agent/bin/js-agent",
        "pkgbuild|--root stage/root --identifier com.komira.js-agent --version 1.2.3 --install-location / --ownership recommended stage/component.pkg",
        "productbuild|--package stage/component.pkg --version 1.2.3 dist/js-agent-1.2.3-macos-arm64.pkg",
        "pkgutil|--expand-full dist/js-agent-1.2.3-macos-arm64.pkg",
        "lsbom|",
    ];
    all[..through].iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn the_package_is_built_checked_and_summed_with_the_commands_of_the_action_in_order() {
    let rig = Rig::new("build-passes");
    let ran = rig.run();
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(rig.log(), commands(16));
    assert_eq!(
        ran.stdout.lines().collect::<Vec<_>>(),
        [
            "js-agent built",
            "js-mac-session built",
            "js-mac-provision built",
            "js-agent prints \"js-agent 1.2.3\"",
            "js-mac-session prints \"js-mac-session 1.2.3\"",
            "js-mac-provision prints \"js-mac-provision 1.2.3\"",
            "staged js-agent 1.2.3 with 7 files and 7 directories in stage",
            "dist/js-agent-1.2.3-macos-arm64.pkg: js-agent 1.2.3 with 14 entries",
            "summed js-agent-1.2.3-macos-arm64.pkg and js-agent, js-mac-session, js-mac-provision",
        ]
    );
    assert_eq!(read(&rig.path("inputs/agent.plist")), "agent-plist-body\n");
    assert_eq!(
        read(&rig.path("inputs/session.plist")),
        "session-plist-body\n"
    );
    assert_eq!(
        read(&rig.path("inputs/pf-pool.plist")),
        "pf-pool-plist-body\n"
    );
    assert_eq!(read(&rig.path("stage/component.pkg")), "component");
    assert_eq!(read(&rig.path(&format!("dist/{PKG}"))), "package bytes");
    assert_eq!(
        read(&rig.path("dist/SHA256SUMS")),
        format!("2e547448dcd0f2fcd9dbc386d33f1553369883451898177559bcf3e3b1083d16  {PKG}\n")
    );
    assert_eq!(
        read(&rig.path("target/release/PROGRAMS.SHA256SUMS")),
        "ced37c426b001ac38c56ce85c94eb005819cda357d4ce59e32e5a3f340c5f9a8  js-agent\n\
         ccb35897821efd737f03f59af9d8ac045bb1552d4fac4c08e85175e9e9d08895  js-mac-session\n\
         a95022ac9002c15653b7209405220db46563c37c2ff58369381faa112bf79230  js-mac-provision\n"
    );
    let staged = rig.path(STAGED_AGENT);
    assert_eq!(
        read(&staged),
        read(&rig.path("target/release/js-agent")),
        "the staged js-agent is the built one"
    );
}

#[test]
fn a_package_expanded_with_its_component_in_a_directory_of_its_own_passes() {
    let mut rig = Rig::new("build-nested-component");
    rig.expand =
        "mkdir -p \"$3/inner\"\ncp pkginfo.txt \"$3/inner/PackageInfo\"\n: > \"$3/inner/Bom\"\n"
            .to_owned();
    let ran = rig.run();
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(rig.log(), commands(16));
}

#[test]
fn a_cargo_that_fails_ends_the_run_before_any_program_is_run() {
    let mut rig = Rig::new("build-cargo-fails");
    rig.fail = "cargo";
    let message = rig.failed();
    assert_eq!(
        message,
        format!(
            "`{} build --locked --release -p js-agent --bin js-agent` ended with exit status: 3: boom",
            rig.stand_in_path("cargo")
        )
    );
    assert_eq!(rig.log(), commands(1));
}

#[test]
fn a_cargo_that_does_not_start_is_named() {
    let rig = Rig::new("build-no-cargo");
    let missing = rig.stand_in_path("no-such-cargo");
    let ran = rig.run_with(&["--cargo", &missing]);
    assert_eq!(
        failed(&ran),
        format!("{missing}: No such file or directory (os error 2)")
    );
    assert_eq!(rig.log(), Vec::<String>::new());
}

#[test]
fn a_program_that_prints_another_version_ends_the_run() {
    let mut rig = Rig::new("build-wrong-version");
    rig.session_line = "js-mac-session 1.2.4".to_owned();
    assert_eq!(
        rig.failed(),
        "js-mac-session prints 'js-mac-session 1.2.4', not 1.2.3"
    );
    assert_eq!(rig.log(), commands(5));
}

#[test]
fn a_program_that_prints_nothing_is_not_the_version() {
    let mut rig = Rig::new("build-no-version-line");
    rig.session_line = String::new();
    assert_eq!(rig.failed(), "js-mac-session prints '', not 1.2.3");
}

#[test]
fn a_codesign_that_fails_ends_the_run() {
    let mut rig = Rig::new("build-codesign-fails");
    rig.fail = "codesign";
    assert_eq!(
        rig.failed(),
        format!(
            "`{} --display --verbose=4 target/release/js-agent` ended with exit status: 3: boom",
            rig.stand_in_path("codesign")
        )
    );
    assert_eq!(rig.log(), commands(7));
}

#[test]
fn a_cdhash_that_is_not_forty_lower_case_hex_digits_ends_the_run() {
    for (name, out, found) in [
        ("none", "Identifier=js-agent\n".to_owned(), ""),
        ("short", codesign_out("abc"), "abc"),
        (
            "upper",
            codesign_out("0123456789ABCDEF0123456789abcdef01234567"),
            "0123456789ABCDEF0123456789abcdef01234567",
        ),
        (
            "not hex",
            codesign_out("0123456789abcdefg123456789abcdef01234567"),
            "0123456789abcdefg123456789abcdef01234567",
        ),
        (
            "two lines",
            format!("{}CDHash=ff\n", codesign_out("aa")),
            "aa\nff",
        ),
    ] {
        let mut rig = Rig::new(&format!("build-cdhash-{}", name.replace(' ', "-")));
        rig.built_out = out;
        assert_eq!(
            rig.failed(),
            format!("the cdhash of js-agent is '{found}', not 40 hex digits"),
            "{name}"
        );
        assert_eq!(rig.log(), commands(7), "{name}");
        assert!(!rig.path("inputs").exists(), "{name}");
    }
}

#[test]
fn an_inputs_directory_that_exists_ends_the_run_before_the_plists() {
    let rig = Rig::new("build-inputs-exists");
    fs::create_dir(rig.path("inputs")).unwrap();
    let message = rig.failed();
    assert_eq!(message, "inputs: File exists (os error 17)");
    assert_eq!(rig.log(), commands(7));
}

#[test]
fn a_plutil_that_fails_ends_the_run_before_the_stage() {
    let mut rig = Rig::new("build-plutil-fails");
    rig.fail = "plutil";
    assert_eq!(
        rig.failed(),
        format!(
            "`{} -lint inputs/agent.plist inputs/session.plist inputs/pf-pool.plist` ended with exit status: 3: boom",
            rig.stand_in_path("plutil")
        )
    );
    assert_eq!(rig.log(), commands(11));
    assert!(!rig.path("stage").exists());
}

#[test]
fn a_stage_directory_that_exists_ends_the_run() {
    let rig = Rig::new("build-stage-exists");
    fs::create_dir(rig.path("stage")).unwrap();
    assert_eq!(
        rig.failed(),
        "stage: exists; stage into a directory that is not there"
    );
    assert_eq!(rig.log(), commands(11));
}

#[test]
fn a_staged_agent_with_another_cdhash_ends_the_run_before_the_package_is_built() {
    let mut rig = Rig::new("build-staged-cdhash");
    rig.staged_out = codesign_out("ffffffffffffffffffffffffffffffffffffffff");
    assert_eq!(
        rig.failed(),
        format!(
            "the staged js-agent has the cdhash 'ffffffffffffffffffffffffffffffffffffffff', not '{HASH}'"
        )
    );
    assert_eq!(rig.log(), commands(12));
    assert!(!rig.path("dist").exists());
}

/// Catches a run refused because `dist` is there before it starts: a farm Mac's runner makes the
/// parent directory of every output the action declares, `dist` among them.
#[test]
fn a_dist_directory_made_before_the_run_holds_the_package() {
    let rig = Rig::new("build-dist-made");
    fs::create_dir(rig.path("dist")).unwrap();
    let ran = rig.run();
    assert_eq!((ran.status, ran.stderr.as_str()), (0, ""), "{}", ran.stdout);
    assert_eq!(rig.log(), commands(16));
    assert!(rig.path(&format!("dist/{PKG}")).is_file());
}

/// Catches a `dist` that is a file taken as the directory: the run ends before `pkgbuild`.
#[test]
fn a_dist_that_is_a_file_ends_the_run_before_pkgbuild() {
    let rig = Rig::new("build-dist-file");
    fs::write(rig.path("dist"), "").unwrap();
    assert_eq!(rig.failed(), "dist: File exists (os error 17)");
    assert_eq!(rig.log(), commands(12));
}

#[test]
fn a_pkgbuild_that_fails_ends_the_run_before_productbuild() {
    let mut rig = Rig::new("build-pkgbuild-fails");
    rig.fail = "pkgbuild";
    let message = rig.failed();
    assert!(
        message.ends_with(
            "--ownership recommended stage/component.pkg` ended with exit status: 3: boom"
        ),
        "{message}"
    );
    assert_eq!(rig.log(), commands(13));
}

#[test]
fn a_productbuild_that_fails_ends_the_run_before_the_package_is_read() {
    let mut rig = Rig::new("build-productbuild-fails");
    rig.fail = "productbuild";
    let message = rig.failed();
    assert!(
        message.ends_with(&format!("dist/{PKG}` ended with exit status: 3: boom")),
        "{message}"
    );
    assert_eq!(rig.log(), commands(14));
    assert!(!rig.path("dist/SHA256SUMS").exists());
}

#[test]
fn a_package_with_a_script_is_refused_and_not_summed() {
    let mut rig = Rig::new("build-script-refused");
    rig.expand = format!(
        "{}mkdir -p \"$3/Scripts\"\n: > \"$3/Scripts/postinstall\"\n",
        rig.expand
    );
    assert_eq!(
        rig.failed(),
        "the package is not the js-agent package: the package holds scripts, and must hold none: [\"postinstall\"]"
    );
    assert!(!rig.path("dist/SHA256SUMS").exists());
}

#[test]
fn a_package_whose_bill_of_materials_misses_an_entry_is_refused() {
    let mut rig = Rig::new("build-bom-short");
    rig.list = "grep -v LICENSE bom.txt\n".to_owned();
    assert_eq!(
        rig.failed(),
        "the package is not the js-agent package: missing from the package: Library/Application Support/js-agent/LICENSE"
    );
}

#[test]
fn an_archive_with_no_component_is_refused() {
    let mut rig = Rig::new("build-no-component");
    rig.expand = "mkdir -p \"$3\"\n".to_owned();
    assert_eq!(
        rig.failed(),
        format!("dist/{PKG} holds 0 components, not one")
    );
}

#[test]
fn an_archive_that_pkgutil_did_not_expand_is_refused() {
    let mut rig = Rig::new("build-not-expanded");
    rig.expand = String::new();
    let message = rig.failed();
    assert!(
        message.ends_with("No such file or directory (os error 2)"),
        "{message}"
    );
}

#[test]
fn a_component_without_package_info_is_refused() {
    let mut rig = Rig::new("build-no-package-info");
    rig.expand = "mkdir -p \"$3\"\n: > \"$3/Bom\"\n".to_owned();
    let message = rig.failed();
    assert!(
        message.ends_with("PackageInfo: No such file or directory (os error 2)"),
        "{message}"
    );
}

#[test]
fn a_pkgutil_that_fails_is_named() {
    let mut rig = Rig::new("build-pkgutil-fails");
    rig.fail = "pkgutil";
    assert_eq!(
        rig.failed(),
        format!("{} failed: boom", rig.stand_in_path("pkgutil"))
    );
}

#[test]
fn a_pkgutil_that_does_not_start_is_named() {
    let rig = Rig::new("build-no-pkgutil");
    let missing = rig.stand_in_path("no-such-pkgutil");
    let ran = rig.run_with(&["--pkgutil", &missing]);
    assert_eq!(
        failed(&ran),
        format!("{missing}: No such file or directory (os error 2)")
    );
}

#[test]
fn an_lsbom_that_fails_is_named() {
    let mut rig = Rig::new("build-lsbom-fails");
    rig.fail = "lsbom";
    assert_eq!(
        rig.failed(),
        format!("{} failed: boom", rig.stand_in_path("lsbom"))
    );
}

#[test]
fn a_version_that_is_not_major_minor_patch_is_refused_before_anything_runs() {
    let rig = Rig::new("build-bad-version");
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["pkg", "build-macos", "--version", "1.2"])
        .current_dir(rig.dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("invalid value '1.2' for '--version <VERSION>'"),
        "{stderr}"
    );
    assert_eq!(rig.log(), Vec::<String>::new());
}

/// The lines of the composite action that start with `prefix`, with `$VERSION` filled in and
/// the quotes taken off.
fn action_lines(action: &str, prefix: &str) -> Vec<String> {
    action
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(prefix))
        .map(|line| line.replace("$VERSION", VERSION).replace('"', ""))
        .collect()
}

#[test]
fn the_cargo_builds_and_the_package_commands_are_the_composite_actions() {
    let action = read(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.github/actions/js-agent-macos-pkg/action.yml"),
    );
    let rig = Rig::new("build-equals-action");
    let ran = rig.run();
    assert_eq!(ran.status, 0, "{}", ran.stderr);
    let logged = |program: &str| -> Vec<String> {
        rig.log()
            .iter()
            .filter_map(|line| line.split_once('|'))
            .filter(|(name, _)| *name == program)
            .map(|(name, args)| format!("{name} {args}"))
            .collect()
    };
    for (program, prefix) in [
        ("cargo", "cargo build "),
        ("pkgbuild", "pkgbuild "),
        ("productbuild", "productbuild "),
    ] {
        let wanted = action_lines(&action, prefix);
        assert_eq!(
            wanted.len(),
            if program == "cargo" { 3 } else { 1 },
            "{program}: {wanted:?}"
        );
        assert_eq!(logged(program), wanted, "{program}");
    }
}
