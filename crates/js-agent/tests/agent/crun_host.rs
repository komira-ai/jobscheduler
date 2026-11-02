//! The start check of `--runtime crun`: each piece of the host a crun job needs is named when it
//! is missing, all of them in one refusal, and a host that has every piece passes. The host is a
//! directory of stand-in files (`CrunHost` names the paths the check reads) and a scripted
//! `crun --version`; every case starts from a host that passes and breaks one piece, so a case
//! that fails names the check that no longer catches its defect. (Linux only: only Linux runs
//! containers.)
//!
//! Every directory is below `/tmp`, whose ancestors are searchable by others on any host the
//! tests run on.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use js_agent::Cli;
use js_agent::crun_host::{CrunHost, Piece, Report, Request, check, flags, ready, start};
use js_contain::subids::IdFiles;
use js_testkit::dir::TestDir;

use crate::support::{FakeCommands, code, linux_host, mac_host, run_scripted, welcome};

const USER: &str = "jsjob";

/// A host that has every piece, in files below a directory of the test.
pub(crate) struct Fixture {
    pub(crate) dir: TestDir,
    pub(crate) host: CrunHost,
    pub(crate) crun: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) scratch: PathBuf,
    pub(crate) commands: FakeCommands,
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

impl Fixture {
    pub(crate) const USER: &'static str = USER;

    fn new(name: &str) -> Self {
        Self::with_ids(name, 990, 991)
    }

    /// A host whose job user has the ids `uid` and `gid`.
    pub(crate) fn with_ids(name: &str, uid: u32, gid: u32) -> Self {
        let dir = TestDir::under("/tmp", name);
        set_mode(dir.path(), 0o755);
        let root = dir.path().to_owned();
        let ids = IdFiles::in_dir(&root.join("etc"));
        write(
            &ids.passwd,
            &format!("root:x:0:0::/root:/bin/sh\n{USER}:x:{uid}:{gid}::/:/usr/sbin/nologin\n"),
        );
        write(&ids.subuid, &format!("{USER}:100000:65536\n"));
        write(&ids.subgid, &format!("{USER}:100000:65536\n"));
        let tools = root.join("bin");
        for tool in ["newuidmap", "newgidmap", "crun"] {
            write(&tools.join(tool), "#!/bin/sh\n");
            set_mode(
                &tools.join(tool),
                if tool == "crun" { 0o755 } else { 0o4755 },
            );
        }
        let cgroup = root.join("cgroup");
        write(
            &cgroup.join("system.slice/js-agent.service/cgroup.controllers"),
            "cpuset cpu io memory pids\n",
        );
        write(
            &root.join("self_cgroup"),
            "0::/system.slice/js-agent.service\n",
        );
        write(&root.join("restrict"), "1\n");
        let (state, scratch) = (root.join("state"), root.join("scratch"));
        fs::create_dir(&state).unwrap();
        set_mode(&state, 0o700);
        fs::create_dir(&scratch).unwrap();
        set_mode(&scratch, 0o711);
        let crun = tools.join("crun");
        // The profile that covers this crun (a path other than the distribution's).
        write(
            &root.join("profiles"),
            &format!("snap.x (enforce)\n{} (unconfined)\n", crun.display()),
        );
        let commands = FakeCommands::default().with(
            &format!("{} --version", crun.display()),
            Ok("crun version 1.14.1\ncommit: abc\n"),
        );
        let host = CrunHost {
            ids,
            apparmor_userns: root.join("restrict"),
            apparmor_profiles: root.join("profiles"),
            cgroup_mount: cgroup,
            self_cgroup: root.join("self_cgroup"),
            id_tool_dirs: vec![tools],
            // The tests are not root: the directories they make are owned by this uid.
            owner_uid: fs::metadata(&root).unwrap().uid(),
        };
        Self {
            dir,
            host,
            crun,
            state,
            scratch,
            commands,
        }
    }

    fn check(&self) -> Report {
        check(&self.request(&self.crun), &self.host, &self.commands)
    }

    fn request<'a>(&'a self, crun: &'a Path) -> Request<'a> {
        Request {
            crun,
            job_user: USER,
            state_root: &self.state,
            scratch_root: &self.scratch,
        }
    }

