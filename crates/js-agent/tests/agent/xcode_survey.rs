//! The survey of a Mac's Xcodes: only an Xcode that answers every question of the catalog
//! is in the hello's `xcode` and held by the runtime, and the simulator runtimes of the
//! ready Xcodes that have `simctl` are the hello's `sim.<platform>` capabilities. The apps
//! are real directories in a scratch tree (the catalog resolves them and checks who may
//! write them); every program is a fake.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::xcode::{Site, Surveyed, Xcode, survey};
use js_agent::{CommandRunner, Streams};
use serde_json::{Value, json};

use crate::support::{
    FakeCommands, FakeFiles, RanWith, Scratch, code, mac_host, run_scripted, welcome,
};
use crate::xcodes::version_plist;

/// The uid that owns the scratch directories, which the catalog accepts besides root.
pub fn owner() -> u32 {
    fs::metadata(env!("CARGO_TARGET_TMPDIR"))
        .expect("the target directory")
        .uid()
}

/// A scratch `Applications` directory (its real path) holding the apps `(name, build)`,
/// each a real `Contents/Developer` tree, and the fake files that list it and hold each
/// app's `version.plist`.
pub fn applications(scratch: &Scratch, apps: &[(&str, &str)]) -> (PathBuf, FakeFiles) {
    // Whatever the umask: a group-writable directory above an app is not trusted.
    fs::set_permissions(scratch.path(""), fs::Permissions::from_mode(0o755)).unwrap();
    let dir = scratch.path("Applications");
    app_dir(&dir);
    let dir = fs::canonicalize(&dir).unwrap();
    let names: Vec<&str> = apps.iter().map(|(name, _)| *name).collect();
    let mut files = FakeFiles::default().with_dir(&dir.to_string_lossy(), &names);
    for (name, build) in apps {
        app_dir(&dir.join(name));
        let plist = dir.join(name).join("Contents/version.plist");
        files = files.with(&plist.to_string_lossy(), &version_plist(build));
    }
    (dir, files)
}

/// Makes the app directory `app` and its `Contents/Developer`, each writable by its owner
/// only, whatever the umask.
fn app_dir(app: &Path) {
    let mut made = app.to_path_buf();
    for part in ["", "Contents", "Developer"] {
        made.push(part);
        fs::create_dir_all(&made).unwrap();
        fs::set_permissions(&made, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The `DEVELOPER_DIR` of the app `name` in `dir`.
pub fn developer(dir: &Path, name: &str) -> String {
    format!("{}/{name}/Contents/Developer", dir.display())
}

/// `xcodebuild -version` of an Xcode of `version` and `build`.
pub fn version_text(version: &str, build: &str) -> String {
    format!("Xcode {version}\nBuild version {build}\n")
}

/// A `simctl list runtimes -j` listing of `runtimes`: `(identifier suffix, version,
/// available)`.
fn listing(runtimes: &[(&str, &str, bool)]) -> String {
    let runtimes: Vec<Value> = runtimes
        .iter()
        .map(|(id, version, available)| {
            json!({
                "identifier": format!("com.apple.CoreSimulator.SimRuntime.{id}"),
                "version": version,
                "isAvailable": available,
                "name": id,
            })
        })
        .collect();
    json!({ "runtimes": runtimes }).to_string()
}

/// A runner that answers each program run with both streams read from
/// `answer(line, DEVELOPER_DIR)`, where the line is the program and its arguments joined
/// with spaces, and records each such line with its whole environment. The probes of the
/// host (run without their streams read) are answered as [`mac_host`]'s.
struct Answering<F> {
    answer: F,
    ran: RefCell<Vec<RanWith>>,
    probes: FakeCommands,
}

impl<F: Fn(&str, &str) -> Result<String, String>> Answering<F> {
    fn new(answer: F) -> Self {
        Self {
            answer,
            ran: RefCell::new(Vec::new()),
            probes: mac_host().commands,
        }
    }

    fn lines(&self) -> Vec<String> {
        self.ran
            .borrow()
            .iter()
            .map(|(line, _)| line.clone())
            .collect()
    }
}

impl<F: Fn(&str, &str) -> Result<String, String>> CommandRunner for Answering<F> {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.probes.run(program, args)
    }

    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        self.probes.run_in(program, args, env)
    }

    fn run_streams(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        _keep: usize,
        limit: Duration,
    ) -> Result<Streams, String> {
        assert_eq!(limit, js_agent::xcode::QUESTION_LIMIT);
        let line = format!("{program} {}", args.join(" "));
        let pairs = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned()));
        self.ran.borrow_mut().push((line.clone(), pairs.collect()));
        let dev = env
            .iter()
            .find_map(|(key, value)| (*key == "DEVELOPER_DIR").then_some(*value))
            .unwrap_or_default();
        (self.answer)(&line, dev).map(|stdout| Streams {
            stdout: stdout.into_bytes(),
            ..Streams::default()
        })
    }
}

