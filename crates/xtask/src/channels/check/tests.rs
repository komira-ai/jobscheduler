use std::cell::RefCell;

use super::*;
use js_testkit::dir::TestDir;

const SAYS: &str = "js-agent 1.2.3\nruntimes: docker crun\n";
const URL: &str = "https://packages.komira.ai/apt";
/// The line `systemd-sysusers` writes to `/etc/passwd` for the package's `sysusers.d` file, uid
/// and gid as it picks them from the system range.
const SYSUSERS_LINE: &str =
    "jsjob:x:998:998:jobscheduler job user:/nonexistent:/usr/sbin/nologin\n";
/// The line the postinst writes to `/etc/subuid` and to `/etc/subgid` on a clean image.
const SUBID_LINE: &str = "jsjob:200000:65536\n";
/// The rest of a clean image's `/etc/passwd`.
const BASE_PASSWD: &str =
    "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-check-{name}"))
}

/// A machine that answers for `apt-get`, `dpkg`, `dpkg-query`, `curl` and the installed binary, and does to a scratch root what each would do to a real one.
struct Machine {
    root: PathBuf,
    calls: RefCell<Vec<String>>,
    arch: &'static str,
    keyring: Vec<u8>,
    dpkg_version: &'static str,
    says: &'static str,
    /// Files below the root that the install does not create.
    omit: Vec<&'static str>,
    /// Links below `etc/systemd/system` the install creates.
    enable: Vec<&'static str>,
    /// Does a process named `js-agent` run?
    running: bool,
    /// A command line (joined) containing this text is refused.
    refuse: Option<&'static str>,
    /// The lines the install adds to `etc/passwd` (the postinst's job user); `None` leaves no
    /// `etc/passwd` at all.
    passwd: Option<&'static str>,
    /// The `etc/subuid` and `etc/subgid` the install leaves (the postinst's ranges); `None`
    /// leaves no such file.
    subids: (Option<&'static str>, Option<&'static str>),
}

impl Machine {
    fn new(root: &Path) -> Machine {
        Machine {
            root: root.to_path_buf(),
            calls: RefCell::new(Vec::new()),
            arch: "amd64",
            keyring: vec![0x99, 0x01, 0x0d],
            dpkg_version: "1.2.3",
            says: SAYS,
            omit: Vec::new(),
            enable: Vec::new(),
            running: false,
            refuse: None,
            passwd: Some(SYSUSERS_LINE),
            subids: (Some(SUBID_LINE), Some(SUBID_LINE)),
        }
    }