    /// The scripted commands with `crun --version` also answered for `crun`.
    fn commands_for(&self, crun: &Path) -> FakeCommands {
        FakeCommands::default().with(
            &format!("{} --version", crun.display()),
            Ok("crun version 1.14.1\ncommit: abc\n"),
        )
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// The pieces a report names, in order.
fn pieces(report: &Report) -> Vec<Piece> {
    report.missing.iter().map(|m| m.piece).collect()
}

/// BEFORE any case breaks the host: it passes. A check that refused a complete host (a
/// version read wrong, a mode taken for another) would stop every agent that is set up right;
/// the report also carries the job user's ids for the runtime to use.
#[test]
fn a_host_with_every_piece_passes() {
    let fx = Fixture::new("crun-host-good");
    let report = fx.check();
    assert_eq!(report.missing, vec![], "{:?}", report.refusal());
    assert_eq!(report.refusal(), None);
    assert_eq!(report.job_ids, Some((990, 991)));
    // The program is not the distribution's path, which is said, loudly.
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("not the distribution's /usr/bin/crun"))
    );
}

/// `--crun-bin` at the distribution's path draws no warning (whether or not crun is installed on
/// the machine that runs the test); any other path does, and says why it matters.
#[test]
fn only_a_crun_that_is_not_the_distributions_draws_a_warning() {
    let fx = Fixture::new("crun-host-warn");
    let distro = check(
        &fx.request(Path::new("/usr/bin/crun")),
        &fx.host,
        &fx.commands,
    );
    assert!(
        !distro.warnings.iter().any(|w| w.contains("distribution")),
        "{:?}",
        distro.warnings
    );
    let other = fx.check();
    assert!(
        other
            .warnings
            .iter()
            .any(|w| w.contains("AppArmor") && w.contains("/usr/bin/crun"))
    );
}

/// A name that resolves to the distribution's file draws no warning: the AppArmor profile
/// attaches to the resolved path. Where the machine has no `/usr/bin/crun` there is nothing to
/// resolve to, and the case is skipped (the CI job that runs this has the distribution's crun).
#[test]
fn a_link_to_the_distributions_crun_draws_no_warning() {
    if !Path::new("/usr/bin/crun").exists() {
        return;
    }
    let fx = Fixture::new("crun-host-link");
    let link = fx.path("crun-link");
    symlink("/usr/bin/crun", &link).unwrap();
    let report = check(&fx.request(&link), &fx.host, &fx.commands);
    assert!(
        !report.warnings.iter().any(|w| w.contains("distribution")),
        "{:?}",
        report.warnings
    );
}

/// No crun at the path, a directory there, or a file that cannot be run: each names `crun`.
#[test]
fn a_missing_or_unrunnable_crun_is_named() {
    let fx = Fixture::new("crun-host-crun");
    set_mode(&fx.crun, 0o644);
    assert_eq!(pieces(&fx.check()), vec![Piece::Crun]);
    assert!(fx.check().missing[0].what.contains("not executable"));
    fs::remove_file(&fx.crun).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::Crun]);
    fs::create_dir(&fx.crun).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::Crun]);
    assert!(fx.check().missing[0].what.contains("not a regular file"));
}

/// The version is read from `crun --version` and must be 1.14 or newer: 1.13 and 0.9 are
/// refused, a failing program and an answer that is no version are named, 1.14.0 and 2.0 pass.
#[test]
fn the_crun_version_must_be_1_14_or_newer() {
    let fx = Fixture::new("crun-host-version");
    let line = format!("{} --version", fx.crun.display());
    let cases = [
        (Ok("crun version 1.13.9\n"), false),
        (Ok("crun version 0.9\n"), false),
        (Ok("crun version 1.14.0\n"), true),
        (Ok("crun version 2.0.1\n"), true),
        (Ok("podman 5.0\n"), false),
        (Ok(""), false),
        (Err("exit status 1"), false),
    ];
    for (answer, passes) in cases {
        let commands = FakeCommands::default().with(&line, answer);
        let report = check(&fx.request(&fx.crun), &fx.host, &commands);
        let want = if passes {
            vec![]
        } else {
            vec![Piece::CrunVersion]
        };
        assert_eq!(pieces(&report), want, "{answer:?}");
    }
}

/// `newuidmap` and `newgidmap` are each looked for and named on their own; one that is neither
/// setuid nor holds file capabilities cannot write the id maps and is named too.
#[test]
fn each_missing_id_tool_is_named() {
    let fx = Fixture::new("crun-host-tools");
    let tools = fx.path("bin");
    fs::remove_file(tools.join("newuidmap")).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::NewUidMap]);
    fs::remove_file(tools.join("newgidmap")).unwrap();
    assert_eq!(
        pieces(&fx.check()),
        vec![Piece::NewUidMap, Piece::NewGidMap]
    );
    write(&tools.join("newuidmap"), "#!/bin/sh\n");
    write(&tools.join("newgidmap"), "#!/bin/sh\n");
    set_mode(&tools.join("newuidmap"), 0o755);
    set_mode(&tools.join("newgidmap"), 0o4755);
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::NewUidMap]);
    assert!(
        report.missing[0].what.contains("neither setuid"),
        "{}",
        report.missing[0]
    );
}

