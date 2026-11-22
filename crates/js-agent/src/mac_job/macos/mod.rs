//! `__mac-job` on macOS: the set-up, the start, the sweep and the check (see [`super`]).

mod files;
mod puts;
mod sys;
mod trees;

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use js_mac_session::disclaim::Program;
use js_types::Arch;

use crate::mac_accounts::Pool;

use super::layout::{Layout, SUBFOLDERS};
use super::profile::{self, JOB_PARAM};
use super::report::{self, AccountUse, Ended, Leftovers, Report};
use super::spec::JobFile;
use super::start::StartFile;
use super::sweep::{self, JobIds, ROUNDS_MAX, Tally};
use super::{Args, AuditSession, Spawn, status};

/// How often the wait for the program looks.
const POLL: Duration = Duration::from_millis(50);

/// The pause between sweep rounds, for a killed process to become a zombie and be reaped.
const SWEEP_PAUSE: Duration = Duration::from_millis(20);

/// The profile text and the parameters it names beside `JOB`.
struct Profile<'a> {
    text: &'a str,
    params: &'a [(String, String)],
}

/// Why the mode stops before a report.
enum Stop {
    /// The command line or the job file is wrong.
    Usage(String),
    /// The job could not be set up or started.
    Setup(String),
}

/// This host's architecture.
fn here() -> Arch {
    if cfg!(target_arch = "aarch64") {
        Arch::Arm64
    } else {
        Arch::X86_64
    }
}

/// Runs the mode; the status of [`super::status`].
pub(super) fn run(args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    match run_job(args) {
        Ok(report) => {
            let text = serde_json::to_string_pretty(&report).unwrap_or_default();
            let _ = writeln!(out, "{text}");
            if let Err(why) = fs::write(args.out.join("report.json"), format!("{text}\n")) {
                let _ = writeln!(err, "js-agent: writing the report: {why}");
            }
            report.status()
        }
        Err(Stop::Usage(why)) => {
            let _ = writeln!(err, "js-agent: {why}");
            status::USAGE
        }
        Err(Stop::Setup(why)) => {
            let _ = writeln!(err, "js-agent: {why}");
            status::SETUP
        }
    }
}

/// The run, up to its report.
fn run_job(args: &Args) -> Result<Report, Stop> {
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(Stop::Setup(format!("{} runs as root", super::VERB)));
    }
    let text = match (&args.job, &args.job_inline) {
        (_, Some(inline)) => inline.clone(),
        (Some(path), None) => fs::read_to_string(path)
            .map_err(|why| Stop::Usage(format!("{}: {why}", path.display())))?,
        (None, None) => return Err(Stop::Usage("no job named".to_owned())),
    };
    let job = JobFile::parse(&text, here()).map_err(|why| Stop::Usage(why.to_string()))?;
    let added = match &args.profile_add {
        Some(path) => fs::read_to_string(path)
            .map_err(|why| Stop::Usage(format!("{}: {why}", path.display())))?,
        None => String::new(),
    };
    let pool = match (&args.accounts, args.account_range) {
        (Some(state), Some(slots)) => Some(make_private(state).and_then(|()| {
            Pool::open(state, slots, args.ready, &std::collections::BTreeSet::new())
                .map_err(|why| Stop::Setup(format!("the account pool: {why}")))
        })?),
        (Some(_), None) => return Err(Stop::Usage("--accounts needs --account-range".to_owned())),
        (None, _) => None,
    };
    let Some(pool) = pool else {
        let Some(name) = &args.user else {
            return Err(Stop::Usage("neither --accounts nor --user".to_owned()));
        };
        return run_as(args, job, &added, name);
    };
    let lent = pool
        .lend(&job.id)
        .map_err(|why| Stop::Setup(format!("lending an account: {why}")))?;
    let name = lent.account.name.clone();
    // The account is retired whatever the run did: it ran, or was meant to run, a job.
    let run = run_as(args, job, &added, &name);
    let retired = pool
        .retire(lent.account)
        .map(|done| format!("{done:?}"))
        .map_err(|held| held.to_string());
    let account = AccountUse {
        name,
        made_now: lent.made_now,
        retired,
    };
    match run {
        Ok(report) => {
            // A retirement shown gone judged the home and the per-user folders after the
            // look the run made before it: only what is still there outside them counts.
            let leftovers = match (&account.retired, args.account_range) {
                (Ok(_), Some(slots)) => report.leftovers.after_retirement(
                    &crate::mac_accounts::Settings::macos(slots, args.ready).user_folders,
                    |file| fs::symlink_metadata(file).is_ok(),
                ),
                _ => report.leftovers,
            };
            Ok(Report {
                account: Some(account),
                leftovers,
                ..report
            })
        }
        Err(Stop::Usage(why)) => Err(Stop::Usage(format!("{why}; account: {account:?}"))),
        Err(Stop::Setup(why)) => Err(Stop::Setup(format!("{why}; account: {account:?}"))),
    }
}

