//! Is a native job refused every place that only the privacy controls (TCC) guard from it,
//! though it inherits the Full Disk Access of the agent? Run for real, as root, on a Mac.
//! (komira-ai/jobscheduler#1020; the places are those of komira-ai/jobscheduler#1025.)
//!
//! Why it is asked: a native job keeps the agent's responsibility for the privacy controls
//! (it is started without the disclaim), so on a Mac whose agent holds Full Disk Access the
//! job holds it too, and Unix modes are all that is left to keep it out of a place that only
//! the privacy controls guard. The profile of every native run therefore read-denies those
//! places itself (`js_agent::native::fence::PRIVACY_ONLY_PLACES`), root's home and every other
//! home, and denies hard links and clones of a denied path (a read deny matches paths, so a
//! link or a clone of a denied file in the job's own directory would be a path no deny names).
//! This test measures that the profile does it.
//!
//! The test process stands in for the agent, as `tests/agent/native/disclaim.rs` explains:
//! what answers for the agent answers for the test's responsible process. So the test must be
//! run from a session whose responsible application holds Full Disk Access (the operator
//! runs it through `cargo xtask mac-root-tests`). Four processes each execute THIS binary in
//! the role `probe` (`FENCE_ROLE=probe`, the places in `FENCE_PLACES` as `name=path` lines)
//! and print one line per place and one about themselves (`FENCE place <name> <open|refused
//! <errno>|absent|failed <errno>>`, `FENCE who pid=.. responsible=..`). A directory is
//! probed by listing it, a file by opening it and reading up to 16 bytes (never printed):
//!
//! 1. root, the test's ordinary child, no profile: it must open every place. It shows the
//!    grant reaches the test and the place is there.
//! 2. the slot's uid, started the ordinary way (counted as the test), no profile: it must
//!    open the six privacy-only places. It shows the grant reaches a process of another uid,
//!    so the job's refusal is not just a uid's.
//! 3. the slot's uid, disclaimed (`js_mac_session::disclaim::Program`), no profile: it must
//!    open none of the six. It shows the grant is what lets 2 in, and that the job could not
//!    hold it without keeping the agent's responsibility.
//! 4. the job: a real native run through the agent's `NativeRuntime` and the real supervisor
//!    (the responsibility is kept) under the production profile, run as the slot's account.
//!    It probes the same places, then, in its working directory, makes a hard link and a
//!    clone (`clonefileat(2)`) of a file it wrote itself, which must open (its controls: the
//!    job can link and clone there), and of the privacy database, which must be refused.
//!
//! The places: the six privacy-only places, root's home (`/private/var/root` and `/var/root`),
//! a file of the invoking user with mode 0600 in a 0755 directory, a home of another user made
//! below the pool's homes with open modes (0755 and 0644: only the profile closes it), and the
//! invoking user's `Documents`, `Desktop`, `Library/Mail`, `Library/Messages` and
//! `Library/Safari` (one root finds absent is not on this Mac and is left out), and three
//! other paths to the privacy database that no read deny names as written: a symlink the test
//! made, the lower-case spelling, and `/.vol/<device>/<inode>`.
//!
//! The verdict (`verdict.rs`) is exactly one final line: `fence: PASS`, `fence: FAIL <names>`
//! or `fence: INCONCLUSIVE <reasons>`. It fails when the job opened any place, or a link
//! or clone of the database, whatever the controls say. It is inconclusive when a control failed: root did not open
//! every place; the counted slot process did not open the six privacy-only places; the
//! disclaimed slot process opened one of them (its other places are left to their modes, which
//! may open the other user's home); a process's who line or responsible pid is missing; 1, 2
//! and the job do not answer for the test's responsible process, or 3 does not answer for
//! itself; the file the job wrote in its run directory is missing or not owned by the slot's
//! uid (the test reads its owner before the run is removed); the job's answer to a place root
//! opened is not a refusal; the job could not link or clone its own file; or its link or clone of the
//! database was not refused. Only a pass passes: the test asserts the verdict, so it never passes on a failed
//! control.
//!
//! Nothing asserts on elapsed time.
//!
//! No pull request job runs it (rule B9). It needs root, a Mac whose process chain holds Full
//! Disk Access and a free uid, makes an account, and removes it: never run it on a production
//! host. Run it as root with `JS_NATIVE_FENCE_ROOT` naming a new directory, owned by root and
//! mode 0755, that every user can search down to, `JS_NATIVE_FENCE_UID` naming the one slot's
//! uid (500 to 60000) that nothing uses, and `JS_NATIVE_FENCE_OWNER_UID` naming the uid of the
//! user who runs the test (`JS_NATIVE_FENCE_OWNER_HOME` names that user's home when `id -P`
//! cannot say), with `--ignored --test-threads=1 --nocapture`. These are test-binary
//! parameters for the operator.
//!
//! One difference from production: the runtime here has the directory of this test binary as
//! an extra tool root (the job executes the binary).
//!
//! The file builds on every platform, so clippy reads it on Linux and the pure part (the
//! verdict) is tested there. Run anywhere but macOS, the ignored test stops at its first line.

#[cfg(target_os = "macos")]
mod mac;
mod verdict;

use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::Path;

use verdict::{Answer, Verdict, Who, final_line};

/// The environment name of the child's role; it does not start with a name the agent reserves.
const ROLE: &str = "FENCE_ROLE";
/// The environment name of the places the probe tries: one `name=path` per line.
const PLACES: &str = "FENCE_PLACES";
/// The environment name that makes the probe also try a hard link and a clone of this path in
/// its working directory (set for the job only).
const LINK_TARGET: &str = "FENCE_LINK_TARGET";