/// The answer of a ready Xcode of `build` (version 16.2) at `dev` to `line`: every question
/// is yes, `xcrun --find` prints a path inside the Xcode except for `metal`, and `simctl`
/// lists `runtimes`. Any other line is not found.
fn ready(line: &str, dev: &str, build: &str, runtimes: &str) -> Result<String, String> {
    let xcodebuild = format!("{dev}/usr/bin/xcodebuild");
    match line.strip_prefix(&xcodebuild) {
        Some(" -version") => return Ok(version_text("16.2", build)),
        Some(" -license check" | " -checkFirstLaunchStatus") => return Ok(String::new()),
        _ => {}
    }
    match line {
        "/usr/bin/xcrun --find metal" => Err("/usr/bin/xcrun: exit status: 1".to_owned()),
        "/usr/bin/xcrun simctl list runtimes -j" => Ok(runtimes.to_owned()),
        _ => match line.strip_prefix("/usr/bin/xcrun --find ") {
            Some(tool) => Ok(format!("{dev}/usr/bin/{tool}\n")),
            None => Err(format!("{line}: not found")),
        },
    }
}

fn site(dir: &Path) -> Site<'_> {
    Site {
        applications: dir,
        owner: owner(),
    }
}

/// A Mac with one Xcode that answers the survey and one whose licence is not accepted says
/// hello with the first's build only, and the versions of the available simulator runtimes
/// the first lists, one key per platform, ascending; it logs why the second is left out.
/// Every question ran with that Xcode's `DEVELOPER_DIR` as its whole environment. It
/// catches a hello that lists every installed Xcode whatever the survey says, a survey that
/// is never run, and a simulator capability that is missing or holds an unavailable
/// runtime.
#[test]
fn a_mac_offers_only_the_xcodes_that_answer_the_survey_and_their_simulators() {
    let scratch = Scratch::new("xcode-survey-hello");
    let apps = [("Xcode-16.2.app", "16C5032a"), ("Xcode-16.4.app", "16F6")];
    let (dir, files) = applications(&scratch, &apps);
    let (good, bad) = (
        developer(&dir, "Xcode-16.2.app"),
        developer(&dir, "Xcode-16.4.app"),
    );
    let runtimes = listing(&[
        ("iOS-18-2", "18.2", true),
        ("iOS-17-5", "17.5", true),
        ("iOS-19-0", "19.0", false),
        ("watchOS-11-0", "11.0", true),
    ]);
    let mut host = mac_host();
    host.applications = dir.clone();
    host.xcode_owner = owner();
    host.files = files;
    host.commands = host
        .commands
        .with(
            &format!("{good}/usr/bin/xcodebuild -version"),
            Ok(&version_text("16.2", "16C5032a")),
        )
        .with(&format!("{good}/usr/bin/xcodebuild -license check"), Ok(""))
        .with(
            &format!("{good}/usr/bin/xcodebuild -checkFirstLaunchStatus"),
            Ok(""),
        )
        .with(
            "/usr/bin/xcrun --find clang",
            Ok(&format!("{good}/usr/bin/clang\n")),
        )
        .with(
            "/usr/bin/xcrun --find swiftc",
            Ok(&format!("{good}/usr/bin/swiftc\n")),
        )
        .with(
            "/usr/bin/xcrun --find simctl",
            Ok(&format!("{good}/usr/bin/simctl\n")),
        )
        .with("/usr/bin/xcrun simctl list runtimes -j", Ok(&runtimes))
        .with(
            &format!("{bad}/usr/bin/xcodebuild -version"),
            Ok(&version_text("16.4", "16F6")),
        )
        .with(
            &format!("{bad}/usr/bin/xcodebuild -license check"),
            Err(&format!("{bad}/usr/bin/xcodebuild: exit status: 69")),
        );
    let (ran, sent, _) = run_scripted(&[], &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(
        sent.borrow()[0].1["capabilities"],
        json!({
            "os": "macos",
            "arch": "arm64",
            "os_version": "26",
            "xcode": "16C5032a",
            "sim.ios": "17.5,18.2",
            "sim.watchos": "11.0",
        })
    );
    let warning = format!(
        "js-agent: warning: {}/Xcode-16.4.app: Xcode 16F6 is not offered: its licence is not \
         accepted: {bad}/usr/bin/xcodebuild: exit status: 69\n",
        dir.display()
    );
    assert!(ran.err.contains(&warning), "{}", ran.err);
    let envs = host.commands.envs.borrow();
    let surveyed: Vec<&RanWith> = envs
        .iter()
        .filter(|(line, _)| line.contains("xcodebuild") || line.starts_with("/usr/bin/xcrun"))
        .collect();
    assert_eq!(surveyed.len(), 10, "{surveyed:?}");
    for (line, env) in surveyed {
        let dev = match line.starts_with(&bad) {
            true => &bad,
            false => &good,
        };
        assert_eq!(env, &[("DEVELOPER_DIR".to_owned(), dev.clone())], "{line}");
    }
}

/// A Mac with no Xcode says hello with neither `xcode` nor any `sim.` key and runs no
/// program of the survey. It catches an empty value, which the server would take as a
/// build or a version, and a survey or simulator listing on a Mac that has nothing to ask.
#[test]
fn a_mac_without_xcode_reports_no_xcode_and_no_simulator_and_runs_no_survey() {
    let host = mac_host();
    let (ran, sent, _) = run_scripted(&[], &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let capabilities = sent.borrow()[0].1["capabilities"].clone();
    let keys: Vec<&String> = capabilities.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["arch", "os", "os_version"]);
    let ran_lines = host.commands.ran.borrow();
    let asked: Vec<&String> = ran_lines
        .iter()
        .filter(|line| line.contains("xcodebuild") || line.contains("xcrun"))
        .collect();
    assert_eq!(asked, Vec::<&String>::new());
}

/// An operator cannot set a `sim.` key: the start is refused naming it. It catches a hello
/// that would carry an operator's simulator list as if the Mac had found it.
#[test]
fn an_operator_cannot_set_a_simulator_capability() {
    let (ran, sent, _) = run_scripted(
        &["--capability", "sim.ios=18.2"],
        &mac_host(),
        vec![welcome(1)],
        1,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.borrow().is_empty());
    assert!(
        ran.err
            .contains("--capability: sim.ios is detected and cannot be set"),
        "{}",
        ran.err
    );
}

/// The offer's Xcodes, which the native runtime runs jobs with, are the ready ones by the
/// real path of their apps: a link `Xcode.app` to an app elsewhere is held as that app. It
/// catches a runtime that holds an Xcode that failed the survey, or one by a link that can
/// be pointed elsewhere after the survey.
#[test]
fn the_offer_holds_the_ready_xcodes_by_the_real_path_of_their_apps() {
    let scratch = Scratch::new("xcode-survey-link");
    let (dir, files) = applications(&scratch, &[("Xcode-16.4.app", "16F6")]);
    let real = fs::canonicalize(scratch.path(""))
        .unwrap()
        .join("Xcode-16.2.app");
    app_dir(&real);
    std::os::unix::fs::symlink(&real, dir.join("Xcode.app")).unwrap();
    let plist = dir.join("Xcode.app/Contents/version.plist");
    let files = files
        .with_dir(&dir.to_string_lossy(), &["Xcode-16.4.app", "Xcode.app"])
        .with(&plist.to_string_lossy(), &version_plist("16C5032a"));
    let real_dev = format!("{}/Contents/Developer", real.display());
    let runner = Answering::new(|line, dev| match dev == real_dev {
        true => ready(line, dev, "16C5032a", "{\"runtimes\":[]}"),
        false => Err(format!("{line}: exit status: 1")),
    });
    let surveyed = survey(&files, &runner, site(&dir));
    assert_eq!(
        surveyed.ready,
        [Xcode {
            build: "16C5032a".to_owned(),
            app: real.clone(),
        }]
    );
    assert_eq!(surveyed.sims, Default::default());
    assert_eq!(
        surveyed.warnings,
        [format!(
            "{}/Xcode-16.4.app: Xcode 16F6 is not offered: it did not answer the survey: \
             {}/usr/bin/xcodebuild -version: exit status: 1",
            dir.display(),
            developer(&dir, "Xcode-16.4.app")
        )]
    );
    // The offer the agent says hello with holds the same Xcodes, which the native runtime
    // runs jobs with.
    let mut args = vec!["js-agent"];
    args.extend(crate::support::flags(&[]));
    let cli = <js_agent::Cli as clap::Parser>::try_parse_from(args).unwrap();
    let mut host = mac_host();
    host.files = files;
    host.applications = dir.clone();
    host.xcode_owner = owner();
    let offer = js_agent::offer(
        &cli,
        &host.platform,
        &runner,
        &host.files,
        host.site(),
        "0",
        None,
    )
    .expect("an offer");
    assert_eq!(offer.xcodes, surveyed.ready);
    assert_eq!(offer.hello.capabilities["xcode"], "16C5032a");
}

/// Each question of the survey that is not a yes leaves the Xcode out, and the warning says
/// which: a program that exits with another status than 0 at the licence or the first
/// launch question is that state, and one that cannot be started or passes its deadline
/// is no answer, as is any failure at the build or compiler question. It catches an Xcode
/// offered after a failed question, and a timeout reported as a licence not accepted.
#[test]
fn each_question_that_is_not_a_yes_leaves_the_xcode_out_with_why() {
    let scratch = Scratch::new("xcode-survey-questions");
    let (dir, files) = applications(&scratch, &[("Xcode.app", "16C5032a")]);
    let dev = developer(&dir, "Xcode.app");
    let xcodebuild = format!("{dev}/usr/bin/xcodebuild");
    let cases: [(&str, Result<&str, String>, String); 7] = [
        (
            "-version",
            Err(format!("{xcodebuild}: exit status: 1")),
            format!("it did not answer the survey: {xcodebuild}: exit status: 1"),
        ),
        (
            "-version",
            Ok("Xcode 16.4\nBuild version 16F6\n"),
            "it did not answer the survey: flagged as build 16C5032a, but xcodebuild says 16F6"
                .to_owned(),
        ),
        (
            "-license check",
            Err(format!("{xcodebuild}: exit status: 69")),
            format!("its licence is not accepted: {xcodebuild}: exit status: 69"),
        ),
        (
            "-license check",
            Err(format!("{xcodebuild}: timed out after 60s")),
            format!("it did not answer the survey: {xcodebuild}: timed out after 60s"),
        ),
        (
            "-checkFirstLaunchStatus",
            Err(format!("{xcodebuild}: exit status: 1")),
            format!("its first launch has not run: {xcodebuild}: exit status: 1"),
        ),
        (
            "-checkFirstLaunchStatus",
            Err(format!(
                "{xcodebuild}: No such file or directory (os error 2)"
            )),
            format!(
                "it did not answer the survey: {xcodebuild}: No such file or directory (os \
                 error 2)"
            ),
        ),
        (
            "--find clang",
            Ok("/usr/bin/clang\n"),
            "it did not answer the survey: xcrun --find clang printed \"/usr/bin/clang\", \
             outside the Xcode"
                .to_owned(),
        ),
    ];
    for (question, answer, why) in cases {
        let runner = Answering::new(|line, dev| {
            let asked = line
                .strip_prefix(&format!("{dev}/usr/bin/xcodebuild "))
                .or_else(|| line.strip_prefix("/usr/bin/xcrun "));
            match asked == Some(question) {
                true => answer.clone().map(str::to_owned),
                false => ready(line, dev, "16C5032a", "{\"runtimes\":[]}"),
            }
        });
        let surveyed = survey(&files, &runner, site(&dir));
        assert_eq!(surveyed.ready, Vec::<Xcode>::new(), "{question}");
        assert_eq!(surveyed.sims, Default::default(), "{question}");
        assert_eq!(
            surveyed.warnings,
            [format!(
                "{}/Xcode.app: Xcode 16C5032a is not offered: {why}",
                dir.display()
            )],
            "{question}"
        );
        assert!(
            !runner
                .lines()
                .iter()
                .any(|line| line.contains("simctl list")),
            "{question}"
        );
    }
}

/// An app that others may write is not offered, and none of its programs is run: the
/// survey would otherwise run what anyone could have put there. It catches a survey that
/// asks before it checks who may write the app.
#[test]
fn an_app_others_can_write_is_not_offered_and_nothing_of_it_runs() {
    let scratch = Scratch::new("xcode-survey-writable");
    let (dir, files) = applications(&scratch, &[("Xcode.app", "16C5032a")]);
    let app = dir.join("Xcode.app");
    fs::set_permissions(&app, fs::Permissions::from_mode(0o777)).unwrap();
    let runner = Answering::new(|line, dev| ready(line, dev, "16C5032a", "{\"runtimes\":[]}"));
    let surveyed = survey(&files, &runner, site(&dir));
    assert_eq!(surveyed.ready, Vec::<Xcode>::new());
    assert_eq!(
        surveyed.warnings,
        [format!(
            "{0}: Xcode 16C5032a is not offered: it did not answer the survey: {0} is not owned \
             by uid {1} or root, or others can write it",
            app.display(),
            owner()
        )]
    );
    assert_eq!(runner.lines(), Vec::<String>::new());
}

/// The simulator capabilities are the union over the ready Xcodes that have `simctl`, each
/// listed under its own `DEVELOPER_DIR`: only available runtimes of iOS, tvOS, watchOS and
/// visionOS (named `xrOS`) with a `major.minor[.patch]` version count, a version listed by
/// both is one, and the versions are ascending by number (`9.1` before `10.0`). An Xcode
/// whose `xcrun` finds no `simctl` is not asked. It catches a capability made from one
/// Xcode only, an unavailable or unknown runtime reported, versions sorted as text, and a
/// listing asked of an Xcode with no `simctl`.
#[test]
fn the_simulators_are_the_available_runtimes_of_every_ready_xcode_with_simctl() {
    let scratch = Scratch::new("xcode-survey-sims");
    let apps = [
        ("Xcode-15.4.app", "15F31d"),
        ("Xcode-16.2.app", "16C5032a"),
        ("Xcode-16.4.app", "16F6"),
    ];
    let (dir, files) = applications(&scratch, &apps);
    let first = listing(&[
        ("iOS-17-5", "17.5", true),
        ("watchOS-10-0", "10.0", true),
        ("tvOS-9-1", "9.1", true),
        ("xrOS-1-0", "1.0", true),
    ]);
    let second = listing(&[
        ("iOS-17-5", "17.5", true),
        ("iOS-18-2", "18.2", true),
        ("tvOS-10-0", "10.0", true),
        ("iOS-19-0", "19.0", false),
        ("macOS-15-0", "15.0", true),
        ("iOS-20", "20", true),
    ]);
    let mut second = serde_json::from_str::<Value>(&second).unwrap();
    second["runtimes"].as_array_mut().unwrap().extend([
        json!({"version": "21.0", "isAvailable": true}),
        json!({"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-22-0", "isAvailable": true}),
        json!({"identifier": "org.example.iOS-23-0", "version": "23.0", "isAvailable": true}),
        json!("not a runtime"),
    ]);
    let second = second.to_string();
    let (d15, d16, d164) = (
        developer(&dir, "Xcode-15.4.app"),
        developer(&dir, "Xcode-16.2.app"),
        developer(&dir, "Xcode-16.4.app"),
    );
    let runner = Answering::new(|line, dev| {
        let build = [(&d15, "15F31d"), (&d16, "16C5032a"), (&d164, "16F6")]
            .into_iter()
            .find_map(|(d, b)| (d.as_str() == dev).then_some(b))
            .unwrap();
        match (build, line) {
            ("15F31d", _) => ready(line, dev, build, &first),
            ("16F6", "/usr/bin/xcrun --find simctl") => Err("xcrun: exit status: 1".to_owned()),
            ("16F6", "/usr/bin/xcrun simctl list runtimes -j") => panic!("16F6 has no simctl"),
            _ => ready(line, dev, build, &second),
        }
    });
    let surveyed = survey(&files, &runner, site(&dir));
    assert_eq!(surveyed.warnings, Vec::<String>::new());
    let builds: Vec<&str> = surveyed.ready.iter().map(|x| x.build.as_str()).collect();
    assert_eq!(builds, ["15F31d", "16C5032a", "16F6"]);
    let sims: Vec<(&str, &str)> = surveyed
        .sims
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        sims,
        [
            ("sim.ios", "17.5,18.2"),
            ("sim.tvos", "9.1,10.0"),
            ("sim.visionos", "1.0"),
            ("sim.watchos", "10.0"),
        ]
    );
    let listed: Vec<Vec<(String, String)>> = runner
        .ran
        .borrow()
        .iter()
        .filter(|(line, _)| line.ends_with("simctl list runtimes -j"))
        .map(|(_, env)| env.clone())
        .collect();
    assert_eq!(
        listed,
        [
            vec![("DEVELOPER_DIR".to_owned(), d15.clone())],
            vec![("DEVELOPER_DIR".to_owned(), d16.clone())],
        ]
    );
}