/// A job user that is not in the user database, or is root, is named, and no range is looked for
/// then (the ranges are the user's).
#[test]
fn a_missing_or_root_job_user_is_named() {
    let fx = Fixture::new("crun-host-user");
    let passwd = &fx.host.ids.passwd;
    write(passwd, "root:x:0:0::/root:/bin/sh\n");
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::JobUser]);
    assert_eq!(report.job_ids, None);
    write(passwd, &format!("{USER}:x:0:991::/:/bin/sh\n"));
    assert_eq!(pieces(&fx.check()), vec![Piece::JobUser]);
    write(passwd, &format!("{USER}:x:990:0::/:/bin/sh\n"));
    assert_eq!(pieces(&fx.check()), vec![Piece::JobUser]);
    fs::remove_file(passwd).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::JobUser]);
}

/// The subuid range and the subgid range are checked apart: a user without one is named for that
/// one only, a range of fewer than 65536 ids counts as missing, and a file that is absent does.
#[test]
fn the_subuid_and_subgid_ranges_are_each_named() {
    let fx = Fixture::new("crun-host-subids");
    write(&fx.host.ids.subuid, "someone:100000:65536\n");
    assert_eq!(pieces(&fx.check()), vec![Piece::SubUid]);
    write(&fx.host.ids.subuid, &format!("{USER}:100000:65536\n"));
    write(&fx.host.ids.subgid, &format!("{USER}:100000:65535\n"));
    assert_eq!(pieces(&fx.check()), vec![Piece::SubGid]);
    fs::remove_file(&fx.host.ids.subgid).unwrap();
    fs::remove_file(&fx.host.ids.subuid).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::SubUid, Piece::SubGid]);
}

/// The state root must exist and be private (`0700`): it holds the taint marker and the host
/// credential. A directory others can enter, a file and a missing path are each named.
#[test]
fn the_state_root_must_be_a_private_directory() {
    let fx = Fixture::new("crun-host-state");
    set_mode(&fx.state, 0o711);
    assert_eq!(pieces(&fx.check()), vec![Piece::StateRoot]);
    fs::remove_dir(&fx.state).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::StateRoot]);
    fs::write(&fx.state, "").unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::StateRoot]);
}

/// The scratch root must be `0711`: `0700` shuts the job's ids out, `0755` lets a job list the
/// other runs' names. A missing one is named too.
#[test]
fn the_scratch_root_must_be_mode_0711() {
    let fx = Fixture::new("crun-host-scratch");
    for mode in [0o700, 0o755, 0o710, 0o777] {
        set_mode(&fx.scratch, mode);
        assert_eq!(pieces(&fx.check()), vec![Piece::ScratchRoot], "{mode:o}");
    }
    set_mode(&fx.scratch, 0o711);
    assert_eq!(pieces(&fx.check()), vec![]);
    fs::remove_dir(&fx.scratch).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::ScratchRoot]);
}

/// Every directory above the scratch root must be searchable by others (the state directory
/// `0711`): each one that is not is named, with its path, so the operator fixes the right one.
#[test]
fn each_directory_above_the_scratch_root_that_others_cannot_search_is_named() {
    let fx = Fixture::new("crun-host-ancestor");
    let inner = fx.path("lib");
    fs::create_dir_all(inner.join("js")).unwrap();
    let scratch = inner.join("js/scratch");
    fs::create_dir(&scratch).unwrap();
    set_mode(&scratch, 0o711);
    set_mode(&inner.join("js"), 0o700);
    set_mode(&inner, 0o750);
    let request = Request {
        scratch_root: &scratch,
        ..fx.request(&fx.crun)
    };
    let report = check(&request, &fx.host, &fx.commands);
    assert_eq!(
        pieces(&report),
        vec![Piece::SearchableAncestor, Piece::SearchableAncestor]
    );
    let text = report.refusal().unwrap();
    assert!(text.contains(&inner.display().to_string()), "{text}");
    assert!(
        text.contains(&inner.join("js").display().to_string()),
        "{text}"
    );
    set_mode(&inner.join("js"), 0o711);
    set_mode(&inner, 0o711);
    assert_eq!(pieces(&check(&request, &fx.host, &fx.commands)), vec![]);
    // The directories are made searchable again so that the test directory is removed.
    set_mode(&inner, 0o755);
}

