//! The postinst of the js-agent .deb (`packaging/js-agent/deb/postinst`), run as dpkg runs it,
//! under `sh` with `DPKG_ROOT` naming a scratch root: it makes the job user through
//! `systemd-sysusers` (a `host-shim` stand-in that adds the account, as the real one does from
//! the package's sysusers file) and gives the user a subordinate uid and gid range in each file
//! that has none, which the agent's own start check then accepts. Only the stand-in is on its
//! `PATH`, so the script runs no other program.
#![cfg(target_os = "linux")]

mod crun_host;

use std::path::{Path, PathBuf};
use std::process::Command;

use crun_host::{SHIM, esc, install_program, profile_env, real, repo, rule};
use xtask::channels::check::job_user_refusals;

const POSTINST: &str = "packaging/js-agent/deb/postinst";
/// The account `systemd-sysusers` makes from the package's sysusers file, at the uid it picks.
const JSJOB: &str = "jsjob:x:998:998:jobscheduler job user:/nonexistent:/usr/sbin/nologin\n";
/// A clean image's accounts and groups.
const PASSWD: &str =
    "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";
const GROUP: &str = "root:x:0:\nnogroup:x:65534:\n";
/// The range a clean image gets in each file.
const FIRST_FREE: &str = "jsjob:200000:65536\n";
/// What the postinst says when no block of ids is free.
const NO_ROOM: &str = "js-agent: no free block of 65536 ids below 2^32 for jsjob in /etc/subuid and /etc/subgid; add one, or the agent will not start\n";

/// A scratch root with the stand-in for `systemd-sysusers`.
struct Rig {
    _scratch: js_testkit::dir::TestDir,
    root: PathBuf,
    shim: PathBuf,
}

/// How a run of the postinst ended.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    code: i32,
    stderr: String,
    calls: Vec<String>,
}

impl Rig {
    /// A root holding a clean image's passwd and group, the subid files `subuid` and `subgid`
    /// (none when `None`), and a `systemd-sysusers` that adds [`JSJOB`] to the passwd and exits
    /// `sysusers_code`.
    fn new(
        name: &str,
        subuid: Option<&str>,
        subgid: Option<&str>,
        sysusers_code: u8,
        sysusers_adds: &str,
    ) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            std::env::temp_dir(),
            &format!("xtask-postinst-{name}"),
        );
        let root = scratch.join("root");
        let shim = scratch.join("shim");
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        std::fs::create_dir_all(&shim).expect("shim");
        install_program(Path::new(SHIM), &shim.join("systemd-sysusers"));
        let rig = Rig {
            root,
            shim,
            _scratch: scratch,
        };
        rig.write("etc/passwd", PASSWD);
        rig.write("etc/group", GROUP);
        if let Some(text) = subuid {
            rig.write("etc/subuid", text);
        }
        if let Some(text) = subgid {
            rig.write("etc/subgid", text);
        }
        let passwd = rig.root.join("etc/passwd");
        let effects = if sysusers_adds.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "append={}={}",
                passwd.display(),
                esc(sysusers_adds)
            )]
        };
        let answers = rule(
            "systemd-sysusers",
            &format!("--root={} js-agent.conf", rig.root.display()),
            sysusers_code,
            "",
            &effects,
        );
        std::fs::write(rig.shim.join("answers"), format!("{answers}\n")).expect("answers");
        rig
    }

    /// A clean image: no subid file, and `systemd-sysusers` adds the account.
    fn clean(name: &str) -> Rig {
        Rig::new(name, None, None, 0, JSJOB)
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.root.join(rel), text).expect("write");
    }

    fn text(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok()
    }

    /// Runs the postinst with `arg` as dpkg does, `DPKG_ROOT` naming the root.
    fn run(&self, arg: &str) -> Run {
        let out = Command::new(real("sh"))
            .arg(repo(POSTINST))
            .arg(arg)
            .env_clear()
            .env("PATH", &self.shim)
            .env("HOST_SHIM_DIR", &self.shim)
            .env("DPKG_ROOT", &self.root)
            .envs(profile_env())
            .output()
            .expect("run sh");
        let calls = std::fs::read_to_string(self.shim.join("calls")).unwrap_or_default();
        Run {
            code: out.status.code().expect("an exit code"),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            calls: calls.lines().map(str::to_owned).collect(),
        }
    }

    /// The call the postinst makes of the stand-in.
    fn sysusers_call(&self) -> String {
        format!(
            "systemd-sysusers --root={} js-agent.conf",
            self.root.display()
        )
    }

    /// A run that exits 0, says nothing and calls `systemd-sysusers` once.
    fn ok(&self) -> Run {
        Run {
            code: 0,
            stderr: String::new(),
            calls: vec![self.sysusers_call()],
        }
    }
}

