//! The two-run isolation test of the Mac pool users, for real, as root, on a Mac.
//!
//! Question: does what a run leaves behind survive into the next run that gets the same
//! uid? Today every run gets a user the helper makes and deletes; the helper reuses uids,
//! and a fixed pool of reused users would too. Deleting a user may not remove what it left.
//!
//! The test serves the real `js-mac-session` helper from this process with a range of ONE
//! uid, so the second run gets the uid of the first. Then:
//!
//! 1. Run A (a native run through the agent's own `NativeRuntime`, under the product's
//!    Seatbelt profile, as the pool user) leaves one item of every kind behind, each with a
//!    random token: a file in its home and in its scratch, a file in the system temp
//!    directory, a process left running (detached), a keychain item, a defaults key, a
//!    launchd agent in its user domain, System V shared memory, semaphore and message
//!    queue, and POSIX shared memory and a POSIX semaphore; and the things that outlive a home:
//!    a file in `DARWIN_USER_TEMP_DIR` and in `DARWIN_USER_CACHE_DIR` (the uid-keyed
//!    directories under `/var/folders`, whose exact paths run A reports), a file in
//!    `/Users/Shared`, and a launchd agent in the `gui/<uid>` domain. What the profile or the
//!    machine refuses is printed as refused, with the reason. A kind is covered when it was
//!    seeded or the profile denied the seed with the sandbox's own signature (`deny(1)`,
//!    `Sandbox:`, or a bare permission error the unified log confirms as that seed's, see
//!    `look.rs`): the job could not create the state, so nothing can carry over. Any other
//!    seed failure, a bare EPERM/EACCES included, proves nothing. A run that did not cover at
//!    least a file, a process, a keychain item, a launchd agent and shared memory, or seeded
//!    nothing itself, fails. A launch agent that root loaded is labelled "loaded by root" and
//!    is no sandbox claim.
//! 2. Between the runs the test does what the agent does when a run ends and the next
//!    begins: `NativeRuntime::remove` (the helper's `kill-uid`, then `user-delete`), and
//!    the next start makes the user again (`user-create`).
//! 3. Run B (the same program, another role, the same uid) looks for every token. The test
//!    also reads the system-wide views: `ipcs` for objects owned by the uid, `ps` for
//!    processes of the uid, `launchctl print` of `user/<uid>` and `gui/<uid>`, and `find -uid`
//!    (every path, no depth limit) over `/private/var/folders`, `/private/tmp`,
//!    `/Users/Shared` and `/Library/Caches`: a path with a token in it fails, so does a token
//!    in the content of a file (the first 2000 files, 1 MiB each; more is a could-not-look);
//!    other files of the uid are printed as notes. A run that fails midway is still cleaned up:
//!    its handle and what it printed are recorded as soon as it exists.
//!    Each run prints its own `getuid()`; A, B and the configured uid must be equal.
//! 4. Every look has three answers: found, absent (only by the documented not-found answer
//!    of the call or tool, see `look.rs`) or could-not-look, which fails the test with its
//!    reason. Anything found fails the test with a table of what survived. Nothing here asserts
//!    anything about time.
//!
//! It lives in js-agent because the question is about what the agent's runtime leaves: the
//! test drives the runtime's own start and remove (the calls the agent makes), and js-agent
//! already holds the fixture that serves the helper from a test process.
//!
//! Every test that touches the machine is `#[ignore]`: it needs root, a Mac and a free uid,
//! and it makes a user. Run the binary as root with `JS_NATIVE_ISO_ROOT` naming a new
//! directory, owned by root and mode 0755, that every user can search down to, and
//! `JS_NATIVE_ISO_UID` naming a uid in 500 to 60000 that nothing uses, with `--ignored
//! --test-threads=1 --nocapture`. See the `js-agent-native-isolation-test` workflow. Never
//! run it on a production host.
//!
//! One difference from production: the runtime here has the directory of this test binary as
//! an extra tool root (the runs execute the binary), a read/exec allowance the agent
//! does not have. Not tested here: cron/at jobs and extended attributes.
//!
//! The file builds on every platform, so clippy reads it on Linux; the parsers and the
//! report are tested there. Run anywhere but macOS, the ignored test stops at its first
//! line.

