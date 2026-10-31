//! The start check of `--runtime crun`: what this host must have before the agent runs a job
//! with `crun`, each missing piece named.
//!
//! The agent is root and runs every `crun` command as a dedicated job user (`--job-user`),
//! so the host needs, besides `crun` itself: `newuidmap` and `newgidmap` (which `crun` runs
//! to map the job's ids), a subordinate uid and gid range for the job user, a state root
//! and a scratch root the job's ids can reach, the cgroup version 2 controllers the leases'
//! limits are made of, and, where the kernel restricts unprivileged user namespaces, the
//! distribution's AppArmor profile for `crun` (it attaches by the path `/usr/bin/crun`, so a
//! copy of the program is refused).
//!
//! [`check`] reads the host through a [`CrunHost`] (the paths of the files it reads, so a
//! test can stand others in) and runs `crun --version` through the agent's
//! [`CommandRunner`]. It never stops at the first problem: the [`Report`] holds every
//! missing piece, and [`Report::refusal`] is the text the agent exits 2 with.

use std::fmt::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use js_contain::subids::{IdFiles, MIN_SUBORDINATE_IDS, check_subordinate_ids};

use crate::Cli;
use crate::cli::RuntimeKind;
use crate::facts::CommandRunner;
use crate::lifecycle::DirLayout;

/// The distribution's `crun`, the default of `--crun-bin`: the path its AppArmor profile
/// attaches to.
pub const DISTRO_CRUN: &str = "/usr/bin/crun";

/// The oldest `crun` the agent runs (major, minor): the one whose failures js-contain reads.
pub const MIN_CRUN: (u64, u64) = (1, 14);

/// The cgroup controllers a lease's limits are made of, which the agent's own cgroup must
/// hold (a unit with `Delegate=yes` gets them).
pub const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];

/// The program that maps a user namespace's uids and gids, and the one for gids.
pub const ID_TOOLS: [&str; 2] = ["newuidmap", "newgidmap"];

/// One part of the host a crun job needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    /// The `crun` program.
    Crun,
    /// The version `crun` reports.
    CrunVersion,
    /// `newuidmap`.
    NewUidMap,
    /// `newgidmap`.
    NewGidMap,
    /// The job user's account.
    JobUser,
    /// The job user's range in `/etc/subuid`.
    SubUid,
    /// The job user's range in `/etc/subgid`.
    SubGid,
    /// The state root.
    StateRoot,
    /// The scratch root.
    ScratchRoot,
    /// A directory above the scratch root that the job's ids cannot search.
    SearchableAncestor,
    /// The cgroup version 2 hierarchy and the agent's delegated cgroup.
    CgroupDelegation,
    /// The AppArmor profile that lets `crun` make a user namespace.
    AppArmorProfile,
}

impl Piece {
    /// The name the refusal gives the piece.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Crun => "crun",
            Self::CrunVersion => "crun version",
            Self::NewUidMap => "newuidmap",
            Self::NewGidMap => "newgidmap",
            Self::JobUser => "job user",
            Self::SubUid => "subuid range",
            Self::SubGid => "subgid range",
            Self::StateRoot => "state root",
            Self::ScratchRoot => "scratch root",
            Self::SearchableAncestor => "path permissions",
            Self::CgroupDelegation => "cgroup v2 delegation",
            Self::AppArmorProfile => "apparmor profile",
        }
    }
}

/// A piece the host lacks, what is wrong with it and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// Which piece.
    pub piece: Piece,
    /// What was found.
    pub what: String,
    /// What the operator does about it.
    pub fix: String,
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} (fix: {})",
            self.piece.name(),
            self.what,
            self.fix
        )
    }
}

/// What [`check`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Every missing piece, in the order the host is checked.
    pub missing: Vec<Missing>,
    /// What the operator should read, though the host may run jobs.
    pub warnings: Vec<String>,
    /// The job user's `(uid, gid)`, when the account was found.
    pub job_ids: Option<(u32, u32)>,
}

impl Report {
    fn miss(&mut self, piece: Piece, what: &str, fix: &str) {
        self.missing.push(Missing {
            piece,
            what: what.to_owned(),
            fix: fix.to_owned(),
        });
    }

    /// The text the agent exits 2 with, naming each missing piece, or `None` when the host
    /// has everything.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        if self.missing.is_empty() {
            return None;
        }
        let mut text = format!(
            "--runtime crun: this host is not ready ({} missing):",
            self.missing.len()
        );
        for piece in &self.missing {
            let _ = write!(text, "\n  - {piece}");
        }
        Some(text)
    }
}

