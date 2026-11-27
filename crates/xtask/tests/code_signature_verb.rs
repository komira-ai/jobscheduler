//! The code signing of the programs of the macOS package, as code: the table of programs
//! (`xtask::release::mac_programs`), the verb `release check-code-signature` with its port and
//! the fake that answers it, and the privacy profile that grants Full Disk Access to
//! `js-agent`, rendered from the table and held byte for byte to the file in the repository. The
//! workflow that signs the programs is read in macos_code_signing.rs.

use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};

use js_testkit::act;
use js_testkit::dir::TestDir;

use xtask::release::code_signature::{self, Codesign, Error, SystemCodesign};
use xtask::release::mac_programs::{
    self, APPLICATION_IDENTITY, AUTHORITIES, FULL_DISK_ACCESS_PROGRAM, INSTALL_DIR, PROGRAMS,
    TEAM_ID,
};

js_testkit::act_test_hook!();

const PROFILE: &str = "packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn display_of(identifier: &str) -> String {
    format!(
        "Executable=/x/{identifier}\nIdentifier={identifier}\nFormat=Mach-O thin (arm64)\nCodeDirectory v=20500 size=900 flags=0x10000(runtime) hashes=20+7 location=embedded\nSignature size=9000\nAuthority={}\nAuthority={}\nAuthority={}\nTimestamp=Nov 21, 2026 at 10:00:00\nInfo.plist=not bound\nTeamIdentifier={TEAM_ID}\nSealed Resources=none\n",
        AUTHORITIES[0], AUTHORITIES[1], AUTHORITIES[2]
    )
}

/// What `codesign --display --requirements -` prints for a program signed as the release signs it.
fn requirements_of(program: &mac_programs::Program) -> String {
    format!("designated => {}\n", program.requirement())
}

/// A `codesign` that answers from text and records what it was asked.
struct Fake {
    verify: Result<(), Error>,
    display: Result<String, Error>,
    requirements: Result<String, Error>,
    calls: RefCell<Vec<&'static str>>,
}

impl Fake {
    fn signed(program: &mac_programs::Program) -> Self {
        Fake {
            verify: Ok(()),
            display: Ok(display_of(program.identifier)),
            requirements: Ok(requirements_of(program)),
            calls: RefCell::new(Vec::new()),
        }
    }
}

fn tool_error(why: &str) -> Error {
    Error::Tool {
        command: "codesign".to_owned(),
        why: why.to_owned(),
    }
}

impl Codesign for Fake {
    fn verify(&self, _binary: &Path) -> Result<(), Error> {
        self.calls.borrow_mut().push("verify");
        self.verify.clone_err()
    }
    fn display(&self, _binary: &Path) -> Result<String, Error> {
        self.calls.borrow_mut().push("display");
        self.display.clone_err()
    }
    fn requirements(&self, _binary: &Path) -> Result<String, Error> {
        self.calls.borrow_mut().push("requirements");
        self.requirements.clone_err()
    }
}

/// `Result` of a value and an [`Error`], copied (the error is not `Clone`).
trait CloneErr<T> {
    fn clone_err(&self) -> Result<T, Error>;
}

impl<T: Clone> CloneErr<T> for Result<T, Error> {
    fn clone_err(&self) -> Result<T, Error> {
        match self {
            Ok(v) => Ok(v.clone()),
            Err(Error::Tool { command, why }) => Err(Error::Tool {
                command: command.clone(),
                why: why.clone(),
            }),
            Err(other) => panic!("a fake answers with a tool error only: {other:?}"),
        }
    }
}

fn refusal(result: Result<String, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused { problems, .. }) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

/// Asserts that `fake` is refused as `name` with a problem that holds `says`.
fn refused_with(fake: &Fake, name: &str, says: &str) {
    let problems = refusal(code_signature::check(fake, Path::new("/x/bin"), name));
    assert!(
        problems.iter().any(|p| p.contains(says)),
        "must say {says:?}: {problems:?}"
    );
}

// ---- the table ----