/// A listing that fails, is not JSON or has no `runtimes` array leaves that Xcode's
/// simulators out with a warning naming its build, and the Xcode is still offered. It
/// catches a failed listing that hides a ready Xcode or stops the agent, and one that is
/// silently taken as no simulator.
#[test]
fn a_listing_that_cannot_be_read_leaves_out_only_the_simulators() {
    let scratch = Scratch::new("xcode-survey-listing");
    let (dir, files) = applications(&scratch, &[("Xcode.app", "16C5032a")]);
    let cases = [
        (
            Err("/usr/bin/xcrun: timed out after 60s".to_owned()),
            "/usr/bin/xcrun: timed out after 60s",
        ),
        (
            Ok("CoreSimulator is not running".to_owned()),
            "not JSON: expected value at line 1 column 1",
        ),
        (Ok("{\"devices\":{}}".to_owned()), "no runtimes array"),
    ];
    for (answer, why) in cases {
        let runner = Answering::new(|line, dev| match line {
            "/usr/bin/xcrun simctl list runtimes -j" => answer.clone(),
            _ => ready(line, dev, "16C5032a", ""),
        });
        let surveyed = survey(&files, &runner, site(&dir));
        assert_eq!(surveyed.ready.len(), 1, "{why}");
        assert_eq!(surveyed.sims, Default::default(), "{why}");
        assert_eq!(
            surveyed.warnings,
            [format!(
                "Xcode 16C5032a: its simulator runtimes cannot be listed ({why}); none of its \
                 own is reported"
            )]
        );
    }
}