/// Where the check reads the host: the paths of the files, so a test can name others.
#[derive(Debug, Clone)]
pub struct CrunHost {
    /// The user database and the subordinate id files.
    pub ids: IdFiles,
    /// The kernel's switch for unprivileged user namespaces under AppArmor.
    pub apparmor_userns: PathBuf,
    /// The AppArmor profiles that are loaded.
    pub apparmor_profiles: PathBuf,
    /// The cgroup version 2 mount.
    pub cgroup_mount: PathBuf,
    /// The agent's own cgroup membership (`/proc/self/cgroup`).
    pub self_cgroup: PathBuf,
    /// The directories `newuidmap` and `newgidmap` are looked for in.
    pub id_tool_dirs: Vec<PathBuf>,
    /// The uid that must own the state root and the scratch root: root on a machine (a test
    /// names the uid it runs as).
    pub owner_uid: u32,
}

impl CrunHost {
    /// This machine's own files.
    #[must_use]
    pub fn system() -> Self {
        Self {
            ids: IdFiles::system(),
            apparmor_userns: "/proc/sys/kernel/apparmor_restrict_unprivileged_userns".into(),
            apparmor_profiles: "/sys/kernel/security/apparmor/profiles".into(),
            cgroup_mount: "/sys/fs/cgroup".into(),
            self_cgroup: "/proc/self/cgroup".into(),
            id_tool_dirs: ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
                .map(PathBuf::from)
                .into(),
            owner_uid: 0,
        }
    }
}

/// What the flags name.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// `--crun-bin`.
    pub crun: &'a Path,
    /// `--job-user`.
    pub job_user: &'a str,
    /// `--state-root`.
    pub state_root: &'a Path,
    /// `--scratch-root`.
    pub scratch_root: &'a Path,
}

/// Checks the host for `request`, naming every piece that is missing.
#[must_use]
pub fn check(request: &Request<'_>, host: &CrunHost, commands: &dyn CommandRunner) -> Report {
    let mut report = Report::default();
    // The programs run as the job user, so what that user can reach decides them.
    let ids = std::fs::read_to_string(&host.ids.passwd)
        .ok()
        .and_then(|text| passwd_ids(&text, request.job_user));
    crun(request.crun, ids, commands, &mut report);
    id_tools(host, ids, &mut report);
    job_user(request.job_user, host, &mut report);
    state_root(request.state_root, host.owner_uid, &mut report);
    scratch_root(request.scratch_root, host.owner_uid, ids, &mut report);
    cgroups(host, &mut report);
    apparmor(request.crun, host, &mut report);
    report
}

/// Whether the user `ids` (uid, gid; `None` when the account is unknown, so only the bits for
/// others count) holds `bit` (4, 2 or 1) on a file of owner `uid`, group `gid` and `mode`:
/// the owner's bits when the user owns it, else the group's when the user's group is the
/// file's, else the others'. Only the user's primary gid is compared, not its supplementary
/// groups, so a directory the user reaches only through a supplementary group is reported as
/// not searchable: a false refusal, which is safe (the job user has no other groups).
fn holds(meta: &std::fs::Metadata, ids: Option<(u32, u32)>, bit: u32) -> bool {
    let (uid, gid) = ids.unwrap_or((u32::MAX, u32::MAX));
    let mode = meta.mode();
    let class = if meta.uid() == uid {
        mode >> 6
    } else if meta.gid() == gid {
        mode >> 3
    } else {
        mode
    };
    class & bit != 0
}

/// The first directory above `path` (its resolved form) that the user cannot search.
fn unsearchable_ancestor(path: &Path, ids: Option<(u32, u32)>) -> Option<PathBuf> {
    // The path was just looked at; one that cannot be resolved now is searched as given.
    let canonical = std::fs::canonicalize(path).unwrap_or(path.to_owned());
    canonical
        .ancestors()
        .skip(1)
        // A directory that cannot be looked at cannot be searched either.
        .find(|ancestor| !std::fs::metadata(ancestor).is_ok_and(|meta| holds(&meta, ids, 1)))
        .map(Path::to_owned)
}