/// The table holds the one program of the package under the identifier the signed program
/// carries, and the program of the grant is that one. The programs that left the package are
/// not in it.
///
/// Catches an identifier that changes (a requirement names it, and so does the profile), a
/// grant holder that is not a program of the package, and a program of an earlier package that
/// is still signed.
#[test]
fn the_table_names_the_one_program_of_the_package() {
    let table: Vec<(&str, &str)> = PROGRAMS.iter().map(|p| (p.name, p.identifier)).collect();
    assert_eq!(table, [("js-agent", "com.komira.js-agent")]);
    assert_eq!(mac_programs::program("js-agent"), Some(PROGRAMS[0]));
    assert_eq!(mac_programs::program("js-mac-session"), None);
    assert_eq!(mac_programs::program("js-mac-provision"), None);
    assert_eq!(mac_programs::program("js-vmm"), None);
    assert_eq!(mac_programs::grant_holder().name, FULL_DISK_ACCESS_PROGRAM);
    assert_eq!(FULL_DISK_ACCESS_PROGRAM, "js-agent");
    assert_eq!(mac_programs::grant_holder(), PROGRAMS[0]);
    assert_eq!(
        PROGRAMS[0].install_path(),
        "/Library/Application Support/js-agent/bin/js-agent"
    );
}

/// The install paths are the package's own (the payload `pkg stage-macos` lays down), the
/// identity is the team's and the authorities start with it.
///
/// Catches a table whose paths drift from the package or whose identity names another team.
#[test]
fn the_table_agrees_with_the_package_and_the_team() {
    let prefix = INSTALL_DIR.trim_start_matches('/');
    let files: Vec<String> = xtask::macpkg::entries()
        .into_iter()
        .map(|e| e.path)
        .collect();
    for p in PROGRAMS {
        assert!(
            files.contains(&p.install_path().trim_start_matches('/').to_owned()),
            "{} is not in the payload under {prefix}",
            p.name
        );
    }
    assert!(APPLICATION_IDENTITY.starts_with("Developer ID Application: "));
    assert!(APPLICATION_IDENTITY.ends_with(&format!("({TEAM_ID})")));
    assert_eq!(AUTHORITIES[0], APPLICATION_IDENTITY);
}

/// The requirement is the identifier, Apple as anchor, the two Developer ID certificate fields
/// and the team, with no code hash, and needs no XML escaping.
///
/// Catches a requirement that pins a hash (a release would not meet it) or drops the team.
#[test]
fn the_requirement_names_the_identifier_the_chain_and_the_team_and_no_hash() {
    assert_eq!(
        PROGRAMS[0].requirement(),
        "identifier \"com.komira.js-agent\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = U7RTX4XD3X"
    );
    for p in PROGRAMS {
        let r = p.requirement();
        assert!(!r.contains("cdhash"), "{r}");
        assert!(!r.contains(['&', '<', '>']), "{r}");
    }
}

// ---- the verb ----

/// A program signed as the release signs it passes, and the three reads happen in order.
///
/// Catches a check that skips a read, or reads in another order (the verification must come
/// first, since a seal that does not hold makes every other answer worthless).
#[test]
fn a_program_signed_as_the_release_signs_it_passes_after_three_reads_in_order() {
    for p in PROGRAMS {
        let fake = Fake::signed(&p);
        let line = code_signature::check(&fake, Path::new("/x/bin"), p.name).expect("passes");
        assert_eq!(
            line,
            format!(
                "/x/bin: signed as {} by team U7RTX4XD3X, hardened runtime, timestamped",
                p.identifier
            )
        );
        assert_eq!(*fake.calls.borrow(), ["verify", "display", "requirements"]);
    }
}

/// A program name outside the table is refused before `codesign` runs.
///
/// Catches a check that reads a signature first and judges the name after, or one that takes a
/// name it does not know as meaning no requirements.
#[test]
fn a_program_outside_the_table_is_refused_before_codesign_runs() {
    let fake = Fake::signed(&PROGRAMS[0]);
    let got = code_signature::check(&fake, Path::new("/x/bin"), "js-vmm");
    assert_eq!(got, Err(Error::UnknownProgram("js-vmm".to_owned())));
    assert!(fake.calls.borrow().is_empty());
    assert_eq!(
        Error::UnknownProgram("js-vmm".to_owned()).to_string(),
        "\"js-vmm\" is not a program of the js-agent package"
    );
}

/// A signature that does not verify ends the check with the reason, and nothing is read after.
///
/// Catches a check that goes on to read an invalid signature and could pass its text.
#[test]
fn a_signature_that_does_not_verify_ends_the_check() {
    let mut fake = Fake::signed(&PROGRAMS[0]);
    fake.verify = Err(tool_error("a sealed resource is missing or invalid"));
    let got = code_signature::check(&fake, Path::new("/x/bin"), "js-agent");
    assert_eq!(
        got,
        Err(tool_error("a sealed resource is missing or invalid"))
    );
    assert_eq!(*fake.calls.borrow(), ["verify"]);
}