mod child;
mod lines;
mod look;
mod report;
mod token;
mod views;

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};
use js_mac_session::client::Client;

use lines::{Kind, Line, Verdict, parse_lines};
use look::Outcome;
use token::Token;

/// The variable naming the directory the test works in.
const ROOT: &str = "JS_NATIVE_ISO_ROOT";
/// The variable naming the one uid of the helper's range.
const UID: &str = "JS_NATIVE_ISO_UID";
const NEEDS_ROOT: &str =
    "needs root: run the test binary as root (see the header of tests/native_isolation/main.rs)";
/// The most polls the test makes for a run to end.
const POLLS: u32 = 50_000_000;
/// The environment names of the child's role and its input; neither starts with a name the
/// agent reserves.
const ROLE: &str = "ISO_ROLE";
const HANDOFF: &str = "ISO_HANDOFF";
/// The libtest test the runs execute.
const CHILD: &str = "child_of_the_runs";

static TERMS: AtomicU64 = AtomicU64::new(1);

/// What a run does when the test starts this binary as it: nothing, unless the role says.
/// Not `#[ignore]`: it is a no-op without the role, and the runs name it exactly.
#[test]
fn child_of_the_runs() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    println!("{}", look::uid_line(rustix::process::getuid().as_raw()));
    match role.as_str() {
        "seed" => {
            let tmp = PathBuf::from(std::env::var("TMPDIR").expect("the agent sets TMPDIR"));
            let spot = child::Spot {
                home: tmp.parent().expect("TMPDIR is below the home").to_owned(),
                tmp,
                uid: rustix::process::getuid().as_raw(),
            };
            for line in child::seed_all(&spot) {
                println!("{}", line.encode());
            }
        }
        "look" => {
            let path = std::env::var(HANDOFF).expect("the test names the seeded list");
            let text = fs::read_to_string(&path).expect("the seeded list is readable");
            let seeded = parse_lines(&text).expect("the seeded list is ours");
            let tmp = PathBuf::from(std::env::var("TMPDIR").expect("the agent sets TMPDIR"));
            let home = tmp.parent().expect("TMPDIR is below the home");
            for line in child::look_all(&seeded, home) {
                println!("{}", line.encode());
            }
        }
        other => panic!("unknown role {other:?}"),
    }
}

/// The helper this process serves, and where it keeps things.
struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    homes: PathBuf,
    state: PathBuf,
    uid: u32,
}

fn fixture() -> Fixture {
    assert!(rustix::process::geteuid().is_root(), "{NEEDS_ROOT}");
    let root = std::env::var_os(ROOT).unwrap_or_else(|| panic!("{ROOT} is not set"));
    let root = fs::canonicalize(root).unwrap();
    let uid: u32 = std::env::var(UID)
        .unwrap_or_else(|_| panic!("{UID} is not set"))
        .parse()
        .unwrap_or_else(|_| panic!("{UID} is not a uid"));
    let fixture = Fixture {
        socket: root.join("sock").join("socket"),
        homes: root.join("homes"),
        state: root.join("state"),
        root,
        uid,
    };
    fs::create_dir(&fixture.homes).unwrap();
    fs::create_dir(fixture.root.join("scratch")).unwrap();
    serve(&fixture);
    fixture
}