fn crun(path: &Path, ids: Option<(u32, u32)>, commands: &dyn CommandRunner, report: &mut Report) {
    let at = path.display();
    let fix = format!(
        "install crun {}.{} or newer (package `crun`)",
        MIN_CRUN.0, MIN_CRUN.1
    );
    // The profile attaches to the resolved path, so a name that resolves to the distribution's
    // file (`/bin/crun` on a merged-usr host) is the distribution's.
    let resolved = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    if resolved(path) != resolved(Path::new(DISTRO_CRUN)) {
        report.warnings.push(format!(
            "--crun-bin {at} is not the distribution's {DISTRO_CRUN}: the distribution's AppArmor \
             profile attaches to that path, so where the kernel restricts unprivileged user \
             namespaces every job fails to start; use {DISTRO_CRUN}"
        ));
    }
    match std::fs::metadata(path) {
        Err(why) => {
            report.miss(Piece::Crun, &format!("{at}: {why}"), &fix);
            return;
        }
        Ok(meta) if !meta.is_file() => {
            report.miss(Piece::Crun, &format!("{at} is not a regular file"), &fix);
            return;
        }
        Ok(meta) if !holds(&meta, ids, 1) => {
            report.miss(
                Piece::Crun,
                &format!("{at} is not executable by the job user (the agent runs it as that user)"),
                &format!("chmod o+x {at}"),
            );
            return;
        }
        Ok(_) => {}
    }
    if let Some(dir) = unsearchable_ancestor(path, ids) {
        report.miss(
            Piece::Crun,
            &format!(
                "{} is not searchable by the job user, so it cannot run {at}",
                dir.display()
            ),
            &format!("chmod o+x {}", dir.display()),
        );
        return;
    }
    let text = match commands.run(&at.to_string(), &["--version"]) {
        Ok(text) => text,
        Err(why) => {
            report.miss(
                Piece::CrunVersion,
                &format!("`{at} --version` failed: {why}"),
                &fix,
            );
            return;
        }
    };
    let first = text.lines().next().unwrap_or_default();
    match version(first) {
        None => report.miss(
            Piece::CrunVersion,
            &format!("`{at} --version` printed {first:?}, which is not a version"),
            &fix,
        ),
        Some(found) if found < MIN_CRUN => report.miss(
            Piece::CrunVersion,
            &format!(
                "crun {}.{} is older than {}.{}",
                found.0, found.1, MIN_CRUN.0, MIN_CRUN.1
            ),
            &fix,
        ),
        Some(_) => {}
    }
}

/// The major and minor number of `crun version 1.14.1`.
fn version(line: &str) -> Option<(u64, u64)> {
    let number = line.split_once("version")?.1.trim();
    let mut parts = number
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u64>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().and_then(Result::ok).unwrap_or(0);
    Some((major, minor))
}

fn id_tools(host: &CrunHost, ids: Option<(u32, u32)>, report: &mut Report) {
    for tool in ID_TOOLS {
        let piece = if tool == "newuidmap" {
            Piece::NewUidMap
        } else {
            Piece::NewGidMap
        };
        let found = host
            .id_tool_dirs
            .iter()
            .map(|dir| dir.join(tool))
            .find_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                meta.is_file().then_some((path, meta))
            });
        let fix = "install the package `uidmap`";
        let Some((found, meta)) = found else {
            report.miss(piece, "not found in the system program directories", fix);
            continue;
        };
        let mode = meta.permissions().mode();
        if !holds(&meta, ids, 1) {
            report.miss(
                piece,
                &format!("{} is not executable by the job user", found.display()),
                fix,
            );
        } else if let Some(dir) = unsearchable_ancestor(&found, ids) {
            report.miss(
                piece,
                &format!("{} is not searchable by the job user", dir.display()),
                &format!("chmod o+x {}", dir.display()),
            );
        } else if !((mode & 0o4000 != 0) | has_file_capabilities(&found)) {
            report.miss(
                piece,
                &format!(
                    "{} is neither setuid nor holds file capabilities, so it cannot write a user namespace's id maps",
                    found.display()
                ),
                &format!("chmod u+s {}, or reinstall the package `uidmap`", found.display()));
        }
    }
}

/// Whether `path` carries file capabilities (`security.capability`), which some
/// distributions give the id tools in place of the setuid bit.
fn has_file_capabilities(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    // A path with a NUL cannot name a file: it holds no attribute.
    std::ffi::CString::new(path.as_os_str().as_bytes()).is_ok_and(|path| {
        // SAFETY: `path` and the attribute name are NUL-terminated strings that outlive
        // the call; a null buffer of length 0 asks only for the attribute's size, so
        // nothing is written.
        let size = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"security.capability".as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        size >= 0
    })
}