/// An answer that printed more than the survey reads is no answer, at a question and at
/// the listing alike. It catches a cut answer read as a whole one.
#[test]
fn an_answer_cut_short_is_no_answer() {
    struct Cut;
    impl CommandRunner for Cut {
        fn run(&self, program: &str, _args: &[&str]) -> Result<String, String> {
            Err(format!("{program}: not found"))
        }
        fn run_in(
            &self,
            program: &str,
            args: &[&str],
            _env: &[(&str, &str)],
        ) -> Result<String, String> {
            self.run(program, args)
        }
        fn run_streams(
            &self,
            _program: &str,
            _args: &[&str],
            _env: &[(&str, &str)],
            _keep: usize,
            _limit: Duration,
        ) -> Result<Streams, String> {
            Ok(Streams {
                stdout: b"Xcode 16.2\nBuild version 16C5032a\n".to_vec(),
                stderr: Vec::new(),
                dropped: true,
            })
        }
    }
    let scratch = Scratch::new("xcode-survey-cut");
    let (dir, files) = applications(&scratch, &[("Xcode.app", "16C5032a")]);
    let surveyed = survey(&files, &Cut, site(&dir));
    assert_eq!(surveyed.ready, Vec::<Xcode>::new());
    assert_eq!(
        surveyed.warnings,
        [format!(
            "{}/Xcode.app: Xcode 16C5032a is not offered: it did not answer the survey: \
             {}/usr/bin/xcodebuild: printed more than 8388608 bytes",
            dir.display(),
            developer(&dir, "Xcode.app")
        )]
    );
}