/// A failed display or requirements read is the check's failure, not a pass.
///
/// Catches a check that treats a read it could not make as an empty answer.
#[test]
fn a_read_that_fails_fails_the_check() {
    let mut fake = Fake::signed(&PROGRAMS[0]);
    fake.display = Err(tool_error("no display"));
    assert_eq!(
        code_signature::check(&fake, Path::new("/x/bin"), "js-agent"),
        Err(tool_error("no display"))
    );
    let mut fake = Fake::signed(&PROGRAMS[0]);
    fake.requirements = Err(tool_error("no requirements"));
    assert_eq!(
        code_signature::check(&fake, Path::new("/x/bin"), "js-agent"),
        Err(tool_error("no requirements"))
    );
    assert_eq!(*fake.calls.borrow(), ["verify", "display", "requirements"]);
}

/// Each way a signature differs from the release's is refused and named.
///
/// Catches a check that accepts an ad hoc signature (the linker's, which the unsigned build
/// has), another identifier or team, an authority chain that is not the team's Developer ID
/// chain, a signature with no secure timestamp, a code directory without the hardened runtime,
/// and a designated requirement that is not the pinned one (another team, another identifier,
/// a code hash, or none).
#[test]
fn each_way_a_signature_differs_from_the_releases_is_refused_and_named() {
    let p = PROGRAMS[0];
    let good = display_of(p.identifier);
    let chain = format!(
        "Authority={}\nAuthority={}\nAuthority={}\n",
        AUTHORITIES[0], AUTHORITIES[1], AUTHORITIES[2]
    );
    let display_cases: Vec<(String, &str)> = vec![
        (
            good.replace("Signature size=9000", "Signature=adhoc"),
            "the signature is ad hoc",
        ),
        (
            good.replace("Identifier=com.komira.js-agent", "Identifier=js-agent"),
            "the identifier is js-agent, not com.komira.js-agent",
        ),
        (
            good.replace("Identifier=com.komira.js-agent\n", ""),
            "the identifier is absent",
        ),
        (
            good.replace("TeamIdentifier=U7RTX4XD3X", "TeamIdentifier=ABCDE12345"),
            "the team is ABCDE12345, not U7RTX4XD3X",
        ),
        (
            good.replace("TeamIdentifier=U7RTX4XD3X\n", "TeamIdentifier=not set\n"),
            "the team is not set",
        ),
        (
            good.replace("TeamIdentifier=U7RTX4XD3X\n", ""),
            "the team is absent",
        ),
        (good.replace(&chain, ""), "the authorities are []"),
        (
            good.replace("Authority=Apple Root CA\n", ""),
            "the authorities are",
        ),
        (
            good.replace(
                "Authority=Apple Root CA\n",
                "Authority=Apple Root CA\nAuthority=Extra CA\n",
            ),
            "Extra CA",
        ),
        (
            good.replace(
                "Authority=Developer ID Certification Authority\nAuthority=Apple Root CA\n",
                "Authority=Apple Root CA\nAuthority=Developer ID Certification Authority\n",
            ),
            "the authorities are",
        ),
        (
            good.replace(
                "Authority=Developer ID Application: Komira AI, Inc. (U7RTX4XD3X)",
                "Authority=Apple Development: Someone (U7RTX4XD3X)",
            ),
            "the authorities are",
        ),
        (
            good.replace("Timestamp=Nov 21, 2026 at 10:00:00\n", "Signed Time=Nov 21, 2026\n"),
            "the signature has no secure timestamp",
        ),
        (
            good.replace("flags=0x10000(runtime)", "flags=0x0(none)"),
            "the code directory has no hardened runtime flag",
        ),
        (
            good.replace("CodeDirectory v=20500 size=900 flags=0x10000(runtime) hashes=20+7 location=embedded\n", ""),
            "the code directory has no hardened runtime flag",
        ),
        (
            good.replace("flags=0x10000(runtime)", "flags=0x10000"),
            "the code directory has no hardened runtime flag",
        ),
        (
            good.replace("flags=0x10000(runtime)", "flags=0x10000(runtime"),
            "the code directory has no hardened runtime flag",
        ),
        (
            good.replace("CodeDirectory v=20500 size=900 flags=0x10000(runtime)", "CodeDirectory v=20500 size=900"),
            "the code directory has no hardened runtime flag",
        ),
        (
            good.replace("flags=0x10000(runtime)", "flags=0x0(none)")
                .replace("CodeDirectory", "Note flags=0x10000(runtime)\nCodeDirectory"),
            "the code directory has no hardened runtime flag",
        ),
    ];
    for (display, says) in display_cases {
        let mut fake = Fake::signed(&p);
        fake.display = Ok(display);
        refused_with(&fake, p.name, says);
    }
    let wrong_requirement = |text: String| {
        let mut fake = Fake::signed(&p);
        fake.requirements = Ok(text);
        refused_with(&fake, p.name, "the designated requirement is");
    };
    let want = p.requirement();
    wrong_requirement(String::new());
    wrong_requirement(format!(
        "designated => {}\n",
        want.replace("U7RTX4XD3X", "ABCDE12345")
    ));
    wrong_requirement(format!(
        "designated => {}\n",
        want.replace("com.komira.js-agent", "com.komira.js-agent.session")
    ));
    wrong_requirement(format!("designated => {want} and cdhash H\"0123\"\n"));
    wrong_requirement("designated => cdhash H\"0123456789\"\n".to_owned());
    wrong_requirement(format!("{want}\n"));
    wrong_requirement(format!("designated => {want}\ndesignated => {want}\n"));
    // The requirement of another program is not this one's.
    wrong_requirement(requirements_of(&PROGRAMS[0]));
}

