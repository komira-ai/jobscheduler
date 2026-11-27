//! The two-run isolation test of the Mac slot accounts, for real, as root, on a Mac.
//!
//! Question: does what a run leaves behind survive into the next run that gets the same
//! uid? Every run is lent a fresh account (`kbf-acct-<n>`) of the agent's account pool, made
//! for it and deleted when the run ends; the uid is the slot's, so the next account made in
//! the same slot has the same uid. A deletion may not leave what the account owned.
//!
//! The test builds the agent's real pool over the real `MacHost` in this process, with a
//! range of ONE uid and no account kept ready, and the agent's real supervisor
//! (`SystemLauncher` over the `js-agent` binary, which answers `__supervise-native`), so
//! the second run gets the uid of the first. Then:
//!
//! 1. Run A (a native run through the agent's own `NativeRuntime`, under the product's
//!    Seatbelt profile, as the run's account) leaves one item of every kind behind, each with a
//!    random token: a file in its home and in its scratch, a file in the system temp
//!    directory, a process left running (detached), a keychain item, a defaults key, a
//!    launchd agent in its user domain, System V shared memory, semaphore and message
//!    queue, and POSIX shared memory and a POSIX semaphore; and the things that outlive a home:
//!    a file in `DARWIN_USER_TEMP_DIR` and in `DARWIN_USER_CACHE_DIR` (the uid-keyed
//!    directories under `/var/folders`, whose exact paths run A reports), a file in
//!    `/Users/Shared`, and a launchd agent in the `gui/<uid>` domain. What the profile or the
//!    machine refuses is printed as refused, with the reason. A kind is covered when it was
//!    seeded or the profile denied the seed with the sandbox's own signature (the kernel's
//!    `Sandbox: <proc>(<pid>) deny(` line, or a bare permission error the unified log confirms
//!    as that seed's by the job's own pid, see `look.rs`): the job could not create the state, so nothing can carry over. Any other
//!    seed failure, a bare EPERM/EACCES included, proves nothing. A run that did not cover at
//!    least a file, a process, a keychain item, a launchd agent and shared memory, or seeded
//!    nothing itself, fails. A launch agent that root loaded is labelled "loaded by root" and
//!    is no sandbox claim. A launch agent that neither the job nor root can load (a pool uid
//!    has no login session, so launchd may have no domain for it) has NO PATH, and is
//!    covered, only on all of `no_path.rs`'s evidence: launchd's domain-level refusal of the
//!    job's own bootstrap, the job's plist valid by `plutil -lint`, every attempt of root run,
//!    and root's `launchctl print` showing the domain absent or without the agent; otherwise it is NOT EXERCISED:
//!    it covers nothing, and the run cannot pass. Root's every attempt is a `PROBE
//!    agent-attempt` line.
//! 2. Between the runs the test does what the agent does when a run ends and the next
//!    begins: `NativeRuntime::remove` (every process of the uid is killed, the account's
//!    record and home are deleted), and the next start is lent a new account on that uid. The
//!    test then checks what the removal itself left: no process of the uid, the account's
//!    home gone and its name no longer known to the system; anything left is a row of the
//!    table of survivors.
//! 3. Run B (the same program, another role, the same uid) looks for every token. The test
//!    also reads the system-wide views: `ipcs` for objects owned by the uid, `ps` for
//!    processes of the uid, `launchctl print` of `user/<uid>` and `gui/<uid>`, and `find -uid`
//!    (every path, no depth limit) over `/private/var/folders`, `/private/tmp`,
//!    `/Users/Shared` and `/Library/Caches`: a path with a token in it fails, so does a token
//!    in the content of a file (the first 2000 files, 1 MiB each; more is a could-not-look);
//!    other files of the uid are printed as notes. A run that fails midway is still cleaned up:
//!    its handle and what it printed are recorded as soon as it exists.
//!    Each run prints its own `getuid()`; A, B and the configured uid must be equal.
//! 4. The verdict has four outcomes: PASS (every kind was seeded, or denied by the sandbox, and
//!    nothing survived); PASS-WITH-UNEXERCISED <kinds> (the minimum set was met and nothing
//!    survived, but some kinds could not be seeded and are unproven: the message names them
//!    with their reasons and the process exits non-zero, so it cannot pass silently); FAIL
//!    (anything survived, or a look could not be made); COULD-NOT-REACH-A-VERDICT (the minimum
//!    set was not met or a control failed). The run always goes to its end and prints every
//!    table.
//!
//! 5. Every look has three answers: found, absent (only by the documented not-found answer
//!    of the call or tool, see `look.rs`) or could-not-look, which fails the test with its
//!    reason. Anything found fails the test with a table of what survived. Nothing here asserts
//!    anything about time.
//!
//! It lives in js-agent because the question is about what the agent's runtime leaves: the
//! test drives the runtime's own start and remove (the calls the agent makes) over the same
//! pool and supervisor code the agent runs.
//!
//! Every test that touches the machine is `#[ignore]`: it needs root, a Mac and a free uid,
//! and it makes an account. Run the binary as root with `JS_NATIVE_ISO_ROOT` naming a new
//! directory, owned by root and mode 0755, that every user can search down to, and
//! `JS_NATIVE_ISO_UID` naming a uid in 501 to 60000, outside 600 to 799 (the range the agent's
//! own accounts use), that nothing uses, with `--ignored --test-threads=1 --nocapture`. The
//! supervisor is the `js-agent` binary next to the test binary when there is one, else the
//! one cargo built for it (`CARGO_BIN_EXE_js-agent`). See the `js-agent-native-isolation-test`
//! workflow. Never run it on a production host.
//!
//! One difference from production: the runtime here has the directory of this test binary as
//! an extra tool root (the runs execute the binary), a read/exec allowance the agent
//! does not have; and the account pool's state and homes are below the test's root
//! directory, not the agent's own. Not tested here: cron/at jobs and extended attributes.
//!
//! The file builds on every platform, so clippy reads it on Linux; the parsers and the
//! report are tested there. Run anywhere but macOS, the ignored test stops at its first
//! line.