/// The run as the user `name`, up to its report.
fn run_as(args: &Args, job: JobFile, added: &str, name: &str) -> Result<Report, Stop> {
    let user = sys::user(name).map_err(|why| Stop::Setup(format!("{name}: {why}")))?;
    if user.uid == 0 {
        return Err(Stop::Setup(format!("{name} is root")));
    }
    let groups = sys::groups(name, user.gid).map_err(|why| Stop::Setup(why.to_string()))?;
    if groups.contains(&sys::ADMIN_GID) {
        return Err(Stop::Setup(format!("{name} is an administrator")));
    }
    let before = user_processes(user.uid)?;
    if !before.is_empty() {
        return Err(Stop::Setup(format!(
            "{name} runs {} process(es) already ({before:?}); one job per user at a time",
            before.len()
        )));
    }
    let root = checked_root(&args.root)?;
    let layout = Layout::new(&root, &job.id);
    fs::create_dir_all(&args.out)
        .map_err(|why| Stop::Setup(format!("{}: {why}", args.out.display())))?;

    let scan_roots = files::scan_roots(&user.home, user.uid);
    let snapshot = files::Snapshot::take(&scan_roots, user.uid);
    make_folder(&layout, &job, user.uid, user.gid)?;
    let mounted = match trees::link(args, &job, &layout, user.uid, user.gid) {
        Ok(mounted) => mounted,
        Err(why) => {
            let _ = files::remove_folder(&layout.folder);
            return Err(Stop::Setup(why));
        }
    };

    let rules = trees::rules(args, &mounted).and_then(|(text, params)| {
        let made = puts::make(args, &job, &layout, user.uid)?;
        Ok((text, params, made))
    });
    let (mount_text, mount_params, put_dirs) = match rules {
        Ok(rules) => rules,
        Err(why) => {
            let _ = trees::release(args, &job, mounted);
            let _ = files::remove_folder(&layout.folder);
            return Err(Stop::Setup(why));
        }
    };
    let (put_text, put_params) = profile::put_rules(&put_dirs);
    let mount_params: Vec<(String, String)> = mount_params.into_iter().chain(put_params).collect();
    let profile_text = profile::render(
        args.profile,
        &job.network,
        &format!("{mount_text}{put_text}"),
        added,
    );
    let new_session = args.audit_session == AuditSession::New;
    let started = Instant::now();
    let profile_of = Profile {
        text: &profile_text,
        params: &mount_params,
    };
    let started_job = start(&job, &layout, name, &user, &profile_of, new_session, args);
    let mut child = match started_job {
        Ok(started) => started,
        Err(why) => {
            // Nothing ran: the trees and the folder go, and the stop says why.
            let _ = trees::release(args, &job, mounted);
            let _ = files::remove_folder(&layout.folder);
            return Err(Stop::Setup(why));
        }
    };
    let deadline = started + Duration::from_secs(job.wall_limit_secs);
    let ended = wait(&mut child, deadline);
    let run_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let asid = if new_session {
        read_asid(&args.out)
    } else {
        None
    };

    let ids = JobIds {
        uid: user.uid,
        asid,
        own_pid: std::process::id().cast_signed(),
    };
    let sweep = sweep_job(&ids);
    let trees = trees::release(args, &job, mounted);
    let put_dirs = puts::report(&job, &put_dirs);
    let outputs = files::collect(&layout.work(), &job.outputs, &args.out.join("outputs"));
    let put = match (&args.put_ingest, job.put_dirs.is_empty()) {
        (Some(root), true) => Some(commit_outputs(args, root, &job, &outputs)),
        _ => None,
    };
    let folder = files::remove_folder(&layout.folder)
        .err()
        .map(|why| format!("{}: {why}", layout.folder.display()));
    let (processes, unchecked) = match user_processes(user.uid) {
        Ok(processes) => (processes, None),
        Err(Stop::Usage(why) | Stop::Setup(why)) => (Vec::new(), Some(why)),
    };
    let leftovers = Leftovers {
        processes,
        unchecked,
        files: snapshot.changed(&scan_roots, user.uid),
        folder: folder.or_else(|| {
            layout
                .folder
                .exists()
                .then(|| format!("{} is still there", layout.folder.display()))
        }),
    };
    Ok(Report {
        id: job.id,
        user: name.to_owned(),
        uid: user.uid,
        asid,
        folder: layout.folder.display().to_string(),
        profile: profile_text,
        ended,
        run_ms,
        sweep,
        outputs,
        leftovers,
        put_dirs,
        put,
        trees,
        account: None,
        spawn: format!("{:?}", args.spawn).to_lowercase(),
        stdout_tail: report::tail(&fs::read(args.out.join("stdout")).unwrap_or_default()),
        stderr_tail: report::tail(&fs::read(args.out.join("stderr")).unwrap_or_default()),
    })
}