/// The agent's own cgroup must be on the version 2 hierarchy and hold `cpu`, `memory` and `pids`:
/// each controller that is not there is named, a hierarchy without a `0::` line and an unreadable
/// membership file are named too.
#[test]
fn the_cgroup_controllers_of_the_agents_own_cgroup_are_named_when_missing() {
    let fx = Fixture::new("crun-host-cgroup");
    let controllers = fx.path("cgroup/system.slice/js-agent.service/cgroup.controllers");
    write(&controllers, "cpuset cpu io pids\n");
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::CgroupDelegation]);
    assert!(
        report.missing[0].what.contains("memory"),
        "{}",
        report.missing[0]
    );
    assert!(!report.missing[0].what.contains("no cpu"));
    write(&controllers, "io\n");
    let what = &fx.check().missing[0].what;
    assert!(what.contains("cpu, memory, pids"), "{what}");
    write(&fx.host.self_cgroup, "12:memory:/x\n1:name=systemd:/x\n");
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::CgroupDelegation]);
    assert!(report.missing[0].what.contains("version 2"));
    fs::remove_file(&fx.host.self_cgroup).unwrap();
    assert_eq!(pieces(&fx.check()), vec![Piece::CgroupDelegation]);
    // A membership that names a cgroup with no controllers file.
    write(&fx.host.self_cgroup, "0::/elsewhere\n");
    assert_eq!(pieces(&fx.check()), vec![Piece::CgroupDelegation]);
}

/// The packaged unit starts the agent in the leaf `<unit>/supervisor` (`DelegateSubgroup=`). The
/// leaf's `cgroup.controllers` is what the unit has enabled for its children, which systemd does
/// not write for a delegated unit and the agent's delegate step writes after this check: the
/// check must ask what the unit is offered, and so pass a host whose leaf shows no controller.
/// Catches a check that refuses a stock systemd 255 host before the agent could enable them.
#[test]
fn an_agent_in_the_supervisor_leaf_is_checked_by_what_its_unit_is_offered() {
    let fx = Fixture::new("crun-host-supervisor");
    let unit = "cgroup/system.slice/js-agent-crun.service";
    write(
        &fx.path(&format!("{unit}/cgroup.controllers")),
        "cpuset cpu io memory pids\n",
    );
    // The leaf shows nothing: the unit has not enabled anything for its children yet.
    write(
        &fx.path(&format!("{unit}/supervisor/cgroup.controllers")),
        "\n",
    );
    write(
        &fx.host.self_cgroup,
        "0::/system.slice/js-agent-crun.service/supervisor\n",
    );
    assert_eq!(pieces(&fx.check()), vec![]);
    // A unit that is not offered `memory` is refused, naming it and the unit.
    write(
        &fx.path(&format!("{unit}/cgroup.controllers")),
        "cpu io pids\n",
    );
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::CgroupDelegation]);
    let what = &report.missing[0].what;
    assert!(
        what.contains("system.slice/js-agent-crun.service is not offered the memory"),
        "{what}"
    );
    assert!(!what.contains("cpu,"), "{what}");
}

/// Where the kernel restricts unprivileged user namespaces, a loaded AppArmor profile must cover
/// crun: none loaded, or a profile of another program, is named; a profile named `crun` covers
/// only `/usr/bin/crun`, and one named by the path covers that path; with the restriction off,
/// or no such switch in the kernel, no profile is needed.
#[test]
fn the_apparmor_profile_is_needed_only_where_user_namespaces_are_restricted() {
    let fx = Fixture::new("crun-host-apparmor");
    // The profile named `crun` attaches to the distribution's path, not to this one.
    write(&fx.host.apparmor_profiles, "crun (unconfined)\n");
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::AppArmorProfile]);
    assert!(
        report.missing[0]
            .what
            .contains(&fx.crun.display().to_string())
    );
    write(
        &fx.host.apparmor_profiles,
        &format!("{} (unconfined)\n", fx.crun.display()),
    );
    assert_eq!(pieces(&fx.check()), vec![]);
    // At the distribution's path the profile `crun` covers it.
    write(&fx.host.apparmor_profiles, "crun (unconfined)\n");
    let distro = check(
        &fx.request(Path::new("/usr/bin/crun")),
        &fx.host,
        &fx.commands,
    );
    assert!(
        !pieces(&distro).contains(&Piece::AppArmorProfile),
        "{distro:?}"
    );
    // A profile that only has `crun` in its name is no cover.
    write(
        &fx.host.apparmor_profiles,
        "crun-extra (unconfined)\nnot-crun (enforce)\n",
    );
    let distro = check(
        &fx.request(Path::new("/usr/bin/crun")),
        &fx.host,
        &fx.commands,
    );
    assert!(pieces(&distro).contains(&Piece::AppArmorProfile));
    // The profiles cannot be read although the restriction is on.
    fs::remove_file(&fx.host.apparmor_profiles).unwrap();
    assert!(pieces(&fx.check()).contains(&Piece::AppArmorProfile));
    // The restriction off, or not in this kernel: nothing is needed.
    write(&fx.host.apparmor_userns, "0\n");
    assert_eq!(pieces(&fx.check()), vec![]);
    fs::remove_file(&fx.host.apparmor_userns).unwrap();
    assert_eq!(pieces(&fx.check()), vec![]);
}