#![cfg(target_os = "macos")]

mod after_seed;
mod child;
mod lines;
mod look;
mod no_path;
mod report;
mod token;
mod views;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::mac_accounts::{MacHost, Pool, Settings, Slots};
use js_agent::mac_supervisor::SystemLauncher;
use js_agent::native::{Accounts, Ceiling, NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

use lines::{Kind, Line, Verdict, parse_lines};
use look::Outcome;
use token::Token;

/// The variable naming the directory the test works in.
const ROOT: &str = "JS_NATIVE_ISO_ROOT";
/// The variable naming the one uid of the pool's range.
const UID: &str = "JS_NATIVE_ISO_UID";
/// The uids the agent's own accounts use; the test's uid is outside them.
const AGENT_UIDS: std::ops::RangeInclusive<u32> = 600..=799;
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
    println!("ISOPID {}", std::process::id());
    match role.as_str() {
        "seed" => {
            let tmp = PathBuf::from(std::env::var("TMPDIR").expect("the agent sets TMPDIR"));
            let spot = child::Spot {
                home: PathBuf::from(std::env::var("HOME").expect("the agent sets HOME")),
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
            let home = PathBuf::from(std::env::var("HOME").expect("the agent sets HOME"));
            for line in child::look_all(&seeded, &home) {
                println!("{}", line.encode());
            }
        }
        other => panic!("unknown role {other:?}"),
    }
}

/// Where the test keeps things: the pool's state and homes are below its root.
struct Fixture {
    root: PathBuf,
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
    assert!(
        !AGENT_UIDS.contains(&uid),
        "{UID} is {uid}, in {AGENT_UIDS:?}, which the agent's own accounts use"
    );
    let fixture = Fixture {
        homes: root.join("homes"),
        state: root.join("state"),
        root,
        uid,
    };
    fs::create_dir(&fixture.homes).unwrap();
    fs::create_dir(&fixture.state).unwrap();
    // The pool refuses a state directory that is not root's and writable by others.
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(fixture.root.join("scratch")).unwrap();
    fixture
}

/// The supervisor binary: the `js-agent` next to the test binary when there is one (the
/// operator copies both), else the one cargo built for this test.
fn supervisor_binary(exe: &Path) -> PathBuf {
    let beside = exe.parent().unwrap().join("js-agent");
    if beside.is_file() {
        beside
    } else {
        PathBuf::from(env!("CARGO_BIN_EXE_js-agent"))
    }
}

/// The production layout of the runtime, plus the directory this binary is in as a tool
/// root (the runs execute it).
fn runtime(fixture: &Fixture, exe: &Path) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let slots = Slots::new(fixture.uid, 1).unwrap();
    // No account is kept ready, so every run is lent an account made for it.
    let mut settings = Settings::macos(slots, 0);
    settings.homes = fs::canonicalize(&fixture.homes).unwrap();
    let pool = Pool::with_settings(
        &fixture.state,
        settings.clone(),
        &BTreeSet::new(),
        Arc::new(MacHost),
    )
    .unwrap_or_else(|why| panic!("the account pool: {why}"));
    let launcher = Arc::new(SystemLauncher::at(supervisor_binary(exe)));
    let config = NativeConfig {
        accounts: Accounts {
            state: fixture.state.clone(),
            slots,
            ready: 0,
        },
        homes: settings.homes,
        network: Ceiling::Off,
        tool_roots: vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
            exe.parent().unwrap().to_owned(),
        ],
        read_denies: vec![
            denied(&fixture.state),
            denied(&fixture.root.join("scratch")),
        ],
        mount_roots: Vec::new(),
        host_ports: Vec::new(),
        read_model: ReadModel::Open,
        journal: fixture.root.join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    };
    let term = TERMS.fetch_add(1, Ordering::SeqCst);
    NativeRuntime::over(Some(Arc::new(pool)), launcher, config, term).unwrap()
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
            ports: Vec::new(),
        }),
        env: Vec::new(),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch,
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        fuse: false,
        labels: Default::default(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        log_sink: None,
        xcode: None,
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

/// The account of the run that is lent the uid, found by the home it owns below the homes
/// directory: its name and its home. Exactly one account has the uid while a run holds it.
fn account_of(homes: &Path, uid: u32) -> Result<(String, PathBuf), String> {
    let entries = fs::read_dir(homes).map_err(|why| format!("the homes directory: {why}"))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|why| format!("the homes directory: {why}"))?;
        let meta = entry
            .metadata()
            .map_err(|why| format!("{}: {why}", entry.path().display()))?;
        if meta.uid() == uid {
            found.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            ));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        n => Err(format!(
            "{n} homes below {} are owned by uid {uid}, not one",
            homes.display()
        )),
    }
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
    pids: Vec<u32>,
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
            pids: look::parse_pids(&stdout),
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
    /// Why the result would prove nothing; kept whatever ends the run.
    unproven: Vec<String>,
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
    /// The two runs and the views; the table of survivors, empty when the uid is clean.
    fn body(&mut self) -> Result<Vec<Row>, String> {
        let uid = self.fixture.uid;
        let exe = self.exe.clone();

        // Run A.
        // The local time just before run A, for `log show --start`.
        let started = view("/bin/date", &["+%Y-%m-%d %H:%M:%S"])?
            .trim()
            .to_owned();
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
        let (user_a, home_a) = account_of(&self.fixture.homes, uid)
            .map_err(|why| format!("the account of run A: {why}"))?;
        let job = look::JobProcs {
            pids: ran_a.pids.clone(),
        };
        self.confirm_denials_by_log(&started, &job);
        // The job's own home, where it left its files, is below the run directory.
        self.bootstrap_agents_as_root(uid, &home_a.join("exec").join(".home"));
        println!("{}", self.seeded_table());
        let missing = look::missing_minimum(&self.seeded);
        if !missing.is_empty() {
            self.unproven.push(format!(
                "run A neither seeded nor had the profile deny {}: the result would prove nothing",
                missing.join(", ")
            ));
        }
        if let Err(why) = self.control(uid) {
            self.unproven.push(why);
        }

        // Between the runs: what the agent does when a run ends.
        self.rt
            .remove(&a)
            .map_err(|why| format!("remove run A: {}", why.0))?;
        self.runs.clear();
        let mut survivors = self.after_removal(uid, &user_a, &home_a)?;

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
        let (user_b, _) = account_of(&self.fixture.homes, uid).map_err(|why| {
            format!(
                "run B did not run as an account of uid {uid}, so this run proves nothing: {why}"
            )
        })?;
        look::uids_agree(ran_a.uid, ran_b.uid, uid)?;

        survivors.extend(ran_b.lines.iter().filter_map(|line| match line.verdict {
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
        }));
        survivors.extend(self.system_views(uid, &[&user_a, &user_b])?);
        Ok(survivors)
    }

    /// What the removal of run A's account left: a process of the uid, the account's home, or
    /// the account's name still known to the system. A look that fails is an error.
    fn after_removal(&self, uid: u32, user: &str, home: &Path) -> Result<Vec<Row>, String> {
        let mut rows = Vec::new();
        let ps = view("/bin/ps", &["-A", "-o", "uid=,pid=,command="])?;
        for proc in views::procs_of_uid(&views::parse_ps(&ps), uid) {
            rows.push(row(
                Kind::Process,
                format!("pid={}", proc.pid),
                format!(
                    "ps lists it for uid {uid} after the removal: {}",
                    proc.command
                ),
            ));
        }
        match fs::symlink_metadata(home) {
            Ok(_) => rows.push(row(
                Kind::HomeFile,
                home.display().to_string(),
                "the home of the removed account is still there",
            )),
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
            Err(why) => return Err(format!("{}: {why}", home.display())),
        }
        let (known, text) = tool_output("/usr/bin/id", &["-u", user])?;
        if known {
            rows.push(row(
                Kind::HomeFile,
                user.to_owned(),
                format!(
                    "the removed account is still known to the system: {}",
                    text.trim()
                ),
            ));
        }
        Ok(rows)
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
                        Verdict::NoPath => "no path (refused the job and root)".to_owned(),
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
    fn system_views(&self, uid: u32, users: &[&str]) -> Result<Vec<Row>, String> {
        let tokens = self.tokens();
        let mut rows = Vec::new();

        let ipcs = view("/usr/bin/ipcs", &["-a"])?;
        let objects = views::parse_ipcs(&ipcs);
        for object in views::ipc_owned_by(&objects, uid, users) {
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
            let found = view_bytes(
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
            rows.extend(other_volume_rows(root));
            match found {
                Ok(listing) => {
                    let text = String::from_utf8_lossy(&listing);
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
                    rows.extend(scan_content(uid, root, &listing, &tokens));
                }
                Err(why) => rows.push(row(Kind::SharedFile, "COULD NOT LOOK", why)),
            }
        }
        Ok(rows)
    }

    /// Takes down what the trial made, whatever happened (it runs from `Drop`, so also on a
    /// panic): the launchd domains, the accounts, the objects the runs left. Best effort, and
    /// no wider than that.
    fn cleanup(&mut self) {
        // The domains go out before the account is deleted: the pool's kill rounds end
        // them too, but one left would make the next run on the uid refuse.
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

/// `find -x` does not enter a volume mounted directly below a root: each such child is a
/// COULD NOT LOOK row (it fails the test), and so is a root that cannot be listed. A mount
/// deeper down is not detected here.
fn other_volume_rows(root: &str) -> Vec<Row> {
    let unlooked = |why: String| vec![row(Kind::SharedFile, "COULD NOT LOOK", why)];
    let root_dev = match fs::metadata(root) {
        Ok(meta) => meta.dev(),
        Err(why) => return unlooked(format!("{root}: {why}")),
    };
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(why) => return unlooked(format!("{root}: {why}")),
    };
    let mut children = Vec::new();
    let mut rows = Vec::new();
    for entry in entries {
        match entry.and_then(|entry| Ok((entry.path(), fs::symlink_metadata(entry.path())?))) {
            Ok((path, meta)) => children.push((path.display().to_string(), meta.dev())),
            Err(why) => rows.extend(unlooked(format!(
                "{root}: a child could not be read: {why}"
            ))),
        }
    }
    for path in look::other_volume_children(root_dev, &children) {
        rows.extend(unlooked(format!(
            "{path} is on another volume than {root}: find -x did not look in it"
        )));
    }
    rows
}

/// The files `find` listed, read for the tokens (a file whose name lacks the token, a cfprefs
/// plist or a keychain database, can still hold it). At most `CONTENT_FILES` files and
/// `CONTENT_BYTES` of each: what is beyond is a COULD NOT LOOK row, never a pass.
fn scan_content(uid: u32, root: &str, listing: &[u8], tokens: &[Token]) -> Vec<Row> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut rows = Vec::new();
    let paths = look::nul_paths(listing);
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
    for raw in paths.into_iter().take(CONTENT_FILES) {
        let path = String::from_utf8_lossy(raw).into_owned();
        let read = fs::File::open(std::ffi::OsStr::from_bytes(raw))
            .and_then(|file| look::read_bounded(file, CONTENT_BYTES));
        match read {
            Ok((bytes, _)) if look::content_with_token(&bytes, tokens) => rows.push(row(
                Kind::SharedFile,
                path.clone(),
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
    match run_bounded("/bin/launchctl", &["print", target]) {
        Ok((code, stdout, stderr)) => {
            look::classify_launchctl_print(code, &String::from_utf8_lossy(&stdout), &stderr)
        }
        Err(why) => Outcome::CouldNotLook(format!("launchctl: {why}")),
    }
}

/// A tool run as root whose exit status matters: its output, or why the look failed.
fn view(program: &str, args: &[&str]) -> Result<String, String> {
    let (code, stdout, stderr) = run_bounded(program, args)?;
    look::view_output(program, code, &String::from_utf8_lossy(&stdout), &stderr)
}

/// Like [`view`], with the output as raw bytes (a path need not be UTF-8).
fn view_bytes(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let (code, stdout, stderr) = run_bounded(program, args)?;
    look::view_output(program, code, "", &stderr)?;
    Ok(stdout)
}

/// The most bytes of a view's output that are read; more is a failed look (and the tool is
/// stopped), never a truncated view.
const OUTPUT_CAP: u64 = 64 << 20;

/// A tool run to the end with its output read up to [`OUTPUT_CAP`]: exit status, standard
/// output, standard error (cut at 1 MiB).
fn run_bounded(program: &str, args: &[&str]) -> Result<(Option<i32>, Vec<u8>, String), String> {
    use std::process::Stdio;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|why| format!("{program}: {why}"))?;
    let stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(mut stream) = stderr {
            if let Ok((bytes, _)) = look::read_bounded(&mut stream, 1 << 20) {
                text = bytes;
            }
            // Keep reading so the tool never blocks on a full pipe.
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
        }
        String::from_utf8_lossy(&text).into_owned()
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{program}: no output pipe"))?;
    let (bytes, more) =
        look::read_bounded(stdout, OUTPUT_CAP).map_err(|why| format!("{program}: {why}"))?;
    if more {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{program}: more than {OUTPUT_CAP} bytes of output"));
    }
    let status = child.wait().map_err(|why| format!("{program}: {why}"))?;
    let errors = errors.join().unwrap_or_default();
    Ok((status.code(), bytes, errors))
}

/// Asserts that run B, on the uid run A had, finds nothing A left behind, and that the
/// system's views (`ipcs`, `ps`, `launchctl print`, `find -uid`) show nothing of A's either,
/// after the agent's own end-of-run calls and no other cleanup; and that the removal of A's
/// account left no process of the uid, no home and no record of the account. It catches an
/// account removal that leaves the home's content, a keychain or defaults item, a process, a launchd agent,
/// the uid-keyed temp and cache directories, or a System V or POSIX object behind for the
/// next run that gets the uid. It fails with the table of what survived, and also when a
/// look could not be made or run A seeded too little to mean anything.
#[test]
#[ignore = "needs root and a Mac; creates an account"]
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
        unproven: Vec::new(),
    };
    let result = trial.body();
    let unproven = std::mem::take(&mut trial.unproven);
    let seeded = trial.seeded.clone();
    drop(trial);
    let rows = match result {
        Ok(rows) => rows,
        Err(why) => panic!(
            "the test could not run: {}",
            look::reasons_message(Some(&why), &unproven)
        ),
    };
    for row in &rows {
        println!("PROBE survived {} {} {}", row[0], row[1], row[2]);
    }
    let verdict = look::final_verdict(&seeded, rows.len(), &unproven);
    println!("PROBE verdict {verdict:?}");
    let table = report::table(["KIND", "WHERE", "DETAIL"], &rows);
    match &verdict {
        look::RunVerdict::Pass => {
            println!(
                "PROBE clean: nothing of run A survived for uid {uid}; every kind was exercised"
            );
            println!("{}", look::verdict_message(&verdict, &seeded));
        }
        look::RunVerdict::PassWithUnexercised(_) => {
            // Not a silent pass: the process exits non-zero with this message.
            panic!("{}", look::verdict_message(&verdict, &seeded));
        }
        look::RunVerdict::Fail => panic!(
            "{} item(s) survived between two runs of uid {uid}:\n{table}\n{}",
            rows.len(),
            look::reasons_message(None, &unproven)
        ),
        look::RunVerdict::NoVerdict => panic!(
            "{}: {}",
            look::verdict_message(&verdict, &seeded),
            look::reasons_message(None, &unproven)
        ),
    }
}