/// A refusal names the binary, the program and every problem on one line.
///
/// Catches a message that drops the problems, which the workflow's log is read for.
#[test]
fn a_refusal_names_the_binary_the_program_and_every_problem() {
    let mut fake = Fake::signed(&PROGRAMS[0]);
    fake.display = Ok("Signature=adhoc\n".to_owned());
    let err = code_signature::check(&fake, Path::new("/x/bin"), "js-agent").unwrap_err();
    let text = err.to_string();
    assert!(
        text.starts_with("/x/bin is not signed as js-agent: the signature is ad hoc; "),
        "{text}"
    );
    assert!(text.contains("the identifier is absent, not com.komira.js-agent; the team is absent, not U7RTX4XD3X; "), "{text}");
}

/// A program of the table is refused when its signature carries another program's identifier.
///
/// Catches a check that holds every program to one identifier or to any identifier of the table.
#[test]
fn a_program_signed_under_another_programs_identifier_is_refused() {
    let mut fake = Fake::signed(&PROGRAMS[0]);
    fake.display = Ok(display_of("com.komira.js-agent.session"));
    refused_with(
        &fake,
        "js-agent",
        "the identifier is com.komira.js-agent.session, not com.komira.js-agent",
    );
}

/// A scratch directory for one test, under Cargo's per-target temporary directory.
fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// A `codesign` in `dir`: this test binary, run through a link named for one of the testkit's
/// fakes, answering every call with the act `steps` (`$*` is the arguments it was given).
fn fake_codesign(dir: &Path, steps: &str) -> PathBuf {
    let link = dir.join("xcrun");
    act::link_self(&link).expect("link the test binary");
    std::fs::write(dir.join("xcrun.rules"), format!("*\t-\t{steps}\n")).expect("write rules");
    link
}

/// The system adapter runs the program it is given with the reads' arguments and the binary,
/// and returns what it prints.
///
/// Catches an adapter that passes other arguments (a check of `--verify` without `--strict`, or
/// a display without the requirement) or drops the binary.
#[test]
fn the_system_adapter_passes_the_arguments_of_each_read() {
    let dir = scratch("codesign-arguments");
    let tool = SystemCodesign {
        program: fake_codesign(&dir, "echo $*"),
    };
    assert_eq!(tool.verify(Path::new("/x/b")), Ok(()));
    assert_eq!(
        tool.display(Path::new("/x/b")).as_deref(),
        Ok("--display --verbose=2 /x/b\n")
    );
    assert_eq!(
        tool.requirements(Path::new("/x/b")).as_deref(),
        Ok("--display --requirements - /x/b\n")
    );
}