/// The simulator listing cut short is left out with a warning, the Xcode still offered.
#[test]
fn a_listing_cut_short_is_left_out() {
    struct CutList<'a>(&'a str);
    impl CommandRunner for CutList<'_> {
        fn run(&self, program: &str, _args: &[&str]) -> Result<String, String> {
            Err(format!("{program}: not found"))
        }
        fn run_in(
            &self,
            program: &str,
            args: &[&str],
            _env: &[(&str, &str)],
        ) -> Result<String, String> {
            self.run(program, args)
        }
        fn run_streams(
            &self,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
            _keep: usize,
            _limit: Duration,
        ) -> Result<Streams, String> {
            let line = format!("{program} {}", args.join(" "));
            let dropped = line.ends_with("simctl list runtimes -j");
            let dev = env[0].1;
            ready(&line, dev, self.0, "{\"runtimes\":[]}").map(|out| Streams {
                stdout: out.into_bytes(),
                stderr: Vec::new(),
                dropped,
            })
        }
    }
    let scratch = Scratch::new("xcode-survey-cut-list");
    let (dir, files) = applications(&scratch, &[("Xcode.app", "16C5032a")]);
    let surveyed = survey(&files, &CutList("16C5032a"), site(&dir));
    assert_eq!(surveyed.ready.len(), 1);
    assert_eq!(
        surveyed.warnings,
        [
            "Xcode 16C5032a: its simulator runtimes cannot be listed (/usr/bin/xcrun: printed more \
          than 8388608 bytes); none of its own is reported"
        ]
    );
}