/// Serves the real helper on the fixture's socket, with a range of the one uid, for as
/// long as the process lives.
#[cfg(target_os = "macos")]
fn serve(fixture: &Fixture) {
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    use clap::Parser as _;
    use js_mac_session::macos::MacHost;
    use js_mac_session::scan::ScanFolder;
    use js_mac_session::server::{self, Caller, CallerCheck};
    use js_mac_session::start::{self, Args};
    use js_mac_session::sweep::SweepPlan;

    /// The caller check of a helper this very process serves: everyone who reaches the
    /// socket is the test.
    struct Anyone;

    impl CallerCheck for Anyone {
        fn identify(&self, _: &UnixStream) -> Result<Caller, String> {
            Ok(Caller {
                token: Vec::new(),
                pid: 0,
                version: 0,
            })
        }

        fn check(&self, _: &Caller) -> Result<(), String> {
            Ok(())
        }
    }

    let path = |path: &Path| path.display().to_string();
    let args = Args::parse_from([
        "js-mac-session".to_owned(),
        "--socket".to_owned(),
        path(&fixture.socket),
        "--socket-group".to_owned(),
        "wheel".to_owned(),
        "--state-dir".to_owned(),
        path(&fixture.state),
        "--uid-range".to_owned(),
        format!("{0}-{0}", fixture.uid),
        "--homes".to_owned(),
        path(&fixture.homes),
        // The boot reset clears the auto-login of the Mac the helper serves; the files it
        // clears are the test's own, so a real login setting is not touched.
        "--kcpassword".to_owned(),
        path(&fixture.root.join("kcpassword")),
        "--loginwindow".to_owned(),
        path(&fixture.root.join("loginwindow")),
        "--daemon-requirement".to_owned(),
        "unused: the check is replaced".to_owned(),
    ]);
    let ready = start::prepare(
        &args,
        Box::new(MacHost),
        "js-native-isolation".to_owned(),
        SweepPlan::macos_schedules(),
        SweepPlan::macos(),
        ScanFolder::macos(),
    )
    .unwrap();
    let check: Arc<dyn CallerCheck> = Arc::new(Anyone);
    std::thread::spawn(move || server::serve(&ready.listener, &ready.helper, &check));
}

#[cfg(not(target_os = "macos"))]
fn serve(_: &Fixture) {
    panic!("these tests run on macOS only");
}

/// The production layout of the runtime, plus the directory this binary is in as a tool
/// root (the runs execute it).
fn runtime(fixture: &Fixture, exe: &Path) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let config = NativeConfig {
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        net: Net::Off,
        tool_roots: vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
            exe.parent().unwrap().to_owned(),
        ],
        read_denies: vec![
            denied(&fixture.state),
            denied(&fixture.root.join("scratch")),
        ],
        read_model: ReadModel::Open,
        journal: fixture.root.join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
    };
    let term = TERMS.fetch_add(1, Ordering::SeqCst);
    NativeRuntime::open(Client::new(&fixture.socket), config, term).unwrap()
}

/// A run of this binary's `child_of_the_runs` in `role`; its scratch directory (the log
/// files) is made here.
fn spec(fixture: &Fixture, run_id: &str, exe: &Path, env: &[(&str, &str)]) -> RunSpec {
    let scratch = fixture.root.join("scratch").join(run_id);
    fs::create_dir(&scratch).unwrap();
    RunSpec {
        run_id: run_id.to_owned(),
        body: Body::Native(NativeSpec {
            program: exe.display().to_string(),
            args: ["--exact", CHILD, "--nocapture", "--test-threads=1"]
                .map(str::to_owned)
                .to_vec(),
            env: env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::Off,
        }),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch,
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        labels: Default::default(),
        bundle: None,
    }
}

/// Polls until the run is no longer running.
fn settle(rt: &mut NativeRuntime, id: &RunHandle) -> Result<Status, String> {
    for _ in 0..POLLS {
        let status = rt.poll(id).map_err(|why| format!("poll: {}", why.0))?;
        if status != Status::Running {
            return Ok(status);
        }
        std::thread::yield_now();
    }
    Err("the run did not end".to_owned())
}

/// The home of the user of `id`: the lease `T.S` is the user `kbf-lease-T-S`.
fn user_of(id: &RunHandle) -> String {
    format!("kbf-lease-{}", id.0.replace('.', "-"))
}