/// The `(uid, gid)` of `name` in passwd-format `text`.
fn passwd_ids(text: &str, name: &str) -> Option<(u32, u32)> {
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let [user, _, uid, gid, ..] = fields[..] else {
            return None;
        };
        (user == name).then_some(())?;
        Some((uid.parse().ok()?, gid.parse().ok()?))
    })
}

fn job_user(name: &str, host: &CrunHost, report: &mut Report) {
    let fix = format!(
        "create the account (`useradd --system --no-create-home --shell /usr/sbin/nologin {name}`)"
    );
    let passwd = match std::fs::read_to_string(&host.ids.passwd) {
        Ok(text) => text,
        Err(why) => {
            report.miss(
                Piece::JobUser,
                &format!("{}: {why}", host.ids.passwd.display()),
                &fix,
            );
            return;
        }
    };
    let Some((uid, gid)) = passwd_ids(&passwd, name) else {
        report.miss(
            Piece::JobUser,
            &format!("no user {name:?} in {}", host.ids.passwd.display()),
            &fix,
        );
        return;
    };
    if uid == 0 || gid == 0 {
        report.miss(
            Piece::JobUser,
            &format!("{name:?} has uid {uid} and gid {gid}: a job never runs as root"),
            "name a user with a uid and a gid above 0",
        );
        return;
    }
    report.job_ids = Some((uid, gid));
    // `check_subordinate_ids` reads both files and stops at the first; given one file
    // twice it answers for that file alone, so each range is named by itself.
    for (piece, file) in [
        (Piece::SubUid, &host.ids.subuid),
        (Piece::SubGid, &host.ids.subgid),
    ] {
        let one = IdFiles {
            passwd: host.ids.passwd.clone(),
            subuid: file.clone(),
            subgid: file.clone(),
        };
        if let Err(why) = check_subordinate_ids(&one, uid) {
            report.miss(
                piece,
                &why.to_string(),
                &format!(
                    "add {name}:<first id>:{MIN_SUBORDINATE_IDS} to {} (`usermod --add-subuids` and `--add-subgids`)",
                    file.display()
                ));
        }
    }
}

fn state_root(path: &Path, owner: u32, report: &mut Report) {
    let at = path.display();
    let fix = format!(
        "mkdir {at}; chown root:root {at}; chmod 0700 {at}: the taint marker and the host credential are the agent's alone"
    );
    match std::fs::symlink_metadata(path) {
        Err(why) => report.miss(Piece::StateRoot, &format!("{at}: {why}"), &fix),
        Ok(meta) if !meta.is_dir() => {
            report.miss(
                Piece::StateRoot,
                &format!("{at} is not a directory (a link is not accepted)"),
                &fix,
            );
        }
        Ok(meta) if !DirLayout::Private.allows(meta.mode()) => report.miss(
            Piece::StateRoot,
            &format!(
                "{at} has mode {:04o}; {}",
                meta.mode() & 0o7777,
                DirLayout::Private.refusal()
            ),
            &fix,
        ),
        Ok(meta) if meta.uid() != owner => report.miss(
            Piece::StateRoot,
            &format!(
                "{at} is owned by uid {}, not root (uid {owner})",
                meta.uid()
            ),
            &fix,
        ),
        Ok(_) => {}
    }
}

fn scratch_root(path: &Path, owner: u32, ids: Option<(u32, u32)>, report: &mut Report) {
    let at = path.display();
    match std::fs::symlink_metadata(path) {
        Err(why) => {
            report.miss(
                Piece::ScratchRoot,
                &format!("{at}: {why}"),
                &format!("mkdir {at}; chmod 0711 {at}"),
            );
            return;
        }
        Ok(meta) if !meta.is_dir() => {
            report.miss(
                Piece::ScratchRoot,
                &format!("{at} is not a directory (a link is not accepted)"),
                &format!("make {at} a directory of mode 0711"),
            );
            return;
        }
        Ok(meta) if !DirLayout::Searchable.allows(meta.mode()) => report.miss(
            Piece::ScratchRoot,
            &format!(
                "{at} has mode {:04o}; {}",
                meta.mode() & 0o7777,
                DirLayout::Searchable.refusal()
            ),
            &format!("chmod 0711 {at}"),
        ),
        Ok(meta) if meta.uid() != owner => report.miss(
            Piece::ScratchRoot,
            &format!(
                "{at} is owned by uid {}, not root (uid {owner})",
                meta.uid()
            ),
            &format!("chown root:root {at}"),
        ),
        Ok(_) => {}
    }
    for ancestor in std::fs::canonicalize(path)
        .unwrap_or(path.to_owned())
        .ancestors()
        .skip(1)
    {
        let searchable = std::fs::metadata(ancestor).is_ok_and(|meta| holds(&meta, ids, 1));
        if !searchable {
            report.miss(
                Piece::SearchableAncestor,
                &format!(
                    "{} is not searchable by the job user: the job's ids could not reach the scratch root below it",
                    ancestor.display()
                ),
                &format!("chmod o+x {} (for the unit's state directory: StateDirectoryMode=0711)", ancestor.display()));
        }
    }
}