/// More simulator versions of one platform than fit the 128-byte value stop before the
/// limit, from the lowest, with a warning that says how many were left out. It catches a
/// hello the server refuses for a value over the limit.
#[test]
fn many_simulator_versions_stop_at_the_value_limit_and_warn() {
    let scratch = Scratch::new("xcode-survey-many-sims");
    let (dir, files) = applications(&scratch, &[("Xcode.app", "16C5032a")]);
    let runtimes: Vec<(String, String)> = (0..40)
        .map(|n| (format!("iOS-100-{n}"), format!("100.{n}")))
        .collect();
    let refs: Vec<(&str, &str, bool)> = runtimes
        .iter()
        .map(|(id, version)| (id.as_str(), version.as_str(), true))
        .collect();
    let text = listing(&refs);
    let runner = Answering::new(|line, dev| ready(line, dev, "16C5032a", &text));
    let surveyed = survey(&files, &runner, site(&dir));
    let value = &surveyed.sims["sim.ios"];
    let kept: Vec<String> = (0..19).map(|n| format!("100.{n}")).collect();
    assert_eq!(value, &kept.join(","));
    assert!(value.len() <= js_agent_wire::limits::MAX_CAPABILITY_VALUE_LEN);
    assert_eq!(
        surveyed.warnings,
        ["sim.ios: 21 of 40 versions do not fit in 128 bytes and are left out of the hello"]
    );
}