/// A host with nothing set up is refused once, and the refusal names each piece, with the count:
/// the operator fixes the host in one pass, not one exit 2 at a time.
#[test]
fn a_host_with_nothing_set_up_is_refused_naming_every_piece() {
    let fx = Fixture::new("crun-host-all");
    let nowhere = fx.path("nowhere");
    let host = CrunHost {
        ids: IdFiles::in_dir(&nowhere),
        apparmor_userns: fx.host.apparmor_userns.clone(),
        apparmor_profiles: nowhere.join("profiles"),
        cgroup_mount: nowhere.join("cgroup"),
        self_cgroup: nowhere.join("self"),
        id_tool_dirs: vec![nowhere.clone()],
        owner_uid: fx.host.owner_uid,
    };
    let request = Request {
        crun: &nowhere.join("crun"),
        job_user: USER,
        state_root: &nowhere.join("state"),
        scratch_root: &nowhere.join("scratch"),
    };
    let report = check(&request, &host, &fx.commands);
    let text = report.refusal().expect("a refusal");
    for name in [
        "crun:",
        "newuidmap:",
        "newgidmap:",
        "job user:",
        "state root:",
        "scratch root:",
        "cgroup v2 delegation:",
        "apparmor profile:",
    ] {
        assert!(text.contains(name), "{name} in {text}");
    }
    assert!(
        text.starts_with("--runtime crun: this host is not ready (8 missing):"),
        "{text}"
    );
    assert!(text.contains("(fix: "));
}

fn cli(extra: &[&str]) -> Cli {
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend_from_slice(extra);
    Cli::try_parse_from(args).unwrap()
}

/// The flags of the runtime are checked against each other before the host is read: `crun`
/// takes none of the engine's flags, needs a job user, a state root and a scratch root, and an
/// absolute `--crun-bin`; and the defaults are `/usr/bin/crun`.
#[test]
fn the_crun_flags_are_checked_against_each_other() {
    let ok = [
        "--runtime",
        "crun",
        "--job-user",
        USER,
        "--state-root",
        "/s",
        "--scratch-root",
        "/r",
    ];
    let cli_ok = cli(&ok);
    let request = flags(&cli_ok).unwrap();
    assert_eq!(request.crun, Path::new("/usr/bin/crun"));
    assert_eq!(request.job_user, USER);
    let with = |more: &[&str]| {
        let mut all = ok.to_vec();
        all.extend_from_slice(more);
        flags(&cli(&all)).map(|_| ()).unwrap_err()
    };
    assert!(with(&["--container-bin", "/usr/bin/docker"]).starts_with("--container-bin: "));
    assert!(with(&["--container-subuid", "x"]).starts_with("--container-subuid: "));
    assert!(with(&["--actions-cgroup", "/sys/fs/cgroup/x"]).starts_with("--actions-cgroup: "));
    assert!(with(&["--crun-bin", "crun"]).contains("not an absolute path"));
    // A lease's directories are under the run's project quota, so the flag is the same as for
    // any other runtime.
    let mut quota = ok.to_vec();
    quota.push("--scratch-quota");
    assert!(
        flags(&cli(&quota)).is_ok(),
        "--scratch-quota bounds /scratch, /work and /root alike"
    );
    assert!(with(&["--egress", "allow-list"]).starts_with("--egress: "));
    let mut none = ok.to_vec();
    none.extend(["--egress", "none"]);
    assert!(
        flags(&cli(&none)).is_ok(),
        "the loopback-only network is the one there is"
    );
    assert!(with(&["--mount-root", "sync=/m"]).starts_with("--mount-root: "));
    assert!(with(&["--mount-root-ro", "sync=/m"]).starts_with("--mount-root-ro: "));
    let without = |drop: &str| {
        let mut all: Vec<&str> = Vec::new();
        let mut skip = false;
        for word in ok {
            match (skip, word == drop) {
                (true, _) => skip = false,
                (false, true) => skip = true,
                (false, false) => all.push(word),
            }
        }
        flags(&cli(&all)).map(|_| ()).unwrap_err()
    };
    assert!(without("--job-user").starts_with("--job-user: required"));
    assert!(without("--state-root").starts_with("--state-root: required"));
    assert!(without("--scratch-root").starts_with("--scratch-root: required"));
}