/// The adapter returns what the program wrote on standard error followed by what it wrote on
/// standard output, as `codesign` writes its report on standard error.
///
/// Catches an adapter that drops standard error (every `--display` read would come back empty
/// on a real `codesign`) or puts it after standard output.
#[test]
fn the_system_adapter_returns_standard_error_then_standard_output() {
    let dir = scratch("codesign-streams");
    let tool = SystemCodesign {
        program: fake_codesign(&dir, "echo out ; echo --stderr err"),
    };
    assert_eq!(tool.display(Path::new("/x/b")).as_deref(), Ok("err\nout\n"));
}

/// An adapter whose program fails reports the command and the exit status, with what the
/// program printed.
///
/// Catches an adapter that takes a failed `codesign` for a signature that verified, or one that
/// loses the command or the reason.
#[test]
fn the_system_adapter_reports_a_failing_program() {
    let dir = scratch("codesign-failing");
    let program = fake_codesign(&dir, "echo --stderr rejected ; exit 1");
    let tool = SystemCodesign {
        program: program.clone(),
    };
    let status = std::process::ExitStatus::from_raw(1 << 8);
    assert_eq!(
        tool.verify(Path::new("/x/b")),
        Err(Error::Tool {
            command: format!("{} --verify --strict --verbose=2 /x/b", program.display()),
            why: format!("{status}: rejected"),
        })
    );
}

/// An adapter whose program cannot start reports the command and the reason the system gives.
///
/// Catches an adapter that takes a program it could not run for a signature that verified, or
/// one that loses the command or the reason.
#[test]
fn the_system_adapter_reports_a_program_that_does_not_start() {
    let dir = scratch("codesign-missing");
    let program = dir.join("codesign");
    let why = std::process::Command::new(&program)
        .output()
        .expect_err("no program there")
        .to_string();
    let tool = SystemCodesign {
        program: program.clone(),
    };
    assert_eq!(
        tool.display(Path::new("/x/b")),
        Err(Error::Tool {
            command: format!("{} --display --verbose=2 /x/b", program.display()),
            why,
        })
    );
}

