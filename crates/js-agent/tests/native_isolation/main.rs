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
//!    queue, and POSIX shared memory and a POSIX semaphore. What the profile or the machine
//!    refuses is printed as refused, with the reason.
//! 2. Between the runs the test does what the agent does when a run ends and the next
//!    begins: `NativeRuntime::remove` (the helper's `kill-uid`, then `user-delete`), and
//!    the next start makes the user again (`user-create`).
//! 3. Run B (the same program, another role, the same uid) looks for every token. The test
//!    also reads the system-wide views: `ipcs` for objects owned by the uid, `ps` for
//!    processes of the uid, and `launchctl print user/<uid>`.
//! 4. Anything found fails the test with a table of what survived. Nothing here asserts
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
//! --test-threads=1 --nocapture`. See the `js-agent-native-privileged-tests` workflow. Never
//! run it on a production host.
//!
//! The file builds on every platform, so clippy reads it on Linux; the parsers and the
//! report are tested there. Run anywhere but macOS, the ignored test stops at its first
//! line.

mod child;
mod lines;
mod report;
mod token;
mod views;

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};
use js_mac_session::client::Client;

use lines::{Kind, Line, Verdict, parse_lines};
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

/// Starts one run of the child in `role`, waits for it and returns its handle and what it
/// printed. The child must exit 0.
fn child_run(
    rt: &mut NativeRuntime,
    fixture: &Fixture,
    exe: &Path,
    name: &str,
    env: &[(&str, &str)],
) -> Result<(RunHandle, Vec<Line>), String> {
    let id = rt
        .create_and_start(&spec(fixture, name, exe, env))
        .map_err(|why| format!("start run {name}: {}", why.0))?;
    let status = settle(rt, &id)?;
    let out = fixture.root.join("scratch").join(name);
    let stdout = fs::read_to_string(out.join("stdout")).unwrap_or_default();
    let stderr = fs::read_to_string(out.join("stderr")).unwrap_or_default();
    let clean = matches!(status, Status::Exited(Exit { code: 0, .. }));
    if !clean {
        return Err(format!(
            "run {name} ended {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ));
    }
    Ok((id, parse_lines(&stdout)?))
}

/// Everything a trial needs to clean up after itself.
struct Trial {
    rt: NativeRuntime,
    fixture: Fixture,
    exe: PathBuf,
    runs: Vec<RunHandle>,
    seeded: Vec<Line>,
}

impl Trial {
    /// The two runs and the views; the table of survivors, empty when the pool user is clean.
    fn body(&mut self) -> Result<Vec<[String; 3]>, String> {
        let uid = self.fixture.uid;
        let exe = self.exe.clone();

        // Run A.
        let (a, lines) = child_run(&mut self.rt, &self.fixture, &exe, "a", &[(ROLE, "seed")])?;
        self.runs.push(a.clone());
        let owner = fs::metadata(self.fixture.homes.join(user_of(&a)))
            .map_err(|why| format!("the home of run A: {why}"))?
            .uid();
        if owner != uid {
            return Err(format!(
                "run A ran as uid {owner}, not the uid {uid} asked for"
            ));
        }
        self.seeded = lines;
        self.bootstrap_agent_as_root(uid);
        println!("{}", self.seeded_table());
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
        let (b, found) = child_run(
            &mut self.rt,
            &self.fixture,
            &exe,
            "b",
            &[(ROLE, "look"), (HANDOFF, &handoff_text)],
        )?;
        self.runs.push(b.clone());
        let owner = fs::metadata(self.fixture.homes.join(user_of(&b)))
            .map_err(|why| format!("the home of run B: {why}"))?
            .uid();
        if owner != uid {
            return Err(format!(
                "run B got uid {owner}, not run A's uid {uid}: the helper did not reuse it, so this run proves nothing"
            ));
        }

        let mut survivors: Vec<[String; 3]> = found
            .iter()
            .filter(|line| line.verdict == Verdict::Found)
            .map(|line| {
                [
                    line.kind.label().to_owned(),
                    line.text.clone(),
                    format!("run B found it (token {})", line.token),
                ]
            })
            .collect();
        survivors.extend(self.system_views(uid, &user_of(&b))?);
        Ok(survivors)
    }

    fn tokens(&self) -> Vec<Token> {
        self.seeded.iter().map(|line| line.token.clone()).collect()
    }

    fn seeded_table(&self) -> String {
        let rows: Vec<[String; 3]> = self
            .seeded
            .iter()
            .map(|line| {
                [
                    line.kind.label().to_owned(),
                    match line.verdict {
                        Verdict::Seeded => "seeded".to_owned(),
                        _ => "refused".to_owned(),
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

    /// A launch agent run A could not load (a user cannot load into its own domain on
    /// every macOS) is loaded by root into the user's domain: the item is still the run's
    /// to leave, and the domain is the one the helper's delete does not end.
    fn bootstrap_agent_as_root(&mut self, uid: u32) {
        let Some(line) = self
            .seeded
            .iter_mut()
            .find(|line| line.kind == Kind::LaunchAgent && line.verdict == Verdict::Refused)
        else {
            return;
        };
        let label = format!("com.komira.iso.{}", line.token);
        let plist = self.fixture.root.join(format!("{label}.plist"));
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n"
        );
        if fs::write(&plist, body).is_err() {
            return;
        }
        let domain = format!("user/{uid}");
        let loaded = tool_output(
            "/bin/launchctl",
            &["bootstrap", &domain, &plist.display().to_string()],
        );
        if matches!(loaded, Ok((true, _))) {
            *line = Line::new(
                Verdict::Seeded,
                Kind::LaunchAgent,
                line.token.clone(),
                &format!("{label} (loaded by root into {domain}: run A could not)"),
            );
        }
    }

    /// Before the runs change over: every item that can be seen from root is seen, and
    /// the process is running, so that "run B found nothing" cannot mean "nothing was
    /// ever there". A seeded item that is not seen fails the test: it proves nothing.
    fn control(&self, uid: u32) -> Result<(), String> {
        let tokens = self.tokens();
        let (_, ps) = tool_output("/bin/ps", &["-A", "-o", "uid=,pid=,command="])?;
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
                | Kind::SysvShm
                | Kind::SysvSem
                | Kind::SysvMsg
                | Kind::PosixShm
                | Kind::PosixSem => child::probe(line, Path::new("/")).is_some(),
                // Not visible to root as the user's own: the keychain and the defaults
                // key are in the user's home, the agent in its launchd domain.
                Kind::Keychain | Kind::Defaults | Kind::LaunchAgent => true,
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
    /// processes by uid, the user's launchd domain.
    fn system_views(&self, uid: u32, user: &str) -> Result<Vec<[String; 3]>, String> {
        let tokens = self.tokens();
        let mut rows = Vec::new();

        let (_, ipcs) = tool_output("/usr/bin/ipcs", &["-a"])?;
        let objects = views::parse_ipcs(&ipcs);
        for object in views::ipc_owned_by(&objects, uid, &[user]) {
            let kind = match object.kind {
                views::IpcKind::Message => Kind::SysvMsg,
                views::IpcKind::Shared => Kind::SysvShm,
                views::IpcKind::Semaphore => Kind::SysvSem,
            };
            rows.push([
                kind.label().to_owned(),
                format!("key={} id={}", object.key, object.id),
                format!("ipcs lists it as owned by {}", object.owner),
            ]);
        }

        let (_, ps) = tool_output("/bin/ps", &["-A", "-o", "uid=,pid=,command="])?;
        let procs = views::parse_ps(&ps);
        for proc in views::procs_with_token(&procs, uid, &tokens) {
            rows.push([
                Kind::Process.label().to_owned(),
                format!("pid={}", proc.pid),
                format!("ps lists it for uid {uid}: {}", proc.command),
            ]);
        }
        // Other processes of the uid carry no token and are not a failure (the user's
        // launchd domain runs its own agents); they are printed for the reader.
        for proc in views::procs_of_uid(&procs, uid) {
            println!(
                "PROBE note process of uid {uid} after run B: pid={} {}",
                proc.pid, proc.command
            );
        }

        let domain = format!("user/{uid}");
        let (ok, print) = tool_output("/bin/launchctl", &["print", &domain])?;
        println!("PROBE note launchctl print {domain} succeeded={ok}");
        for line in views::lines_with_token(&print, &tokens) {
            rows.push([
                Kind::LaunchAgent.label().to_owned(),
                domain.clone(),
                format!("launchctl print: {line}"),
            ]);
        }
        Ok(rows)
    }

    /// Takes down what the trial made, whatever happened: the users, the objects the runs
    /// left, the user's launchd domain. Best effort.
    fn cleanup(&mut self) {
        // The domain goes out before the user is deleted: deleting a user does not end
        // its launchd domain, and one left makes the next run on the uid refuse.
        let domain = format!("user/{}", self.fixture.uid);
        let _ = tool_output("/bin/launchctl", &["bootout", &domain]);
        for id in std::mem::take(&mut self.runs) {
            let _ = self.rt.remove(&id);
        }
        for line in &self.seeded {
            child::remove(line);
        }
    }
}

/// Asserts that run B, on the uid run A had, finds nothing A left behind, and that the
/// system's views (`ipcs`, `ps`, `launchctl print`) show nothing of A's either, after the
/// agent's own end-of-run calls and no other cleanup. It catches a user delete that
/// leaves the home's content, a keychain or defaults item, a process, a launchd agent or a
/// System V or POSIX object behind for the next run that gets the uid. It fails with the
/// table of what survived.
#[test]
#[ignore = "needs root and a Mac; creates a user"]
fn a_second_run_on_the_same_uid_finds_nothing_the_first_left() {
    let fixture = fixture();
    let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let rt = runtime(&fixture, &exe);
    let uid = fixture.uid;
    let mut trial = Trial {
        rt,
        fixture,
        exe,
        runs: Vec::new(),
        seeded: Vec::new(),
    };
    let result = trial.body();
    trial.cleanup();
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