/// What a process does when the test starts this binary as it: nothing, unless the role says.
/// Not `#[ignore]`: it is a no-op without the role, and the processes name it exactly.
#[test]
fn child_of_the_runs() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    match role.as_str() {
        "probe" => probe(),
        other => panic!("unknown role {other:?}"),
    }
}

/// The pid of the process that `pid` answers to for the privacy controls, `None` when the
/// system cannot say (and always off macOS, which has no such answer).
#[cfg(target_os = "macos")]
fn responsible(pid: u32) -> Option<u32> {
    js_mac_session::disclaim::responsible_pid(pid).ok()
}

#[cfg(not(target_os = "macos"))]
fn responsible(_pid: u32) -> Option<u32> {
    None
}

/// Tries `path`: a directory is listed, a file is opened and up to 16 bytes of it read.
fn attempt(path: &Path) -> io::Result<()> {
    let file = File::open(path)?;
    if file.metadata()?.is_dir() {
        for entry in fs::read_dir(path)? {
            entry?;
        }
    } else {
        let mut head = Vec::new();
        file.take(16).read_to_end(&mut head)?;
    }
    Ok(())
}

/// A clone of `target` named `to` in the working directory, through the tree store's safe
/// binding of `clonefileat(2)` (`js_treestore::mac::Clonefile`, which clones a file as well as
/// a directory). Its source is a path, so the clone is tried without a read of the source
/// first (a tool such as `cp -c` reads the source's metadata first, and so would be refused
/// before it tried the clone).
#[cfg(target_os = "macos")]
fn clone_file(target: &str, to: &str) -> io::Result<()> {
    use js_treestore::mac::{Clonefile, Cloner as _};
    use std::os::fd::AsFd as _;
    let here = File::open(".")?;
    Clonefile.clone_dir(Path::new(target), here.as_fd(), std::ffi::OsStr::new(to))
}

/// No clone off macOS (`clonefile` is the APFS call): the probe runs on a Mac only.
#[cfg(not(target_os = "macos"))]
fn clone_file(_target: &str, _to: &str) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// The answer to a hard link of `target` named `to` and a read through it.
fn linked(target: &str, to: &str) -> Answer {
    Answer::from_io(&fs::hard_link(target, to).and_then(|()| attempt(Path::new(to))))
}

/// The answer to a clone of `target` named `to` and a read of the clone.
fn cloned(target: &str, to: &str) -> Answer {
    Answer::from_io(&clone_file(target, to).and_then(|()| attempt(Path::new(to))))
}

/// The `probe` role: prints the answer for every place of `FENCE_PLACES`; then, when
/// `FENCE_LINK_TARGET` is set, for a hard link and a clone of a file it writes itself (its
/// controls) and of that path, all in its working directory; then who it is. It starts with an
/// empty line, so its first line is not glued to libtest's `test <name> ... `.
fn probe() {
    println!();
    let places = std::env::var(PLACES).unwrap_or_else(|_| panic!("{PLACES} is not set"));
    for line in places.lines() {
        let (name, path) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{PLACES} line {line:?} is not name=path"));
        let answer = Answer::from_io(&attempt(Path::new(path)));
        println!("{}", verdict::place_line(name, &answer));
    }
    if let Ok(target) = std::env::var(LINK_TARGET) {
        let (link, clone) = match fs::write("fence-own", "x") {
            Ok(()) => (
                linked("fence-own", "fence-own-hardlink"),
                cloned("fence-own", "fence-own-clone"),
            ),
            Err(why) => {
                let answer = Answer::from_io(&Err(why));
                (answer, answer)
            }
        };
        println!("{}", verdict::place_line("control-hardlink", &link));
        println!("{}", verdict::place_line("control-clone", &clone));
        println!(
            "{}",
            verdict::place_line("evasion-hardlink", &linked(&target, "fence-hardlink"))
        );
        println!(
            "{}",
            verdict::place_line("evasion-clone", &cloned(&target, "fence-clone"))
        );
    }
    let pid = std::process::id();
    let who = Who {
        pid,
        responsible: responsible(pid),
    };
    println!("{}", verdict::who_line(&who));
}

/// Asserts that a job, which inherits the agent's Full Disk Access, is refused every place that
/// only the privacy controls guard (the six privacy-only places, root's home, another user's
/// private file and open home, the invoking user's guarded folders) and opens none through a
/// symlink, the lower-case spelling, `/.vol`, a hard link or a clone, while root and a counted
/// slot process open them and a disclaimed slot process opens none of the six. It fails when the
/// job opened anything and when a control failed, so it never passes without having shown the
/// grant reaches the test. Catches a profile that leaves a privacy-only place or a home to the
/// grant, a spelling or a link that gets round a read deny, and a run whose controls prove
/// nothing.
#[test]
#[ignore = "needs root, a Mac whose process chain holds Full Disk Access, and a free uid; creates an account"]
fn a_job_holding_the_grant_is_refused_every_place_only_the_privacy_controls_guard() {
    let verdict = run_on_a_mac();
    assert_eq!(verdict, Verdict::Pass, "{}", final_line(&verdict));
}

#[cfg(target_os = "macos")]
fn run_on_a_mac() -> Verdict {
    mac::run()
}

#[cfg(not(target_os = "macos"))]
fn run_on_a_mac() -> Verdict {
    panic!("this test runs on macOS only")
}