/// Asserts a clean image gets the account and the first free block in both files, after which
/// the agent's own start check finds nothing missing about the job user or its ranges, and that
/// a second run (a reinstall, an upgrade) changes nothing.
///
/// Catches a postinst that makes no range, one in one file only, or one the agent does not
/// accept; and one that adds a second range at every configure.
#[test]
fn a_clean_image_gets_the_job_user_and_a_range_the_agent_accepts_once() {
    let rig = Rig::clean("clean");
    assert_eq!(rig.job_user_refusals(), rig.missing_all());
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(rig.text("etc/subuid").as_deref(), Some(FIRST_FREE));
    assert_eq!(rig.text("etc/subgid").as_deref(), Some(FIRST_FREE));
    assert_eq!(rig.job_user_refusals(), Vec::<String>::new());
    // The stand-in adds the account again on the second run; the real one does not, and the
    // ranges, which this asserts, are what the postinst writes itself.
    rig.write("etc/passwd", &format!("{PASSWD}{JSJOB}"));
    assert_eq!(rig.run("configure").code, 0);
    assert_eq!(rig.text("etc/subuid").as_deref(), Some(FIRST_FREE));
    assert_eq!(rig.text("etc/subgid").as_deref(), Some(FIRST_FREE));
}

impl Rig {
    /// The agent's start check of the root, as `check-apt` reads it.
    fn job_user_refusals(&self) -> Vec<String> {
        job_user_refusals(&self.root)
    }

    /// The agent's refusal of a root with no job user.
    fn missing_all(&self) -> Vec<String> {
        vec![format!(
            "the agent's start check refuses the host: job user: no user \"jsjob\" in {}/etc/passwd (fix: create the account (`useradd --system --no-create-home --shell /usr/sbin/nologin jsjob`))",
            self.root.display()
        )]
    }
}

/// Asserts a range the host gives the job user already is kept, file by file: the one the setup
/// script wrote on one host and an operator copied to others, one named by the user's uid,
/// and one of more ids than the agent needs; a file that lacks one gets a block clear of the
/// other file's range.
///
/// Catches a postinst that overwrites or adds to a host's working range.
#[test]
fn a_range_the_host_has_already_is_kept_and_only_a_file_without_one_gets_a_block() {
    for (name, subuid, subgid) in [
        (
            "kept-copied",
            "jsjob:296608:65536\n",
            "jsjob:296608:65536\n",
        ),
        ("kept-by-uid", "998:300000:65536\n", "998:300000:65536\n"),
        (
            "kept-larger",
            "jsjob:100000:1000000\n",
            "jsjob:100000:65536\n",
        ),
    ] {
        let rig = Rig::new(name, Some(subuid), Some(subgid), 0, JSJOB);
        assert_eq!(rig.run("configure"), rig.ok(), "{name}");
        assert_eq!(rig.text("etc/subuid").as_deref(), Some(subuid), "{name}");
        assert_eq!(rig.text("etc/subgid").as_deref(), Some(subgid), "{name}");
        assert_eq!(rig.job_user_refusals(), Vec::<String>::new(), "{name}");
    }
    // subgid has the copied range, subuid none: subuid gets the first block clear of both.
    let rig = Rig::new("one-file", Some(""), Some("jsjob:200000:65536\n"), 0, JSJOB);
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subuid").as_deref(),
        Some("jsjob:265536:65536\n")
    );
    assert_eq!(
        rig.text("etc/subgid").as_deref(),
        Some("jsjob:200000:65536\n")
    );
    assert_eq!(rig.job_user_refusals(), Vec::<String>::new());
}