/// Commits the outputs the agent copied to `<out>/outputs` to the put ingest at `root`, its
/// directories given to the node's user as the node needs them: the key's folders its, the
/// job's put directory root's with the node's group able to write it (the node removes the file
/// and the directory). An output that was not copied is not committed: the commit fails.
fn commit_outputs(
    args: &Args,
    root: &Path,
    job: &JobFile,
    copied: &super::report::Outputs,
) -> Result<super::put::Committed, String> {
    if !copied.missing.is_empty() {
        return Err(format!("outputs missing: {}", copied.missing.join("; ")));
    }
    let node = puts::node_user(args)?;
    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce).map_err(|why| format!("a nonce: {why}"))?;
    let nonce = hex::encode(nonce);
    let ready = |dir: &Path| -> std::io::Result<()> {
        let mut at = root.to_owned();
        for part in ["cas", "b", job.id.as_str()] {
            at.push(part);
            std::os::unix::fs::chown(&at, Some(node.0), Some(node.1))?;
        }
        std::os::unix::fs::chown(dir, Some(0), Some(node.1))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o770))
    };
    let collected = args.out.join("outputs");
    super::put::commit(&collected, &job.outputs, root, &job.id, &nonce, &ready)
        .map_err(|why| why.to_string())
}

/// Makes `state`, root's and closed to everyone else, when it is missing.
fn make_private(state: &Path) -> Result<(), Stop> {
    if state.exists() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)
        .map_err(|why| Stop::Setup(format!("{}: {why}", state.display())))
}

/// The pids of `uid`'s live processes.
fn user_processes(uid: u32) -> Result<Vec<i32>, Stop> {
    Ok(sys::processes(false)
        .map_err(|why| Stop::Setup(format!("listing the processes: {why}")))?
        .into_iter()
        .filter(|p| p.uid == uid)
        .map(|p| p.pid)
        .collect())
}