/// The agent's own cgroup must offer the controllers a lease's limits are made of.
///
/// This is the check that can be made before the runtime exists, and it is not a proof that
/// `cgroup.subtree_control` can be enabled: the runtime (`crun/mod.rs`) does not move the
/// agent into a leaf cgroup and does not enable `subtree_control` on the agent's own cgroup
/// (it writes `+cpu +memory +pids` only in the new `<actions>/helpers` and in each lease's
/// cgroup, which needs the controllers in `<actions>`'s own `subtree_control`). A cgroup that
/// holds the agent cannot enable controllers for children (the no-internal-process rule), so
/// the cgroup `<actions>` the wiring names must be one the operator or the unit has set up;
/// that is the wiring's to check, where `<actions>` is known.
///
/// The packaged unit (`packaging/js-agent/js-agent-crun.service`) sets `DelegateSubgroup=supervisor`,
/// so the agent starts in `<unit>/supervisor` and the unit's own cgroup holds no process: it is
/// the cgroup `<actions>` and its `subtree_control` belong to (`<unit>/actions`). This check
/// reads the leaf's `cgroup.controllers`, which holds what the unit enabled for its children.
fn cgroups(host: &CrunHost, report: &mut Report) {
    let fix = "run the agent from a systemd unit with Delegate=yes on a host with cgroup version 2 (unified hierarchy)";
    let own = match std::fs::read_to_string(&host.self_cgroup) {
        Ok(text) => text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(str::to_owned),
        Err(why) => {
            report.miss(
                Piece::CgroupDelegation,
                &format!("{}: {why}", host.self_cgroup.display()),
                fix,
            );
            return;
        }
    };
    let Some(own) = own else {
        report.miss(
            Piece::CgroupDelegation,
            "the agent is not in a cgroup version 2 hierarchy (no `0::` line)",
            fix,
        );
        return;
    };
    let dir = host.cgroup_mount.join(own.trim_start_matches('/'));
    let file = dir.join("cgroup.controllers");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(why) => {
            report.miss(
                Piece::CgroupDelegation,
                &format!("{}: {why}", file.display()),
                fix,
            );
            return;
        }
    };
    let held: Vec<&str> = text.split_whitespace().collect();
    let lacking: Vec<&str> = CONTROLLERS
        .into_iter()
        .filter(|controller| !held.contains(controller))
        .collect();
    if !lacking.is_empty() {
        report.miss(
            Piece::CgroupDelegation,
            &format!(
                "the agent's cgroup {own} holds no {} controller (it holds: {})",
                lacking.join(", "),
                held.join(" ")
            ),
            fix,
        );
    }
}

fn apparmor(crun: &Path, host: &CrunHost, report: &mut Report) {
    // No switch, no restriction: a kernel without the AppArmor userns rule needs no profile.
    let Ok(switch) = std::fs::read_to_string(&host.apparmor_userns) else {
        return;
    };
    if switch.trim() != "1" {
        return;
    }
    let fix = "install the package `apparmor`, which ships the profile for crun on Ubuntu 24.04 (/etc/apparmor.d/crun; other distributions: their apparmor profiles), and load it: apparmor_parser -r /etc/apparmor.d/crun";
    let profiles = match std::fs::read_to_string(&host.apparmor_profiles) {
        Ok(text) => text,
        Err(why) => {
            report.miss(
                Piece::AppArmorProfile,
                &format!(
                    "unprivileged user namespaces are restricted (apparmor_restrict_unprivileged_userns=1) and {} cannot be read: {why}",
                    host.apparmor_profiles.display()
                ),
                fix);
            return;
        }
    };
    // A profile that names the program's path covers any path; the distribution's `crun`
    // profile (attached to /usr/bin/crun) covers that path only.
    // The kernel attaches by the resolved path, so a link is looked through.
    let resolved = std::fs::canonicalize(crun).unwrap_or(crun.to_owned());
    let named = |name: &str| profiles.lines().any(|line| profile_name(line) == name);
    let loaded = named(&resolved.display().to_string())
        || (resolved == Path::new(DISTRO_CRUN) && named("crun"));
    if !loaded {
        report.miss(
            Piece::AppArmorProfile,
            &format!(
                "unprivileged user namespaces are restricted (apparmor_restrict_unprivileged_userns=1) and no loaded AppArmor profile covers {} (resolved from {}): crun could not make the job's user namespace",
                resolved.display(),
                crun.display()
            ),
            fix);
    }
}