/// Asserts what is not a range the agent accepts is not kept as one: too few ids, a first id of
/// 0, a range that passes 2^32, ids that are not numbers, a line of more than three fields (a
/// fourth, or an empty one after a final `:`) and another user's range. The new
/// block is clear of every one of them that is a range.
///
/// Catches a postinst that reads a range the agent would refuse as the job user's.
#[test]
fn a_range_the_agent_would_refuse_is_not_taken_for_one() {
    let subuid = "jsjob:200000:65535\njsjob:0:65536\njsjob:4294967295:65536\njsjob:x:y\njsjob:100000:65536:extra\njsjob:100000:65536:\nalice:300000:65536\n";
    let rig = Rig::new("refused-ranges", Some(subuid), None, 0, JSJOB);
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subuid"),
        Some(format!("{subuid}jsjob:365536:65536\n"))
    );
    assert_eq!(
        rig.text("etc/subgid").as_deref(),
        Some("jsjob:365536:65536\n")
    );
    assert_eq!(rig.job_user_refusals(), Vec::<String>::new());
}

/// Asserts the block is the first from 200000 that overlaps no range of either file and holds
/// no uid and no gid: each of those moves it past itself, in whatever order they come, and a
/// number with leading zeros is read as decimal, not octal.
///
/// Catches a block that overlaps another user's range (two users would share host ids) or a
/// host account's id, and the shell reading `0200000` as an octal number.
#[test]
fn the_block_overlaps_no_range_uid_or_gid_of_the_host() {
    let subuid = "bob:265536:65536\n";
    let subgid = "alice:0200000:065536\n";
    let rig = Rig::new("clear", Some(subuid), Some(subgid), 0, JSJOB);
    rig.write(
        "etc/passwd",
        &format!("{PASSWD}carol:x:331100:331100::/home/carol:/bin/bash\n"),
    );
    rig.write("etc/group", &format!("{GROUP}dave:x:396600:\n"));
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subuid"),
        Some(format!("{subuid}jsjob:396601:65536\n"))
    );
    assert_eq!(
        rig.text("etc/subgid"),
        Some(format!("{subgid}jsjob:396601:65536\n"))
    );
    // An id just past a block and a range that ends where the block starts do not move it.
    let rig = Rig::new("touching", Some("bob:100000:100000\n"), Some(""), 0, JSJOB);
    rig.write(
        "etc/passwd",
        &format!("{PASSWD}carol:x:265536:265536::/home/carol:/bin/bash\n"),
    );
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subgid").as_deref(),
        Some("jsjob:200000:65536\n")
    );
}

/// Asserts the boundary at 2^32: a block that ends exactly there is taken, and when none fits
/// the postinst writes nothing, says why on standard error and still exits 0, so the agent's
/// start check names the missing range.
///
/// Catches a range that passes 2^32 (which the agent refuses) and an install that fails over a
/// host whose ids are all taken.
#[test]
fn a_block_ends_at_most_at_2_to_the_32_and_none_left_is_a_warning() {
    let rig = Rig::new("last-block", Some("big:1:4294901759\n"), None, 0, JSJOB);
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subgid").as_deref(),
        Some("jsjob:4294901760:65536\n")
    );
    assert_eq!(rig.job_user_refusals(), Vec::<String>::new());

    let rig = Rig::new("no-room", Some("big:1:4294901760\n"), Some(""), 0, JSJOB);
    let run = rig.run("configure");
    assert_eq!(
        run,
        Run {
            code: 0,
            stderr: NO_ROOM.to_owned(),
            calls: vec![rig.sysusers_call()],
        }
    );
    assert_eq!(
        rig.text("etc/subuid").as_deref(),
        Some("big:1:4294901760\n")
    );
    assert_eq!(rig.text("etc/subgid").as_deref(), Some(""));
}