/// `root`, resolved: a folder of root's that only root may write.
fn checked_root(root: &Path) -> Result<PathBuf, Stop> {
    if !root.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(root)
            .map_err(|why| Stop::Setup(format!("{}: {why}", root.display())))?;
    }
    let resolved =
        fs::canonicalize(root).map_err(|why| Stop::Setup(format!("{}: {why}", root.display())))?;
    let meta = fs::metadata(&resolved)
        .map_err(|why| Stop::Setup(format!("{}: {why}", resolved.display())))?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(Stop::Setup(format!(
            "{}: not a folder of root's that only root may write",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// Makes the job folder and its subfolders, the user's and closed to everyone else, and copies
/// the inputs into `work`.
fn make_folder(layout: &Layout, job: &JobFile, uid: u32, gid: u32) -> Result<(), Stop> {
    let setup =
        |what: &Path, why: std::io::Error| Stop::Setup(format!("{}: {why}", what.display()));
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    // A folder that is there already is another run's: it is refused and left alone.
    builder
        .create(&layout.folder)
        .map_err(|why| setup(&layout.folder, why))?;
    let filled = fill_folder(layout, job, uid, gid, &builder);
    if filled.is_err() {
        let _ = files::remove_folder(&layout.folder);
    }
    filled
}

/// The subfolders of a job folder just made, its owner, and its inputs.
fn fill_folder(
    layout: &Layout,
    job: &JobFile,
    uid: u32,
    gid: u32,
    builder: &fs::DirBuilder,
) -> Result<(), Stop> {
    let setup =
        |what: &Path, why: std::io::Error| Stop::Setup(format!("{}: {why}", what.display()));
    for dir in std::iter::once(layout.folder.clone())
        .chain(SUBFOLDERS.iter().map(|name| layout.folder.join(name)))
    {
        if dir != layout.folder {
            builder.create(&dir).map_err(|why| setup(&dir, why))?;
        }
        std::os::unix::fs::chown(&dir, Some(uid), Some(gid)).map_err(|why| setup(&dir, why))?;
    }
    if let Some(inputs) = &job.inputs {
        files::copy_tree(Path::new(inputs), &layout.work(), Some((uid, gid)))
            .map_err(|why| Stop::Setup(format!("copying the inputs from {inputs}: {why}")))?;
    }
    Ok(())
}

/// Starts the job: this program again, in the start mode (`super::start`), with the job's
/// environment, working directory and streams; that process opens the audit session, drops to the
/// user, applies the profile and executes the job's program in its own place. Returns the child,
/// whose pid is the job's program's.
fn start(
    job: &JobFile,
    layout: &Layout,
    user_name: &str,
    user: &sys::User,
    profile: &Profile<'_>,
    new_session: bool,
    args: &Args,
) -> Result<std::process::Child, String> {
    let out = args.out.as_path();
    let stream = |name: &str| {
        File::create(out.join(name)).map_err(|why| format!("{}: {why}", out.join(name).display()))
    };
    let (stdout, stderr) = (stream("stdout")?, stream("stderr")?);
    let asid_file = out.join("asid");
    let _ = fs::remove_file(&asid_file);
    let start = StartFile {
        uid: user.uid,
        gid: user.gid,
        new_session,
        asid_file: asid_file.to_string_lossy().into_owned(),
        profile: profile.text.to_owned(),
        params: std::iter::once((
            JOB_PARAM.to_owned(),
            layout.folder.to_string_lossy().into_owned(),
        ))
        .chain(profile.params.iter().cloned())
        .collect(),
        program: job.program.clone(),
        args: job.args.clone(),
        disclaim: args.spawn == Spawn::Disclaimed,
    };
    let start_path = out.join("start.json");
    let text = serde_json::to_string(&start).map_err(|why| format!("the start file: {why}"))?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&start_path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|why| format!("{}: {why}", start_path.display()))?;
    let me = std::env::current_exe().map_err(|why| format!("this program's path: {why}"))?;
    let mut program = Program::new(
        &me,
        [
            std::ffi::OsStr::new(super::start::VERB),
            std::ffi::OsStr::new("--start"),
            start_path.as_os_str(),
        ],
    );
    // The start process answers for the agent; the job's program, executed in its place, does
    // too unless the start file says to disclaim it (the owner's decision: a job is an ordinary
    // child of the agent, and its profile keeps it on the mount to its own trees).
    program.keep_responsibility();
    program
        .env_clear()
        .envs(layout.env(job, user_name))
        .current_dir(layout.work())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    program
        .spawn()
        .map_err(|why| format!("starting {}: {why}", job.program))
}

/// The audit session the start process wrote, once the job has ended.
pub(crate) fn read_asid(out: &Path) -> Option<i32> {
    fs::read_to_string(out.join("asid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The start mode (`super::start`): runs in a new process of this program, one thread, as root.
/// Does not return when the job's program is executed.
pub(super) fn start_job(file: &Path, err: &mut dyn Write) -> u8 {
    let why = match start_steps(file) {
        Ok(never) => match never {},
        Err(why) => why,
    };
    let _ = writeln!(err, "js-agent: the start of the job failed: {why}");
    super::start::START_FAILED
}

/// The steps of the start; the error of the first that fails.
fn start_steps(file: &Path) -> Result<std::convert::Infallible, String> {
    let text = fs::read_to_string(file).map_err(|why| format!("{}: {why}", file.display()))?;
    let _ = fs::remove_file(file);
    let start = StartFile::parse(&text)?;
    if start.new_session {
        let asid = sys::new_audit_session(start.uid)
            .map_err(|why| format!("a new audit session: {why}"))?;
        fs::write(&start.asid_file, asid.to_string())
            .map_err(|why| format!("{}: {why}", start.asid_file))?;
    }
    if rustix::process::geteuid().is_root() {
        sys::drop_to(start.uid, start.gid)
            .map_err(|why| format!("the drop to uid {}: {why}", start.uid))?;
    }
    sys::apply_profile(&start.profile, &start.params)
        .map_err(|why| format!("the Seatbelt profile was refused: {why}"))?;
    let program = std::ffi::OsStr::new(&start.program);
    let program_args: Vec<std::ffi::OsString> = start.args.iter().map(Into::into).collect();
    let why = if start.disclaim {
        js_mac_session::disclaim::exec(program, &program_args)
    } else {
        js_mac_session::disclaim::exec_kept(program, &program_args)
    };
    Err(format!("executing {}: {why}", start.program))
}

/// Waits for the program until `deadline`; past it, kills the program (the sweep ends the rest).
fn wait(child: &mut std::process::Child, deadline: Instant) -> Ended {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ended(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ended::WallLimit;
            }
        }
    }
}

/// How a program's exit status reads.
fn ended(status: std::process::ExitStatus) -> Ended {
    match (status.code(), status.signal()) {
        (Some(code), _) => Ended::Exited(code),
        (None, Some(signal)) => Ended::Signalled(signal),
        (None, None) => Ended::Signalled(0),
    }
}

/// Ends every process of the job, in rounds, until a round finds none. A process table that
/// cannot be read stops the sweep, not clean, and the cleanup after it goes on.
pub(crate) fn sweep_job(ids: &JobIds) -> Tally {
    let mut tally = Tally::default();
    for _ in 0..ROUNDS_MAX {
        let procs = match sys::processes(ids.asid.is_some()) {
            Ok(procs) => procs,
            Err(why) => {
                tally.failed(format!("listing the processes: {why}"));
                break;
            }
        };
        let found = sweep::targets(&procs, ids);
        if !tally.round(&found) {
            break;
        }
        for target in &found {
            sys::kill(target.pid);
        }
        thread::sleep(SWEEP_PAUSE);
    }
    tally
}

/// The mode of a regular file copied for a job: the owner's bits and the others' read and
/// execute, never set-id.
pub(super) fn copied_mode(mode: u32) -> u32 {
    (mode & 0o755) | 0o600
}

/// Sets `path`'s permission bits.
pub(super) fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}