/// `start` gives the refusal of a host that lacks a piece (so the agent exits 2) and, for a
/// complete host, the warnings and no error.
#[test]
fn start_refuses_an_incomplete_host_and_returns_the_warnings_of_a_complete_one() {
    let fx = Fixture::new("crun-host-start");
    let args = [
        "--runtime",
        "crun",
        "--job-user",
        USER,
        "--crun-bin",
        fx.crun.to_str().unwrap(),
        "--state-root",
        fx.state.to_str().unwrap(),
        "--scratch-root",
        fx.scratch.to_str().unwrap(),
    ];
    // Only the AppArmor profile (named for the distribution's path) is missing here.
    write(&fx.host.apparmor_profiles, "crun (unconfined)\n");
    let refused = start(&cli(&args), &fx.host, &fx.commands).unwrap_err();
    assert!(
        refused.contains("apparmor profile:") && refused.contains("(1 missing)"),
        "{refused}"
    );
    write(
        &fx.host.apparmor_profiles,
        &format!("{} (unconfined)\n", fx.crun.display()),
    );
    let ready = start(&cli(&args), &fx.host, &fx.commands).unwrap();
    assert_eq!(ready.warnings.len(), 1, "{:?}", ready.warnings);
    assert_eq!(
        (ready.crun, ready.job_user, ready.job_ids),
        (fx.crun.clone(), USER.to_owned(), (990, 991))
    );
}

/// `ready` builds the runtime's inputs from what the check found, and refuses job ids that are
/// missing or root's: every `crun` is run as them, so a missing user must never become root.
/// It catches a report with no ids (or a zero uid or gid) that goes through as `(0, 0)`.
#[test]
fn ready_refuses_job_ids_that_are_missing_or_root() {
    let request = Request {
        crun: Path::new("/usr/bin/crun"),
        job_user: USER,
        state_root: Path::new("/s"),
        scratch_root: Path::new("/r"),
    };
    let report = |job_ids| Report {
        job_ids,
        ..Report::default()
    };
    let good = ready(&request, report(Some((990, 991)))).expect("ids that are not root's");
    assert_eq!(good.job_ids, (990, 991));
    for ids in [None, Some((0, 991)), Some((990, 0)), Some((0, 0))] {
        let why = ready(&request, report(ids)).expect_err("refused");
        assert!(why.starts_with("--job-user: "), "{ids:?}: {why}");
    }
}

/// Runs the agent to its first wait with `extra`; returns (status, stderr).
fn run_agent(extra: &[&str]) -> (String, String) {
    let (ran, _, _) = run_scripted(extra, &linux_host(), vec![welcome(1)], 1);
    (ran.status(), ran.err)
}

/// The program refuses a runtime flag that cannot work, with status 2 and the flag named, before
/// it registers: `--runtime docker` without an engine program, and the crun flags without
/// `--runtime crun` (which would otherwise be ignored and run the engine runtime).
#[test]
fn the_agent_refuses_runtime_flags_that_cannot_work() {
    let cases = [
        (
            vec!["--runtime", "docker"],
            "--runtime docker: needs --container-bin",
        ),
        (
            vec!["--crun-bin", "/usr/bin/crun"],
            "--crun-bin: needs --runtime crun",
        ),
        (vec!["--job-user", USER], "--job-user: needs --runtime crun"),
        (
            vec![
                "--runtime",
                "docker",
                "--job-user",
                USER,
                "--container-bin",
                "/usr/bin/docker",
            ],
            "--job-user: needs --runtime crun",
        ),
        (
            vec![
                "--runtime",
                "crun",
                "--job-user",
                USER,
                "--state-root",
                "/s",
                "--scratch-root",
                "/r",
                "--mount-root",
                "sync=/m",
            ],
            "--mount-root: not accepted with --runtime crun",
        ),
    ];
    for (args, why) in cases {
        let (status, err) = run_agent(&args);
        assert_eq!(status, code(2), "{args:?}: {err}");
        assert!(err.contains(why), "{args:?}: {err}");
    }
}

/// A value that parses for the `--native-*` flag `name`: the numbers and the level are
/// given their own, every other flag takes a path. A new flag of another type fails the parse
/// here, so whoever adds it says what it takes.
fn native_value(name: &str) -> &'static str {
    match name {
        "native-network" => "off",
        "native-max-file-mib" | "native-max-processes" => "1",
        "native-pf-uid-start" | "native-pf-uid-count" => "600",
        _ => "/native",
    }
}

/// The `--native-*` flags clap knows, read from the definition of `Cli`.
fn native_flag_names() -> Vec<String> {
    let names: Vec<String> = Cli::command()
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .filter(|long| long.starts_with("native-"))
        .map(str::to_owned)
        .collect();
    assert!(names.len() >= 7, "{names:?}");
    names
}