/// A Mac with more Xcodes than the catalog takes (64) offers none and says why, and runs
/// no program of them. It catches a survey that asks without bound, and a refusal that is
/// not told.
#[test]
fn more_xcodes_than_the_catalog_takes_are_refused_with_a_warning() {
    let scratch = Scratch::new("xcode-survey-many");
    let names: Vec<(String, String)> = (0..65)
        .map(|n| (format!("Xcode-{n:02}.app"), format!("16A{n:03}")))
        .collect();
    let apps: Vec<(&str, &str)> = names
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (dir, files) = applications(&scratch, &apps);
    let runner = Answering::new(|line, _| panic!("{line} was run"));
    let surveyed = survey(&files, &runner, site(&dir));
    assert_eq!(surveyed.ready, Vec::<Xcode>::new());
    assert_eq!(
        surveyed.warnings,
        ["the Xcodes cannot be surveyed (too many --xcode flags); none is offered"]
    );
}

/// A build that the plist allows but the catalog's flag does not (`-`) is left out by name,
/// and the other Xcodes are surveyed. It catches one odd build that hides every Xcode.
#[test]
fn an_unflaggable_build_is_left_out_by_name() {
    let scratch = Scratch::new("xcode-survey-odd-build");
    let apps = [("Xcode-odd.app", "16-A00"), ("Xcode.app", "16C5032a")];
    let (dir, files) = applications(&scratch, &apps);
    let runner = Answering::new(|line, dev| ready(line, dev, "16C5032a", "{\"runtimes\":[]}"));
    let surveyed = survey(&files, &runner, site(&dir));
    let builds: Vec<&str> = surveyed.ready.iter().map(|x| x.build.as_str()).collect();
    assert_eq!(builds, ["16C5032a"]);
    assert_eq!(
        surveyed.warnings,
        [format!(
            "{}/Xcode-odd.app: the build is not 1 to 32 ASCII letters and digits; Xcode 16-A00 \
             is not offered",
            dir.display()
        )]
    );
}

/// The machine's own site is `/Applications`, whose apps must be root's. It catches an
/// agent that would trust an app another user owns.
#[test]
fn the_machine_looks_in_applications_for_root_owned_apps() {
    let site = Site::system();
    assert_eq!(site.applications, Path::new("/Applications"));
    assert_eq!(site.owner, 0);
}

/// A survey with nothing found is empty: no Xcode, no simulator, no warning.
#[test]
fn an_empty_applications_directory_surveys_nothing() {
    let scratch = Scratch::new("xcode-survey-empty");
    let (dir, files) = applications(&scratch, &[]);
    let runner = Answering::new(|line, _| panic!("{line} was run"));
    assert_eq!(survey(&files, &runner, site(&dir)), Surveyed::default());
}