    /// The refusal for the command line `line`, when the machine is told to refuse it.
    fn refusal(&self, line: &str) -> Option<Answer> {
        let text = self.refuse.filter(|text| line.contains(text))?;
        Some(Answer {
            ok: false,
            stdout: String::new(),
            stderr: format!("refused: {text}\n"),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        write(&self.root.join(rel), bytes).expect("write");
    }

    fn install(&self) {
        for rel in files() {
            if !self.omit.contains(&rel) {
                self.put(rel, b"file");
            }
        }
        for link in &self.enable {
            self.put(link, b"link");
        }
        if self.running {
            self.put("proc/42/comm", b"js-agent\n");
        }
        self.put("proc/43/comm", b"bash\n");
        if let Some(lines) = self.passwd {
            self.put("etc/passwd", format!("{BASE_PASSWD}{lines}").as_bytes());
        }
        for (rel, text) in [("etc/subuid", self.subids.0), ("etc/subgid", self.subids.1)] {
            if let Some(text) = text {
                self.put(rel, text.as_bytes());
            }
        }
    }
}

fn answer(stdout: &str) -> Result<Answer, Error> {
    Ok(Answer {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

impl Host for Machine {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let name = program
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        let line = format!("{name} {}", args.join(" "));
        self.calls.borrow_mut().push(line.clone());
        if let Some(refused) = self.refusal(&line) {
            return Ok(refused);
        }
        match (name.as_str(), args.first().map(String::as_str)) {
            ("dpkg", _) => answer(&format!("{}\n", self.arch)),
            ("dpkg-query", _) => answer(self.dpkg_version),
            ("apt-get", Some("install")) if line.contains("js-agent=") => {
                self.install();
                answer("")
            }
            ("curl", _) => {
                let at = args.iter().position(|a| a == "--output").expect("--output");
                write(Path::new(&args[at + 1]), &self.keyring).expect("keyring");
                answer("")
            }
            ("js-agent", Some("--version")) => answer(self.says),
            _ => answer(""),
        }
    }
}

fn apt<'a>(root: &'a Path) -> Apt<'a> {
    Apt {
        root,
        arch: "amd64",
        version: "1.2.3",
        apt_url: URL,
        apt_get: Path::new("apt-get"),
        dpkg_query: Path::new("dpkg-query"),
        dpkg: Path::new("dpkg"),
        curl: Path::new("curl"),
    }
}

#[test]
fn a_clean_install_from_testing_is_checked_and_adds_the_repository_the_way_a_user_does() {
    // Catches: a source that is not deb822 with Signed-By and the suite testing, a package
    // installed at no version, and the order (update, upgrade, certificates, keyring, source, update, install).
    let dir = scratch("apt-ok");
    let machine = Machine::new(&dir);
    let said = check_apt(&machine, &apt(&dir)).expect("checked");
    assert_eq!(
        machine
            .calls()
            .iter()
            .map(|c| c.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>(),
        [
            "dpkg",
            "apt-get",
            "apt-get",
            "apt-get",
            "curl",
            "apt-get",
            "apt-get",
            "dpkg-query",
            "js-agent"
        ]
    );
    assert!(
        machine
            .calls()
            .contains(&"apt-get install -y --no-install-recommends js-agent=1.2.3".to_owned())
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(SOURCES)).expect("source"),
        "Types: deb\nURIs: https://packages.komira.ai/apt\nSuites: testing\nComponents: main\nArchitectures: amd64\nSigned-By: /usr/share/keyrings/komira-archive-keyring.gpg\n"
    );
    assert!(
        machine
            .calls()
            .iter()
            .any(|c| c.ends_with("https://packages.komira.ai/apt/komira-archive-keyring.gpg"))
    );
    assert!(
        said[0].starts_with("check-apt: js-agent 1.2.3 installed from the suite testing on amd64;"),
        "{said:?}"
    );
}

#[test]
fn a_package_missing_any_of_its_nine_files_is_named() {
    // Catches: a package that installs without the files the units and the setup script need.
    let dir = scratch("apt-files");
    let mut machine = Machine::new(&dir);
    machine.omit = files().collect();
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    let want: Vec<String> = files()
        .map(|rel| format!("the package's file /{rel} is not installed"))
        .collect();
    assert_eq!(problems, want);
    assert_eq!(
        files().collect::<Vec<_>>(),
        [
            "usr/bin/js-agent",
            "usr/lib/systemd/system/js-agent-crun.service",
            "usr/lib/systemd/system/js-agent-crun-gpu.service",
            "usr/lib/tmpfiles.d/js-agent-crun.conf",
            "usr/lib/sysusers.d/js-agent.conf",
            "usr/lib/systemd/system/js-agent.service",
            "usr/lib/tmpfiles.d/js-agent.conf",
            "usr/share/js-agent/crun_host_setup.sh",
            "usr/share/doc/js-agent/copyright",
        ]
    );
}

#[test]
fn a_unit_that_the_install_enabled_or_a_process_it_started_is_named() {
    // Catches: a postinst that enables or starts a unit on install, which the package promises
    // it does not; any of the three units counts.
    let dir = scratch("apt-enabled");
    let mut machine = Machine::new(&dir);
    machine.enable = vec![
        "etc/systemd/system/multi-user.target.wants/js-agent-crun.service",
        "etc/systemd/system/js-agent-crun-gpu.service",
        "etc/systemd/system/multi-user.target.wants/js-agent.service",
    ];
    machine.running = true;
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(problems.len(), 4, "{problems:?}");
    for (problem, tail) in problems.iter().zip([
        "etc/systemd/system/js-agent-crun-gpu.service",
        "etc/systemd/system/multi-user.target.wants/js-agent-crun.service",
        "etc/systemd/system/multi-user.target.wants/js-agent.service",
    ]) {
        assert!(problem.starts_with("a unit is enabled: "), "{problems:?}");
        assert!(
            problem.ends_with(&format!("{tail} exists, and the package enables nothing")),
            "{problems:?}"
        );
    }
    assert_eq!(
        problems[3],
        "js-agent is running (process 42), and the package starts nothing"
    );
}

#[test]
fn a_file_that_is_not_a_directory_below_the_systemd_directory_is_not_a_wants_entry() {
    // Catches: a stray file named like a unit's directory read as a directory holding a link.
    let dir = scratch("apt-stray");
    let mut machine = Machine::new(&dir);
    machine.enable = vec!["etc/systemd/system/stray"];
    assert!(check_apt(&machine, &apt(&dir)).is_ok());
}

#[test]
fn a_package_at_another_version_or_a_binary_that_says_another_is_refused() {
    // Catches: apt installing a version other than the one asked, and a binary of another build.
    let dir = scratch("apt-version");
    let mut machine = Machine::new(&dir);
    machine.dpkg_version = "1.2.2";
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        ["dpkg says js-agent \"1.2.2\" is installed, not \"1.2.3\""]
    );
    for (name, says) in [
        ("apt-says-other", "js-agent 1.2.2\nruntimes: docker crun\n"),
        ("apt-says-suffix", "js-agent 1.2.3+0123456789ab\n"),
        ("apt-says-nothing", ""),
    ] {
        let dir = scratch(name);
        let mut machine = Machine::new(&dir);
        machine.says = says;
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        let first = says.lines().next().unwrap_or_default();
        assert_eq!(
            e,
            format!("the binary says {first:?}, not \"js-agent 1.2.3\"")
        );
    }
    // Only the first line is judged: the runtimes line follows it.
    let dir = scratch("apt-says-first-line");
    let mut machine = Machine::new(&dir);
    machine.says = "js-agent 1.2.3\nruntimes: docker\nanything\n";
    assert!(check_apt(&machine, &apt(&dir)).is_ok());
}

#[test]
fn an_armored_or_empty_keyring_is_refused_because_signed_by_takes_a_gpg_file() {
    // Catches: a keyring published as armor, which apt silently does not read from Signed-By.
    for keyring in [b"-----BEGIN PGP PUBLIC KEY BLOCK-----".to_vec(), Vec::new()] {
        let dir = scratch("apt-keyring");
        let mut machine = Machine::new(&dir);
        machine.keyring = keyring;
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        assert!(
            e.contains("komira-archive-keyring.gpg is not a binary keyring"),
            "{e}"
        );
        assert!(!dir.join(SOURCES).exists());
    }
}

#[test]
fn a_program_that_fails_is_named_with_what_it_said_and_a_machine_of_another_architecture_is_refused()
 {
    // Catches: an apt-get update that fails (a signature apt could not verify) read as a pass.
    for (refuse, program) in [
        ("update", "apt-get update"),
        ("dist-upgrade", "apt-get dist-upgrade"),
        ("ca-certificates", "ca-certificates"),
        ("--output", "--output"),
        ("js-agent=", "js-agent=1.2.3"),
        ("--showformat", "dpkg-query"),
        ("--version", "js-agent --version"),
        ("--print-architecture", "dpkg --print-architecture"),
    ] {
        let dir = scratch("apt-refused");
        let mut machine = Machine::new(&dir);
        machine.refuse = Some(refuse);
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        assert!(
            e.contains(program) && e.contains(&format!("failed: refused: {refuse}")),
            "{refuse}: {e}"
        );
    }
    let dir = scratch("apt-arch");
    let mut machine = Machine::new(&dir);
    machine.arch = "arm64";
    let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
    assert_eq!(
        e,
        "this machine is arm64, not amd64: the check would install another package"
    );
}

#[test]
fn flags_out_of_their_form_are_usage_errors_before_anything_runs() {
    // Catches: a check that installs from a plain-HTTP repository or for an architecture it is
    // not on.
    let dir = scratch("apt-usage");
    let machine = Machine::new(&dir);
    for change in [
        |r: &mut Apt| r.version = "v1.2.3",
        |r: &mut Apt| r.arch = "i386",
        |r: &mut Apt| r.apt_url = "http://packages.komira.ai/apt",
        |r: &mut Apt| r.apt_url = "https://packages.komira.ai/apt/",
    ] {
        let mut r = apt(&dir);
        change(&mut r);
        assert!(matches!(check_apt(&machine, &r), Err(Error::Usage(_))));
    }
    assert!(machine.calls().is_empty());
}

#[test]
fn the_image_is_updated_and_upgraded_before_certificates_and_curl_are_installed() {
    // Catches: the failure of the first release's amd64 install check, `apt-get install -y
    // ca-certificates curl` on the digest-pinned ubuntu:24.04 image refused with "you have held
    // broken packages" because the image's packages are older than the archive's: the apt-get
    // calls are `update`, then the upgrade, then the install of the two packages, before the
    // repository is added, and the second `update` follows the source file.
    let dir = scratch("apt-upgrade-order");
    let machine = Machine::new(&dir);
    check_apt(&machine, &apt(&dir)).expect("checked");
    let calls: Vec<String> = machine
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("apt-get "))
        .collect();
    assert_eq!(
        calls,
        [
            "apt-get update",
            "apt-get dist-upgrade -y -o Dpkg::Options::=--force-confold",
            "apt-get install -y --no-install-recommends ca-certificates curl",
            "apt-get update",
            "apt-get install -y --no-install-recommends js-agent=1.2.3",
        ]
    );
}