/// `--runtime crun` refuses every `--native-*` flag, each by name, on the command line of an
/// otherwise complete crun agent. The flags are walked from the definition of `Cli`, so a new
/// native flag that `flags` does not refuse fails here. It catches a refusal list that misses
/// a native flag, which would start a crun agent that ignores it.
#[test]
fn the_crun_runtime_refuses_every_native_flag_by_name() {
    for name in native_flag_names() {
        let flag = format!("--{name}");
        let cli = cli(&[
            "--runtime",
            "crun",
            "--job-user",
            USER,
            "--state-root",
            "/s",
            "--scratch-root",
            "/r",
            &flag,
            native_value(&name),
        ]);
        let why = flags(&cli).map(|_| ()).unwrap_err();
        assert!(
            why.starts_with(&format!("{flag}: not accepted with --runtime crun")),
            "{flag}: {why}"
        );
    }
}

/// The whole program, given `--runtime crun` with the native socket, ends with status 2 and the
/// socket named, before it registers and before any host file is read.
#[test]
fn the_agent_refuses_the_crun_runtime_with_the_native_socket() {
    let (status, err) = run_agent(&[
        "--runtime",
        "crun",
        "--job-user",
        USER,
        "--state-root",
        "/s",
        "--scratch-root",
        "/r",
        "--native-helper-socket",
        "/native/helper.sock",
    ]);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--native-helper-socket: not accepted with --runtime crun"),
        "{err}"
    );
}

/// The whole program, given `--runtime docker` with an engine program and the native socket,
/// ends with status 2 and the socket named, before it registers: containers run on Linux only
/// and native jobs on a Mac only, so no host takes both, and a Linux host says whose flag it
/// is.
#[test]
fn the_agent_refuses_the_docker_runtime_with_the_native_socket() {
    let (ran, sent, _) = run_scripted(
        &[
            "--runtime",
            "docker",
            "--container-bin",
            "/usr/bin/docker",
            "--native-helper-socket",
            "/native/helper.sock",
            "--state-root",
            "/s",
            "--scratch-root",
            "/r",
        ],
        &linux_host(),
        vec![welcome(1)],
        1,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.borrow().is_empty());
    assert!(
        ran.err
            .contains("--native-helper-socket: native jobs run on macOS only"),
        "{}",
        ran.err
    );
}

/// `--runtime crun` ends the agent with status 2 and the check's refusal when `crun` is not
/// where `--crun-bin` says, naming it with the other pieces this host lacks. Nothing is sent to
/// the server.
#[test]
fn the_agent_exits_2_naming_the_missing_crun() {
    let (ran, sent, _) = run_scripted(
        &[
            "--runtime",
            "crun",
            "--job-user",
            "js-no-such-user-for-this-test",
            "--crun-bin",
            "/nonexistent/crun",
            "--state-root",
            "/nonexistent/state",
            "--scratch-root",
            "/nonexistent/scratch",
        ],
        &linux_host(),
        vec![welcome(1)],
        1,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("--runtime crun: this host is not ready"),
        "{}",
        ran.err
    );
    for piece in [
        "crun: /nonexistent/crun",
        "job user:",
        "state root:",
        "scratch root:",
    ] {
        assert!(ran.err.contains(piece), "{piece} in {}", ran.err);
    }
    assert_eq!(sent.borrow().len(), 0);
}

/// An id tool that is not executable is named as that (not as a missing setuid bit), and a
/// passwd with a line too short to be an entry does not hide the job user after it.
#[test]
fn an_id_tool_that_cannot_run_is_named_and_a_short_passwd_line_is_skipped() {
    let fx = Fixture::new("crun-host-tool-mode");
    set_mode(&fx.path("bin/newgidmap"), 0o644);
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::NewGidMap]);
    assert!(
        report.missing[0].what.contains("not executable"),
        "{}",
        report.missing[0]
    );
    set_mode(&fx.path("bin/newgidmap"), 0o4755);
    let passwd = &fx.host.ids.passwd;
    write(
        passwd,
        &format!("broken\nshort:x:1\n{USER}:x:990:991::/:/bin/sh\n"),
    );
    assert_eq!(pieces(&fx.check()), vec![]);
}

/// A scratch root that is a file, not a directory, is named as the scratch root.
#[test]
fn a_scratch_root_that_is_a_file_is_named() {
    let fx = Fixture::new("crun-host-scratch-file");
    fs::remove_dir(&fx.scratch).unwrap();
    fs::write(&fx.scratch, "").unwrap();
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::ScratchRoot]);
    assert!(report.missing[0].what.contains("not a directory"));
}