/// The verb's arguments name the binary, the program and the `codesign` to run, and `run`
/// refuses what the program prints when it is no signature.
///
/// Catches a verb that does not pass `--codesign` through, or one that takes any output as a pass.
#[test]
fn the_verb_runs_the_given_codesign_and_refuses_output_that_is_no_signature() {
    use clap::Parser;
    let dir = scratch("codesign-verb");
    let codesign = fake_codesign(&dir, "echo $*");
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: code_signature::CheckArgs,
    }
    let cli = Cli::parse_from([
        "x",
        "/x/b",
        "--program",
        "js-agent",
        "--codesign",
        codesign.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(cli.args.binary, PathBuf::from("/x/b"));
    assert_eq!(cli.args.codesign, codesign);
    let problems = refusal(code_signature::run(&cli.args));
    assert!(
        problems
            .iter()
            .any(|p| p == "the signature has no secure timestamp"),
        "{problems:?}"
    );
    let default = Cli::parse_from(["x", "/x/b", "--program", "js-agent"]);
    assert_eq!(default.args.codesign, PathBuf::from("/usr/bin/codesign"));
}

// ---- the profile ----

/// The profile in the repository is the one the table renders, byte for byte.
///
/// Catches a profile that drifts from the table: another team, identifier, path or service, a
/// code hash in the requirement, a denial, or a second entry.
#[test]
fn the_committed_profile_is_the_one_the_table_renders() {
    let text = read(PROFILE);
    assert_eq!(mac_programs::profile_problems(&text), Vec::<String>::new());
    assert_eq!(text, mac_programs::privacy_profile());
}

/// The profile grants one service to one program by its installed path and its requirement.
///
/// Catches a profile that grants more than Full Disk Access, to another program than the agent
/// (the root helper, say) or to more than one, or by a path other than the package's.
#[test]
fn the_profile_grants_full_disk_access_to_the_agent_alone() {
    let text = mac_programs::privacy_profile();
    assert_eq!(text.matches("<key>Identifier</key>").count(), 1);
    assert_eq!(text.matches("<key>SystemPolicy").count(), 1);
    assert!(text.contains("<key>SystemPolicyAllFiles</key>"));
    assert!(text.contains(&format!(
        "<key>Identifier</key>\n\t\t\t\t\t\t<string>{INSTALL_DIR}/js-agent</string>"
    )));
    assert!(text.contains("<string>identifier \"com.komira.js-agent\" and anchor apple generic"));
    assert!(!text.contains("js-agent.session"));
    assert!(!text.contains("bin/js-mac-session"));
    assert!(text.contains("<key>IdentifierType</key>\n\t\t\t\t\t\t<string>path</string>"));
    assert!(text.contains("<key>Authorization</key>\n\t\t\t\t\t\t<string>Allow</string>"));
    assert!(text.contains("<key>StaticCode</key>\n\t\t\t\t\t\t<false/>"));
    assert!(text.contains("<string>com.apple.TCC.configuration-profile-policy</string>"));
    assert!(text.contains("<key>PayloadScope</key>\n\t<string>System</string>"));
    assert!(text.contains(&mac_programs::grant_holder().requirement()));
    assert!(!text.contains("cdhash"));
    assert!(!text.contains("Deny"));
    assert_eq!(
        text.matches("com.komira.js-agent.full-disk-access").count(),
        2
    );
}

/// A profile that differs in any one of the ways that matter is named by its line.
///
/// Catches a comparison that passes a profile for another team, another identifier, a code hash,
/// another path, another service, a denial or a second entry, or a file cut short.
#[test]
fn each_way_a_profile_differs_is_named() {
    let text = mac_programs::privacy_profile();
    let holder = mac_programs::grant_holder();
    let entry_start = text.find("\t\t\t\t\t<dict>").expect("the entry");
    let entry_end =
        text.find("\t\t\t\t\t</dict>\n").expect("the entry end") + "\t\t\t\t\t</dict>\n".len();
    let entry = &text[entry_start..entry_end];
    let cases: Vec<(String, &str)> = vec![
        (text.replace("= U7RTX4XD3X", "= ABCDE12345"), "ABCDE12345"),
        (
            text.replace(
                "identifier \"com.komira.js-agent\"",
                "identifier \"com.komira.js-agent.session\"",
            ),
            "com.komira.js-agent.session\\\" and anchor",
        ),
        (
            text.replace(" and anchor apple generic", " and cdhash H\"0123456789012345678901234567890123456789\" and anchor apple generic"),
            "cdhash",
        ),
        (
            text.replace(&holder.install_path(), "/tmp/js-agent"),
            "/tmp/js-agent",
        ),
        (
            text.replace(&holder.install_path(), "/Library/Application Support/js-agent/bin/js-mac-session"),
            "bin/js-mac-session<",
        ),
        (
            text.replace("SystemPolicyAllFiles", "SystemPolicyDesktopFolder"),
            "SystemPolicyDesktopFolder",
        ),
        (text.replace("<string>Allow</string>", "<string>Deny</string>"), "Deny"),
        (text.replace("<false/>", "<true/>"), "<true/>"),
        (text.replace("<string>path</string>", "<string>bundleID</string>"), "bundleID"),
        (text.replace("<string>System</string>", "<string>User</string>"), "User"),
        (text.replace(entry, &format!("{entry}{entry}")), "lines"),
        (text.trim_end().to_owned(), "line ends"),
        (text.replace('\n', "\r\n").replace("\r\n", "\n\n"), "lines"),
    ];
    for (changed, says) in cases {
        let problems = mac_programs::profile_problems(&changed);
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{says:?} not in {problems:?}"
        );
    }
}

/// The verb is reachable as `xtask release check-code-signature`: it exits 1 and says why on
/// stderr for a program outside the table and for a binary whose `codesign` output is no
/// signature.
///
/// Catches a verb that is not wired into the command line, or one whose refusal does not reach
/// the exit status the workflow's step reads.
#[test]
fn the_command_line_refuses_with_status_1_and_names_the_reason() {
    use std::process::Command;
    let xtask = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["release", "check-code-signature"])
            .args(args)
            .output()
            .expect("run xtask")
    };
    let out = xtask(&["/x/bin", "--program", "js-vmm"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "xtask: \"js-vmm\" is not a program of the js-agent package\n"
    );
    let dir = scratch("codesign-command-line");
    let codesign = fake_codesign(&dir, "echo $*");
    let out = xtask(&[
        "/x/bin",
        "--program",
        "js-agent",
        "--codesign",
        codesign.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .starts_with("xtask: /x/bin is not signed as js-agent: "),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A failed read shows the command and the reason in the error's text.
///
/// Catches an error that loses the command line the workflow's log is read for.
#[test]
fn a_tool_error_prints_the_command_and_the_reason() {
    assert_eq!(
        tool_error("exit status: 1").to_string(),
        "codesign: exit status: 1"
    );
}