/// The profile name of a line of `/sys/kernel/security/apparmor/profiles`, `name (mode)`.
fn profile_name(line: &str) -> &str {
    line.rsplit_once(" (").map_or(line, |(name, _)| name).trim()
}

/// A host that has every piece: what the runtime is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// `--crun-bin`, or the distribution's `crun`.
    pub crun: PathBuf,
    /// `--job-user`.
    pub job_user: String,
    /// The job user's `(uid, gid)`.
    pub job_ids: (u32, u32),
    /// What the operator should read, though the host may run jobs.
    pub warnings: Vec<String>,
}

/// What `--runtime crun` asks of the other flags, and then of the host.
///
/// # Errors
///
/// The text for exit status 2: a flag that is wrong or missing, or [`Report::refusal`].
pub fn start(cli: &Cli, host: &CrunHost, commands: &dyn CommandRunner) -> Result<Ready, String> {
    let request = flags(cli)?;
    let report = check(&request, host, commands);
    if let Some(text) = report.refusal() {
        return Err(text);
    }
    // A job user the check did not find is a refusal above; the ids are never 0 here.
    let job_ids = report.job_ids.unwrap_or_default();
    Ok(Ready {
        crun: request.crun.to_owned(),
        job_user: request.job_user.to_owned(),
        job_ids,
        warnings: report.warnings,
    })
}

/// The flags of `--runtime`, `--crun-bin` and `--job-user` checked against each other.
///
/// # Errors
///
/// The text names the flag at fault.
pub fn flags(cli: &Cli) -> Result<Request<'_>, String> {
    if cli.runtime != Some(RuntimeKind::Crun) {
        return Err("--runtime crun: not given".to_owned());
    }
    let engine_flags = [
        ("--container-bin", cli.container_bin.is_some()),
        ("--container-network", cli.container_network.is_some()),
        ("--container-user", cli.container_user.is_some()),
        ("--container-subuid", cli.container_subuid.is_some()),
        ("--container-host", cli.container_host.is_some()),
        ("--container-pull", cli.container_pull.is_some()),
        ("--container-tmp-size", cli.container_tmp_size.is_some()),
        ("--container-add-host", !cli.container_add_host.is_empty()),
        (
            "--container-egress-allow",
            !cli.container_egress_allow.is_empty(),
        ),
        ("--container-nft", cli.container_nft.is_some()),
        ("--actions-cgroup", cli.actions_cgroup.is_some()),
        ("--gpu-toolkit-hook", cli.gpu_toolkit_hook.is_some()),
        // A job's mount is bound from a directory the job's writer may swap for a link
        // between the agent's check and the bind; `CrunRuntime` does not read the binds
        // back yet, so no mount root is accepted.
        ("--mount-root", !cli.mount_roots.is_empty()),
        ("--mount-root-ro", !cli.mount_roots_ro.is_empty()),
    ];
    if let Some((flag, _)) = engine_flags.into_iter().find(|(_, given)| *given) {
        return Err(format!(
            "{flag}: not accepted with --runtime crun (a flag of the container engine, or a mount root, which the crun runtime does not read back after it binds)"
        ));
    }
    let crun = cli.crun_bin.as_deref().unwrap_or(Path::new(DISTRO_CRUN));
    if !crun.is_absolute() {
        return Err(format!(
            "--crun-bin: {} is not an absolute path",
            crun.display()
        ));
    }
    let job_user = cli.job_user.as_deref().ok_or(
        "--job-user: required with --runtime crun (the agent runs every crun as that user)",
    )?;
    let state_root = cli
        .state_root
        .as_deref()
        .ok_or("--state-root: required with --runtime crun: the taint marker is kept there")?;
    let scratch_root = cli.scratch_root.as_deref().ok_or(
        "--scratch-root: required with --runtime crun: each run's directory is made there",
    )?;
    Ok(Request {
        crun,
        job_user,
        state_root,
        scratch_root,
    })
}