/// Each piece is named in the refusal by its own words, and no two share one: the operator
/// reads these to find what to fix.
#[test]
fn every_piece_has_its_own_name() {
    let all = [
        Piece::Crun,
        Piece::CrunVersion,
        Piece::NewUidMap,
        Piece::NewGidMap,
        Piece::JobUser,
        Piece::SubUid,
        Piece::SubGid,
        Piece::StateRoot,
        Piece::ScratchRoot,
        Piece::SearchableAncestor,
        Piece::CgroupDelegation,
        Piece::AppArmorProfile,
    ];
    let mut names: Vec<&str> = all.iter().map(|piece| piece.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len());
}

/// `flags` is for `--runtime crun` only: without it the answer is an error, not a request.
#[test]
fn flags_without_the_crun_runtime_is_an_error() {
    let cli = cli(&["--runtime", "docker", "--container-bin", "/usr/bin/docker"]);
    assert_eq!(
        flags(&cli).map(|_| ()).unwrap_err(),
        "--runtime crun: not given"
    );
}

/// A Mac is told `--runtime crun` is Linux only, before any host file is read.
#[test]
fn a_mac_refuses_the_crun_runtime() {
    let (ran, _, _) = run_scripted(
        &["--runtime", "crun", "--job-user", USER],
        &mac_host(),
        vec![welcome(1)],
        1,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("--runtime crun: Linux only"),
        "{}",
        ran.err
    );
}

/// The agent runs `crun` as the job user, so a `crun` only root can execute is refused (mode
/// 0700, or 0750 with a group that is not the job user's), and the job user's own group bit counts.
#[test]
fn a_crun_the_job_user_cannot_execute_is_refused() {
    let fx = Fixture::new("crun-host-crun-x");
    set_mode(&fx.crun, 0o700);
    assert_eq!(pieces(&fx.check()), vec![Piece::Crun]);
    assert!(fx.check().missing[0].what.contains("job user"));
    set_mode(&fx.crun, 0o750);
    assert_eq!(pieces(&fx.check()), vec![Piece::Crun]);
    // The file's own group is the job user's group: the group bit is the one that decides.
    let gid = fs::metadata(&fx.crun).unwrap().gid();
    write(
        &fx.host.ids.passwd,
        &format!("{USER}:x:990:{gid}::/:/usr/sbin/nologin\n"),
    );
    assert_eq!(pieces(&fx.check()), vec![]);
    set_mode(&fx.crun, 0o705);
    assert_eq!(pieces(&fx.check()), vec![Piece::Crun]);
}

/// A directory above `crun` (or above an id tool) that the job user cannot search refuses the
/// program in it, even when the program's own mode is fine.
#[test]
fn a_crun_below_a_directory_the_job_user_cannot_search_is_refused() {
    let fx = Fixture::new("crun-host-crun-dir");
    let bin = fx.crun.parent().unwrap().to_owned();
    set_mode(&bin, 0o700);
    let report = fx.check();
    assert_eq!(
        pieces(&report),
        vec![Piece::Crun, Piece::NewUidMap, Piece::NewGidMap]
    );
    assert!(report.missing[0].what.contains(&bin.display().to_string()));
    set_mode(&bin, 0o755);
    assert_eq!(pieces(&fx.check()), vec![]);
}

/// A state root or scratch root that a user other than root owns is refused whatever its mode:
/// the owner can change the mode and the files under it.
#[test]
fn a_state_or_scratch_root_not_owned_by_root_is_refused() {
    let mut fx = Fixture::new("crun-host-owner");
    assert_eq!(pieces(&fx.check()), vec![]);
    // The directories are owned by the uid the tests run as; the check wants another.
    fx.host.owner_uid += 1;
    let report = fx.check();
    assert_eq!(pieces(&report), vec![Piece::StateRoot, Piece::ScratchRoot]);
    assert!(report.missing[0].what.contains("not root"));
}

/// The kernel attaches an AppArmor profile by the resolved path: a link to a covered program is
/// accepted, a link to one no profile names is refused though the link's own path is named.
#[test]
fn the_apparmor_profile_is_matched_against_the_resolved_crun() {
    let fx = Fixture::new("crun-host-link");
    let link = fx.path("crun-link");
    symlink(&fx.crun, &link).unwrap();
    let to_covered = check(&fx.request(&link), &fx.host, &fx.commands_for(&link));
    assert_eq!(pieces(&to_covered), vec![]);
    // Only the link is named by a profile; the program it leads to is not.
    write(
        &fx.path("profiles"),
        &format!("{} (unconfined)\n", link.display()),
    );
    let report = check(&fx.request(&link), &fx.host, &fx.commands_for(&link));
    assert_eq!(pieces(&report), vec![Piece::AppArmorProfile]);
    assert!(
        report.missing[0]
            .what
            .contains(&fx.crun.display().to_string())
    );
}