/// The problems `check-apt` finds when the install leaves `passwd` (the job user's lines) in
/// `etc/passwd`, and the root it ran on.
fn passwd_problems(name: &str, passwd: Option<&'static str>) -> (Vec<String>, TestDir) {
    let dir = scratch(name);
    let mut machine = Machine::new(&dir);
    machine.passwd = passwd;
    let problems = match check_apt(&machine, &apt(&dir)) {
        Ok(_) => Vec::new(),
        Err(Error::Refused(problems)) => problems,
        Err(e) => panic!("{name}: {e}"),
    };
    (problems, dir)
}

/// The agent's refusal of a host with no `jsjob` in the root's `etc/passwd`, as check-apt words it.
#[cfg(target_os = "linux")]
fn no_user(dir: &Path) -> String {
    format!(
        "the agent's start check refuses the host: job user: no user \"jsjob\" in {}/etc/passwd (fix: create the account (`useradd --system --no-create-home --shell /usr/sbin/nologin jsjob`))",
        dir.display()
    )
}

// The agent's start check is Linux only (js_agent::crun_host), so the tests of its refusals run
// on Linux; check-apt itself runs in a Linux container.
#[cfg(target_os = "linux")]
#[test]
fn an_install_whose_postinst_made_no_job_user_is_refused_with_the_agents_own_words() {
    // Catches: a package that installs without making jsjob (no sysusers.d file, no postinst, a
    // postinst that does not run systemd-sysusers): the agent would exit 2 at its first start,
    // "job user: no user "jsjob" in /etc/passwd". Also an /etc/passwd that is not there at all,
    // and a jsjob line whose ids do not parse, which the agent reads as no user.
    let (problems, dir) = passwd_problems("user-none", Some(""));
    assert_eq!(problems, [no_user(&dir)]);

    let (problems, dir) = passwd_problems("user-no-passwd", None);
    assert_eq!(
        problems,
        [format!(
            "the agent's start check refuses the host: job user: {}/etc/passwd: No such file or directory (os error 2) (fix: create the account (`useradd --system --no-create-home --shell /usr/sbin/nologin jsjob`))",
            dir.display()
        )]
    );

    // Another account's name that starts with jsjob is not the job user.
    let (problems, dir) = passwd_problems(
        "user-prefix",
        Some("jsjobs:x:998:998::/nonexistent:/usr/sbin/nologin\n"),
    );
    assert_eq!(problems, [no_user(&dir)]);

    let (problems, dir) = passwd_problems(
        "user-uid-text",
        Some("jsjob:x:abc:998::/nonexistent:/usr/sbin/nologin\n"),
    );
    assert_eq!(
        problems,
        [
            no_user(&dir),
            "the job user jsjob has uid \"abc\", not a system uid (1 to 999)".to_owned(),
        ]
    );

    let (problems, dir) = passwd_problems("user-short", Some("jsjob:x\n"));
    assert_eq!(
        problems,
        [
            no_user(&dir),
            "the job user jsjob has uid \"\", not a system uid (1 to 999)".to_owned(),
            "the job user jsjob has the home \"\", not \"/nonexistent\"".to_owned(),
            "the job user jsjob has the shell \"\", not \"/usr/sbin/nologin\"".to_owned(),
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_job_user_with_root_ids_is_refused_by_the_agents_check_and_as_no_system_uid() {
    // Catches: a sysusers line that pins uid or gid 0, with which every crun would run as root.
    let (problems, _dir) = passwd_problems(
        "user-root",
        Some("jsjob:x:0:0:jobscheduler job user:/nonexistent:/usr/sbin/nologin\n"),
    );
    assert_eq!(
        problems,
        [
            "the agent's start check refuses the host: job user: \"jsjob\" has uid 0 and gid 0: a job never runs as root (fix: name a user with a uid and a gid above 0)",
            "the job user jsjob has uid \"0\", not a system uid (1 to 999)",
        ]
    );
    let (problems, _dir) = passwd_problems(
        "user-root-gid",
        Some("jsjob:x:998:0:jobscheduler job user:/nonexistent:/usr/sbin/nologin\n"),
    );
    assert_eq!(
        problems,
        [
            "the agent's start check refuses the host: job user: \"jsjob\" has uid 998 and gid 0: a job never runs as root (fix: name a user with a uid and a gid above 0)"
        ]
    );
}

#[test]
fn a_job_user_that_is_not_a_system_account_without_home_and_login_is_named() {
    // Catches: a sysusers line (or a hand-made account the postinst kept) that gives the job
    // user a regular uid, a home or a login shell; the agent's check accepts those, the package
    // promises none of them. Each field is named on its own, and the uid is judged at both ends
    // of the system range.
    let (problems, _dir) = passwd_problems(
        "user-regular",
        Some("jsjob:x:1000:1000::/home/jsjob:/bin/bash\n"),
    );
    assert_eq!(
        problems,
        [
            "the job user jsjob has uid \"1000\", not a system uid (1 to 999)",
            "the job user jsjob has the home \"/home/jsjob\", not \"/nonexistent\"",
            "the job user jsjob has the shell \"/bin/bash\", not \"/usr/sbin/nologin\"",
        ]
    );
    for (name, line) in [
        (
            "user-uid-1",
            "jsjob:x:1:1::/nonexistent:/usr/sbin/nologin\n",
        ),
        (
            "user-uid-999",
            "jsjob:x:999:999::/nonexistent:/usr/sbin/nologin\n",
        ),
    ] {
        let (problems, _dir) = passwd_problems(name, Some(line));
        assert_eq!(problems, Vec::<String>::new(), "{name}");
    }
}

#[test]
fn the_check_says_the_job_user_was_accepted() {
    // Catches: a pass line that claims less than the check did, or more.
    let dir = scratch("user-ok");
    let machine = Machine::new(&dir);
    let said = check_apt(&machine, &apt(&dir)).expect("checked");
    assert_eq!(
        said,
        [
            "check-apt: js-agent 1.2.3 installed from the suite testing on amd64; it says \"js-agent 1.2.3\"; its 9 files are there; the agent's start check accepts the job user jsjob and its subordinate ranges; no unit is enabled and nothing runs"
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_start_check_of_check_apt_runs_no_program() {
    // Catches: a runner that starts a program on the checked machine, which the job user's part
    // of the agent's check never needs: either way of running is refused, naming the program.
    use js_agent::CommandRunner as _;
    assert_eq!(
        NoPrograms.run("/usr/bin/crun", &["--version"]),
        Err("/usr/bin/crun: not run by check-apt".to_owned())
    );
    assert_eq!(
        NoPrograms.run_in("/usr/bin/crun", &["--version"], &[("PATH", "/usr/bin")]),
        Err("/usr/bin/crun: not run by check-apt".to_owned())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_install_whose_postinst_left_the_job_user_without_ranges_is_refused_with_the_agents_own_words()
{
    // Catches: a package that makes jsjob but no subordinate range, the second refusal a host
    // set up from the package alone met ("no range for the daemon's user jsjob"); each file is
    // judged on its own, a range of another user or of too few ids is no range, and a range
    // given by the job user's uid is one.
    let want = |dir: &Path, file: &str| {
        format!(
            "the agent's start check refuses the host: {kind} range: {path}/etc/{file} has no range for the daemon's user jsjob (uid 998): the container driver runs every container in a user namespace whose ids map only to subordinate ids. Add one (usermod --add-subuids 100000-165535 --add-subgids 100000-165535 <user>) (fix: add jsjob:<first id>:65536 to {path}/etc/{file} (`usermod --add-subuids` and `--add-subgids`))",
            kind = &file[..6],
            path = dir.display()
        )
    };
    for (name, subids, missing) in [
        ("ranges-none", (None, None), &["subuid", "subgid"][..]),
        (
            "ranges-uid-only",
            (Some(SUBID_LINE), Some("")),
            &["subgid"][..],
        ),
        (
            "ranges-other-user",
            (Some("alice:200000:65536\n"), Some(SUBID_LINE)),
            &["subuid"][..],
        ),
        (
            "ranges-by-uid",
            (Some("998:200000:65536\n"), Some("998:200000:65536\n")),
            &[][..],
        ),
    ] {
        let dir = scratch(name);
        let mut machine = Machine::new(&dir);
        machine.subids = subids;
        let problems = match check_apt(&machine, &apt(&dir)) {
            Ok(_) => Vec::new(),
            Err(Error::Refused(problems)) => problems,
            Err(e) => panic!("{name}: {e}"),
        };
        let expected: Vec<String> = missing.iter().map(|file| want(&dir, file)).collect();
        assert_eq!(problems, expected, "{name}");
    }
    // Fewer ids than the agent asks for is its other refusal.
    let dir = scratch("ranges-too-few");
    let mut machine = Machine::new(&dir);
    machine.subids = (Some("jsjob:200000:65535\n"), Some(SUBID_LINE));
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        [format!(
            "the agent's start check refuses the host: subuid range: {path}/etc/subuid gives the daemon's user jsjob (uid 998) 65535 subordinate ids; at least 65536 are needed so that every id an image uses maps (fix: add jsjob:<first id>:65536 to {path}/etc/subuid (`usermod --add-subuids` and `--add-subgids`))",
            path = dir.display()
        )]
    );
}
