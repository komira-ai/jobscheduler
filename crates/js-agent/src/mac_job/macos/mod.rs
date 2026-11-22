//! `__mac-job` on macOS: the set-up, the start, the sweep and the check (see [`super`]).

mod files;
mod sys;
mod trees;

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read as _, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
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
use super::put;
use super::report::{self, AccountUse, Ended, Leftovers, Report};
use super::spec::JobFile;
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
            Pool::open(state, slots, args.ready)
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
        Ok(report) => Ok(Report {
            account: Some(account),
            ..report
        }),
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

    let (mount_text, mount_params) = match trees::rules(args, &mounted) {
        Ok(rules) => rules,
        Err(why) => {
            let _ = trees::release(args, &job, mounted);
            let _ = files::remove_folder(&layout.folder);
            return Err(Stop::Setup(why));
        }
    };
    let profile_text = profile::render(args.profile, &job.network, &mount_text, added);
    let new_session = args.audit_session == AuditSession::New;
    let started = Instant::now();
    let profile_of = Profile {
        text: &profile_text,
        params: &mount_params,
    };
    let started_job = start(&job, &layout, name, &user, &profile_of, new_session, args);
    let (mut child, asid) = match started_job {
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

    let ids = JobIds {
        uid: user.uid,
        asid,
        own_pid: std::process::id().cast_signed(),
    };
    let sweep = sweep_job(&ids);
    let trees = trees::release(args, &job, mounted);
    let put = args
        .put_ingest
        .as_ref()
        .map(|root| commit_outputs(args, root, &job, &layout));
    let outputs = files::collect(&layout.work(), &job.outputs, &args.out.join("outputs"));
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
        put,
        trees,
        account: None,
        spawn: format!("{:?}", args.spawn).to_lowercase(),
        stdout_tail: report::tail(&fs::read(args.out.join("stdout")).unwrap_or_default()),
        stderr_tail: report::tail(&fs::read(args.out.join("stderr")).unwrap_or_default()),
    })
}

/// Commits the job's outputs to the put ingest at `root`, its directories given to the node's
/// user as the node needs them: the key's folders its, the job's put directory root's with the
/// node's group able to write it (the node removes the file and the directory).
fn commit_outputs(
    args: &Args,
    root: &Path,
    job: &JobFile,
    layout: &Layout,
) -> Result<put::Committed, String> {
    let ids = args
        .put_user
        .split_once(':')
        .map(|(uid, gid)| (uid.parse::<u32>(), gid.parse::<u32>()));
    let node = match ids {
        Some((Ok(uid), Ok(gid))) => (uid, gid),
        Some(_) => return Err(format!("--put-user {}: a name or uid:gid", args.put_user)),
        None => {
            let user =
                sys::user(&args.put_user).map_err(|why| format!("{}: {why}", args.put_user))?;
            (user.uid, user.gid)
        }
    };
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
        fs::set_permissions(dir, fs::Permissions::from_mode(0o770))
    };
    put::commit(&layout.work(), &job.outputs, root, &job.id, &nonce, &ready)
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

/// Starts the program: the child opens the audit session, drops to the user and applies the
/// profile. Returns the child and the session id it reported.
fn start(
    job: &JobFile,
    layout: &Layout,
    user_name: &str,
    user: &sys::User,
    profile: &Profile<'_>,
    new_session: bool,
    args: &Args,
) -> Result<(std::process::Child, Option<i32>), String> {
    let out = args.out.as_path();
    let threads =
        sys::own_threads().map_err(|why| format!("counting this process's threads: {why}"))?;
    if threads != 1 {
        return Err(format!(
            "this process has {threads} threads; the start allocates after fork and needs one"
        ));
    }
    let stream = |name: &str| {
        File::create(out.join(name)).map_err(|why| format!("{}: {why}", out.join(name).display()))
    };
    let (stdout, stderr) = (stream("stdout")?, stream("stderr")?);
    let (asid_read, asid_write) = sys::pipe().map_err(|why| format!("a pipe: {why}"))?;
    let cstring =
        |what: &str, text: &str| CString::new(text).map_err(|_| format!("the {what} holds a NUL"));
    let steps = sys::ChildSteps {
        new_session,
        asid_out: asid_write.as_raw_fd(),
        uid: user.uid,
        gid: user.gid,
        profile: cstring("profile", profile.text)?,
        params: std::iter::once((
            JOB_PARAM.to_owned(),
            layout.folder.to_string_lossy().into_owned(),
        ))
        .chain(profile.params.iter().cloned())
        .map(|(name, value)| Ok((cstring("parameter", &name)?, cstring("parameter", &value)?)))
        .collect::<Result<Vec<_>, String>>()?,
    };
    let run_steps = move || {
        // SAFETY: `Program` runs this in the child of a fork of this process, which has one
        // thread (checked above), so the allocation inside `sandbox_init_with_parameters` cannot
        // meet a lock another thread held (`ChildSteps::run`).
        unsafe { steps.run() }
    };
    // SAFETY: as above: the steps allocate only because no other thread exists to hold a lock.
    let mut program = unsafe { Program::with_steps(&job.program, &job.args, run_steps) };
    if args.spawn == Spawn::Ordinary {
        // The owner's decision (the module documentation of `super`): the job holds the agent's
        // grants, and its profile keeps it on the mount to its own trees.
        program.keep_responsibility();
    }
    program
        .env_clear()
        .envs(layout.env(job, user_name))
        .current_dir(layout.work())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    let child = program
        .spawn()
        .map_err(|why| format!("starting {}: {why}", job.program))?;
    drop(asid_write);
    let asid = if new_session {
        let mut bytes = [0u8; 4];
        File::from(asid_read)
            .read_exact(&mut bytes)
            .ok()
            .map(|()| i32::from_ne_bytes(bytes))
    } else {
        None
    };
    Ok((child, asid))
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
fn sweep_job(ids: &JobIds) -> Tally {
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
        for (pid, _) in found {
            sys::kill(pid);
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