/// Asserts a subid file whose last line has no newline gets one before the new line, so the
/// host's last range is not joined to the job user's.
///
/// Catches `alice:100000:65536jsjob:...`, which loses alice's range.
#[test]
fn a_file_without_a_final_newline_keeps_its_last_line() {
    let rig = Rig::new(
        "open-end",
        Some("alice:100000:65536"),
        Some("alice:100000:65536\n"),
        0,
        JSJOB,
    );
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subuid").as_deref(),
        Some("alice:100000:65536\njsjob:200000:65536\n")
    );
    assert_eq!(
        rig.text("etc/subgid").as_deref(),
        Some("alice:100000:65536\njsjob:200000:65536\n")
    );
}

/// Asserts the account is found by its line in passwd when that line is the last and has no
/// newline: a range named by its uid is then the job user's.
///
/// Catches a last passwd line that the read loop drops.
#[test]
fn the_job_users_uid_is_read_from_a_last_line_without_newline() {
    let rig = Rig::new(
        "passwd-open-end",
        Some("998:300000:65536\n"),
        Some("998:300000:65536\n"),
        0,
        "",
    );
    rig.write("etc/passwd", &format!("{PASSWD}{}", JSJOB.trim_end()));
    assert_eq!(rig.run("configure"), rig.ok());
    assert_eq!(
        rig.text("etc/subuid").as_deref(),
        Some("998:300000:65536\n")
    );
}

/// Asserts the steps the postinst acts at: `configure` and the three abort steps that put a
/// configured package back each make the account and the ranges; any other argument does
/// nothing at all.
///
/// Catches a postinst that runs at no step, or at one where dpkg does not expect it to change
/// the system.
#[test]
fn only_configure_and_the_abort_steps_act() {
    for arg in [
        "configure",
        "abort-upgrade",
        "abort-deconfigure",
        "abort-remove",
    ] {
        let rig = Rig::clean(arg);
        assert_eq!(rig.run(arg), rig.ok(), "{arg}");
        assert_eq!(rig.text("etc/subuid").as_deref(), Some(FIRST_FREE), "{arg}");
    }
    for arg in ["triggered", ""] {
        let rig = Rig::clean(&format!("other-{arg}"));
        assert_eq!(
            rig.run(arg),
            Run {
                code: 0,
                stderr: String::new(),
                calls: Vec::new()
            },
            "{arg}"
        );
        assert_eq!(rig.text("etc/subuid"), None, "{arg}");
        assert_eq!(rig.text("etc/subgid"), None, "{arg}");
    }
}

/// Asserts a `systemd-sysusers` that fails fails the postinst (dpkg then leaves the package
/// unconfigured and apt says so) and no range is written for an account that was not made.
///
/// Catches a postinst that goes on, or exits 0, when the account could not be made.
#[test]
fn a_sysusers_failure_fails_the_postinst_before_any_range() {
    let rig = Rig::new("sysusers-fails", None, None, 1, "");
    let run = rig.run("configure");
    assert_eq!(run.code, 1);
    assert_eq!(run.calls, [rig.sysusers_call()]);
    assert_eq!(rig.text("etc/subuid"), None);
    assert_eq!(rig.text("etc/subgid"), None);
}

/// The postinst is the package's own file: the test runs the text `release deb` packs.
#[test]
fn the_postinst_run_here_is_the_one_the_package_carries() {
    assert_eq!(
        std::fs::read_to_string(repo(POSTINST)).expect("postinst"),
        xtask::release::deb::POSTINST
    );
    assert!(Path::new(SHIM).is_file());
}