fn tool_output(program: &str, args: &[&str]) -> Result<(bool, String), String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|why| format!("{program}: {why}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

/// What one run of the child printed, and the handle of the run.
struct Ran {
    id: RunHandle,
    lines: Vec<Line>,
    uid: Option<u32>,
}

/// Starts one run of the child in `role`, waits for it and returns its handle and what it
/// printed. The child must exit 0. The handle goes into `runs` the moment the run exists, and
/// whatever the child printed goes into `sink` whether or not the run ended well, so that a run
/// that fails midway still leaves its user and its seeded objects to the cleanup.
fn child_run(
    rt: &mut NativeRuntime,
    fixture: &Fixture,
    exe: &Path,
    name: &str,
    env: &[(&str, &str)],
    runs: &mut Vec<RunHandle>,
    sink: &mut Vec<Line>,
) -> Result<Ran, String> {
    let id = rt
        .create_and_start(&spec(fixture, name, exe, env))
        .map_err(|why| format!("start run {name}: {}", why.0))?;
    look::record_then(runs, id, |id| {
        let status = settle(rt, id);
        let out = fixture.root.join("scratch").join(name);
        let stdout = fs::read_to_string(out.join("stdout")).unwrap_or_default();
        let stderr = fs::read_to_string(out.join("stderr")).unwrap_or_default();
        sink.extend(lines::parse_lines_lenient(&stdout));
        let status = status?;
        let clean = matches!(status, Status::Exited(Exit { code: 0, .. }));
        if !clean {
            return Err(format!(
                "run {name} ended {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            ));
        }
        Ok(Ran {
            id: id.clone(),
            lines: parse_lines(&stdout)?,
            uid: look::parse_uid(&stdout),
        })
    })
}

/// Everything a trial needs to clean up after itself. Dropping it cleans up, so a panic
/// anywhere in the body leaves no user, launchd domain or IPC object behind.
struct Trial {
    rt: NativeRuntime,
    fixture: Fixture,
    exe: PathBuf,
    runs: Vec<RunHandle>,
    seeded: Vec<Line>,
}

impl Drop for Trial {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// One row of the table of survivors.
type Row = [String; 3];

fn row(kind: Kind, place: impl Into<String>, detail: impl Into<String>) -> Row {
    [kind.label().to_owned(), place.into(), detail.into()]
}

impl Trial {
    /// The two runs and the views; the table of survivors, empty when the pool user is clean.
    fn body(&mut self) -> Result<Vec<Row>, String> {
        let uid = self.fixture.uid;
        let exe = self.exe.clone();

        // Run A.
        let started = Instant::now();
        let ran_a = child_run(
            &mut self.rt,
            &self.fixture,
            &exe,
            "a",
            &[(ROLE, "seed")],
            &mut self.runs,
            &mut self.seeded,
        )?;
        let a = ran_a.id.clone();
        let owner = fs::metadata(self.fixture.homes.join(user_of(&a)))
            .map_err(|why| format!("the home of run A: {why}"))?
            .uid();
        if owner != uid {
            return Err(format!(
                "run A ran as uid {owner}, not the uid {uid} asked for"
            ));
        }
        self.confirm_denials_by_log(started);
        self.bootstrap_agents_as_root(uid);
        println!("{}", self.seeded_table());
        let missing = look::missing_minimum(&self.seeded);
        if !missing.is_empty() {
            return Err(format!(
                "run A neither seeded nor had the profile deny {}: the result would prove nothing",
                missing.join(", ")
            ));
        }
        self.control(uid)?;

        // Between the runs: what the agent does when a run ends.
        self.rt
            .remove(&a)
            .map_err(|why| format!("remove run A: {}", why.0))?;
        self.runs.clear();

        // Run B, on the same uid.
        let handoff = self.fixture.root.join("seeded.txt");
        let text: Vec<String> = self.seeded.iter().map(Line::encode).collect();
        fs::write(&handoff, text.join("\n")).map_err(|why| format!("the seeded list: {why}"))?;
        let handoff_text = handoff.display().to_string();
        let mut not_cleaned = Vec::new();
        let ran_b = child_run(
            &mut self.rt,
            &self.fixture,
            &exe,
            "b",
            &[(ROLE, "look"), (HANDOFF, &handoff_text)],
            &mut self.runs,
            &mut not_cleaned,
        )?;
        let b = ran_b.id.clone();
        let owner = fs::metadata(self.fixture.homes.join(user_of(&b)))
            .map_err(|why| format!("the home of run B: {why}"))?
            .uid();
        if owner != uid {
            return Err(format!(
                "run B got uid {owner}, not run A's uid {uid}: the helper did not reuse it, so this run proves nothing"
            ));
        }
        look::uids_agree(ran_a.uid, ran_b.uid, uid)?;

        let mut survivors: Vec<Row> = ran_b
            .lines
            .iter()
            .filter_map(|line| match line.verdict {
                Verdict::Found => Some(row(
                    line.kind,
                    line.text.clone(),
                    format!("run B found it (token {})", line.token),
                )),
                Verdict::Unlooked => Some(row(
                    line.kind,
                    "COULD NOT LOOK",
                    format!("{} (token {})", line.text, line.token),
                )),
                _ => None,
            })
            .collect();
        survivors.extend(self.system_views(uid, &user_of(&b))?);
        Ok(survivors)
    }

    fn tokens(&self) -> Vec<Token> {
        self.seeded.iter().map(|line| line.token.clone()).collect()
    }

    fn seeded_table(&self) -> String {
        let rows: Vec<Row> = self
            .seeded
            .iter()
            .map(|line| {
                [
                    line.kind.label().to_owned(),
                    match line.verdict {
                        Verdict::Seeded if line.text.contains(look::LOADED_BY_ROOT) => {
                            "loaded by root (not by the job; not a sandbox claim)".to_owned()
                        }
                        Verdict::Seeded => "seeded".to_owned(),
                        Verdict::Denied => "refused by the profile".to_owned(),
                        _ => "refused (proves nothing)".to_owned(),
                    },
                    line.text.clone(),
                ]
            })
            .collect();
        for row in &rows {
            println!("PROBE {} {} {}", row[0], row[1], row[2]);
        }
        report::table(["KIND", "RUN A", "WHERE OR WHY"], &rows)
    }

    /// A seed that failed with a bare permission error is the profile's only if the unified
    /// log, read for run A's window, holds a sandbox denial that is that seed's; the log is
    /// not read when nothing failed that way. A log that cannot be read leaves them refused.
    fn confirm_denials_by_log(&mut self, since: Instant) {
        let candidates = self.seeded.iter().any(|line| {
            line.verdict == Verdict::Refused && look::is_permission_failure(&line.text)
        });
        if !candidates {
            return;
        }
        let job = look::JobProcs::default();
        let secs = since.elapsed().as_secs() + 60;
        let predicate = "eventMessage CONTAINS \"Sandbox\" AND eventMessage CONTAINS \"deny(\"";
        let shown = view(
            "/usr/bin/log",
            &[
                "show",
                "--last",
                &format!("{secs}s"),
                "--style",
                "compact",
                "--predicate",
                predicate,
            ],
        );
        let log = match shown {
            Ok(text) => Some(text),
            Err(why) => {
                println!("PROBE note the unified log could not be read: {why}");
                None
            }
        };
        for line in &mut self.seeded {
            if line.verdict != Verdict::Refused {
                continue;
            }
            let verdict =
                look::resolve_seed(line.kind, &line.token, &line.text, log.as_deref(), &job);
            if verdict == Verdict::Denied {
                line.verdict = Verdict::Denied;
                line.text = format!("confirmed in the unified log: {}", line.text);
            }
        }
    }

    /// A launch agent run A could not load (a user cannot load into its own domain on
    /// every macOS) is loaded by root into the user's domain: the item is still the run's
    /// to leave, and the domain is the one the helper's delete does not end.
    fn bootstrap_agents_as_root(&mut self, uid: u32) {
        for (kind, domain) in [(Kind::LaunchAgent, "user"), (Kind::GuiLaunchAgent, "gui")] {
            let Some(line) = self.seeded.iter_mut().find(|line| {
                line.kind == kind && matches!(line.verdict, Verdict::Refused | Verdict::Denied)
            }) else {
                continue;
            };
            let label = format!("com.komira.iso.{}", line.token);
            let plist = self.fixture.root.join(format!("{label}-{domain}.plist"));
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n"
            );
            if fs::write(&plist, body).is_err() {
                continue;
            }
            let target = format!("{domain}/{uid}");
            let loaded = tool_output(
                "/bin/launchctl",
                &["bootstrap", &target, &plist.display().to_string()],
            );
            if matches!(loaded, Ok((true, _))) {
                *line = Line::new(
                    Verdict::Seeded,
                    kind,
                    line.token.clone(),
                    &format!(
                        "{label} ({} into {target}: run A could not)",
                        look::LOADED_BY_ROOT
                    ),
                );
            }
        }
    }

    /// Before the runs change over: every item that can be seen from root is seen, and
    /// the process is running, so that "run B found nothing" cannot mean "nothing was
    /// ever there". A seeded item that is not seen, or not possible to look at, fails the
    /// test: it proves nothing.
    fn control(&self, uid: u32) -> Result<(), String> {
        let tokens = self.tokens();
        let ps = view("/bin/ps", &["-A", "-o", "uid=,pid=,command="])?;
        let procs = views::parse_ps(&ps);
        let running = views::procs_with_token(&procs, uid, &tokens);
        let mut missing = Vec::new();
        for line in self.seeded.iter().filter(|l| l.verdict == Verdict::Seeded) {
            let seen = match line.kind {
                Kind::Process => running
                    .iter()
                    .any(|p| p.command.contains(line.token.as_str())),
                Kind::HomeFile
                | Kind::ScratchFile
                | Kind::SystemTempFile
                | Kind::UserTempDir
                | Kind::UserCacheDir
                | Kind::SharedFile
                | Kind::SysvShm
                | Kind::SysvSem
                | Kind::SysvMsg
                | Kind::PosixShm
                | Kind::PosixSem => {
                    matches!(child::probe(line, Path::new("/")), Outcome::Found(_))
                }
                // Proved by the job itself, which read the password and the key back before it
                // called them seeded.
                Kind::Keychain | Kind::Defaults => true,
                // Proved by root's `launchctl print` of the domain the agent is in.
                Kind::LaunchAgent | Kind::GuiLaunchAgent => {
                    let domain = match line.kind {
                        Kind::LaunchAgent => format!("user/{uid}"),
                        _ => format!("gui/{uid}"),
                    };
                    match look::domain_holds(&domain, &print_domain(&domain), &line.token) {
                        Ok(()) => true,
                        Err(why) => {
                            println!("PROBE note control: {why}");
                            false
                        }
                    }
                }
            };
            if !seen {
                missing.push(format!("{} {}", line.kind.label(), line.text));
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "items run A said it left were not there before the runs changed over, so the test would prove nothing for them: {}",
            missing.join("; ")
        ))
    }

    /// The system-wide views, read as root after run B ended: SysV objects by owner,
    /// processes by uid, the user's launchd domains, and the files of the uid outside its
    /// home. A view that fails is an error, not an empty view.
    fn system_views(&self, uid: u32, user: &str) -> Result<Vec<Row>, String> {
        let tokens = self.tokens();
        let mut rows = Vec::new();

        let ipcs = view("/usr/bin/ipcs", &["-a"])?;
        let objects = views::parse_ipcs(&ipcs);
        for object in views::ipc_owned_by(&objects, uid, &[user]) {
            let kind = match object.kind {
                views::IpcKind::Message => Kind::SysvMsg,
                views::IpcKind::Shared => Kind::SysvShm,
                views::IpcKind::Semaphore => Kind::SysvSem,
            };
            rows.push(row(
                kind,
                format!("key={} id={}", object.key, object.id),
                format!("ipcs lists it as owned by {}", object.owner),
            ));
        }

        let ps = view("/bin/ps", &["-A", "-o", "uid=,pid=,command="])?;
        let procs = views::parse_ps(&ps);
        for proc in views::procs_with_token(&procs, uid, &tokens) {
            rows.push(row(
                Kind::Process,
                format!("pid={}", proc.pid),
                format!("ps lists it for uid {uid}: {}", proc.command),
            ));
        }
        // Other processes of the uid carry no token and are not a failure (the user's
        // launchd domain runs its own agents); they are printed for the reader.
        for proc in views::procs_of_uid(&procs, uid) {
            println!(
                "PROBE note process of uid {uid} after run B: pid={} {}",
                proc.pid, proc.command
            );
        }

        for (domain, kind) in [("user", Kind::LaunchAgent), ("gui", Kind::GuiLaunchAgent)] {
            let target = format!("{domain}/{uid}");
            match print_domain(&target) {
                Outcome::Found(text) => {
                    println!("PROBE note launchctl print {target}: the domain exists");
                    for line in views::lines_with_token(&text, &tokens) {
                        rows.push(row(
                            kind,
                            target.clone(),
                            format!("launchctl print: {line}"),
                        ));
                    }
                }
                Outcome::Absent => println!("PROBE note launchctl print {target}: no such domain"),
                Outcome::CouldNotLook(why) => {
                    rows.push(row(kind, "COULD NOT LOOK", format!("{target}: {why}")))
                }
            }
        }

        for root in FIND_ROOTS {
            // No depth limit: a limit would hide a file below it. '-x' stays on the volume.
            let found = view(
                "/usr/bin/find",
                &[
                    "-x",
                    root,
                    "-uid",
                    &uid.to_string(),
                    "-type",
                    "f",
                    "-print0",
                ],
            );
            note_other_volumes(root);
            match found {
                Ok(text) => {
                    let (with, notes, cut) = look::split_found_files(&text, &tokens, FIND_LIMIT);
                    for path in with {
                        rows.push(row(
                            Kind::SharedFile,
                            path,
                            format!(
                                "find -uid {uid} under {root}: a file with a token in its path"
                            ),
                        ));
                    }
                    for path in notes {
                        println!("PROBE note file of uid {uid} under {root}: {path}");
                    }
                    if cut {
                        println!(
                            "PROBE note more than {FIND_LIMIT} other files of uid {uid} under {root}; the rest are not listed (all were examined for tokens)"
                        );
                    }
                    rows.extend(scan_content(uid, root, &text, &tokens));
                }
                Err(why) => rows.push(row(Kind::SharedFile, "COULD NOT LOOK", why)),
            }
        }
        Ok(rows)
    }

    /// Takes down what the trial made, whatever happened (it runs from `Drop`, so also on a
    /// panic): the launchd domains, the users, the objects the runs left. Best effort, and
    /// no wider than that.
    fn cleanup(&mut self) {
        // The domains go out before the user is deleted: deleting a user does not end
        // its launchd domain, and one left makes the next run on the uid refuse.
        for domain in ["user", "gui"] {
            let target = format!("{domain}/{}", self.fixture.uid);
            let _ = tool_output("/bin/launchctl", &["bootout", &target]);
        }
        for id in std::mem::take(&mut self.runs) {
            let _ = self.rt.remove(&id);
        }
        for line in std::mem::take(&mut self.seeded) {
            if line.verdict == Verdict::Seeded {
                child::remove(&line);
            }
        }
    }
}

/// The directories the bounded `find -uid` reads, as root, for files the uid owns outside
/// its home.
const FIND_ROOTS: [&str; 4] = [
    "/private/var/folders",
    "/private/tmp",
    "/Users/Shared",
    "/Library/Caches",
];
/// The most notes of one `find` that are printed (every path is examined for a token).
const FIND_LIMIT: usize = 500;
/// The most files of one `find` whose content is read, and the most bytes read of each.
const CONTENT_FILES: usize = 2000;
const CONTENT_BYTES: u64 = 1 << 20;

/// `find -x` does not enter a volume mounted below a root: say which children are on another
/// device, so that a leak there is not a silent pass.
fn note_other_volumes(root: &str) {
    let Ok(root_meta) = fs::metadata(root) else {
        return;
    };
    let children: Vec<(String, u64)> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let meta = fs::symlink_metadata(entry.path()).ok()?;
            Some((entry.path().display().to_string(), meta.dev()))
        })
        .collect();
    for path in look::other_volume_children(root_meta.dev(), &children) {
        println!("PROBE note {path} is on another volume than {root}: find -x did not look in it");
    }
}

/// The files `find` listed, read for the tokens (a file whose name lacks the token, a cfprefs
/// plist or a keychain database, can still hold it). At most `CONTENT_FILES` files and
/// `CONTENT_BYTES` of each: what is beyond is a COULD NOT LOOK row, never a pass.
fn scan_content(uid: u32, root: &str, listing: &str, tokens: &[Token]) -> Vec<Row> {
    let mut rows = Vec::new();
    let paths: Vec<&str> = listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect();
    if paths.len() > CONTENT_FILES {
        rows.push(row(
            Kind::SharedFile,
            "COULD NOT LOOK",
            format!(
                "{} files of uid {uid} under {root}: the content of the {} beyond the first {CONTENT_FILES} was not read",
                paths.len(),
                paths.len() - CONTENT_FILES
            ),
        ));
    }
    for path in paths.into_iter().take(CONTENT_FILES) {
        let read = fs::File::open(path).and_then(|file| look::read_bounded(file, CONTENT_BYTES));
        match read {
            Ok((bytes, _)) if look::content_with_token(&bytes, tokens) => rows.push(row(
                Kind::SharedFile,
                path,
                format!("find -uid {uid} under {root}: a token is in the content of the file"),
            )),
            Ok((_, true)) => rows.push(row(
                Kind::SharedFile,
                "COULD NOT LOOK",
                format!("{path}: longer than {CONTENT_BYTES} bytes, the rest was not read"),
            )),
            Ok(_) => {}
            // The file went between the listing and the read: not there.
            Err(why) if why.raw_os_error() == Some(libc::ENOENT) => {}
            Err(why) => rows.push(row(
                Kind::SharedFile,
                "COULD NOT LOOK",
                format!("{path}: {why}"),
            )),
        }
    }
    rows
}

/// Root's `launchctl print` of `target` (`user/<uid>` or `gui/<uid>`), as the three answers.
fn print_domain(target: &str) -> Outcome {
    match Command::new("/bin/launchctl")
        .args(["print", target])
        .output()
    {
        Ok(out) => look::classify_launchctl_print(
            out.status.code(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ),
        Err(why) => Outcome::CouldNotLook(format!("launchctl: {why}")),
    }
}

/// A tool run as root whose exit status matters: its output, or why the look failed.
fn view(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|why| format!("{program}: {why}"))?;
    look::view_output(
        program,
        out.status.code(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )
}

/// Asserts that run B, on the uid run A had, finds nothing A left behind, and that the
/// system's views (`ipcs`, `ps`, `launchctl print`, `find -uid`) show nothing of A's either,
/// after the agent's own end-of-run calls and no other cleanup. It catches a user delete
/// that leaves the home's content, a keychain or defaults item, a process, a launchd agent,
/// the uid-keyed temp and cache directories, or a System V or POSIX object behind for the
/// next run that gets the uid. It fails with the table of what survived, and also when a
/// look could not be made or run A seeded too little to mean anything.
#[test]
#[ignore = "needs root and a Mac; creates a user"]
fn a_second_run_on_the_same_uid_finds_nothing_the_first_left() {
    let fixture = fixture();
    let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let rt = runtime(&fixture, &exe);
    let uid = fixture.uid;
    // The guard cleans up when it goes out of scope, on a panic too.
    let mut trial = Trial {
        rt,
        fixture,
        exe,
        runs: Vec::new(),
        seeded: Vec::new(),
    };
    let result = trial.body();
    drop(trial);
    match result {
        Err(why) => panic!("the test could not run: {why}"),
        Ok(rows) if rows.is_empty() => {
            println!("PROBE clean: nothing of run A survived for uid {uid}")
        }
        Ok(rows) => {
            for row in &rows {
                println!("PROBE survived {} {} {}", row[0], row[1], row[2]);
            }
            panic!(
                "{} item(s) survived between two runs of uid {uid}:\n{}",
                rows.len(),
                report::table(["KIND", "WHERE", "DETAIL"], &rows)
            );
        }
    }
}
