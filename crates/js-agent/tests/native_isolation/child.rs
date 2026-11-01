//! What run A leaves behind and what run B looks for. Both run as the pool user, inside
//! the Seatbelt profile of the product, through the `child_of_the_runs` test of this
//! binary. An item the profile (or the machine) refuses is reported as refused with the
//! reason, never silently skipped: a table of only refusals proves nothing.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::lines::{Kind, Line, Verdict};
use super::token::Token;

// The calls libc has no declaration of on macOS. All take C strings or plain integers.
unsafe extern "C" {
    fn msgget(key: c_int, flags: c_int) -> c_int;
    fn msgctl(id: c_int, cmd: c_int, buf: *mut c_void) -> c_int;
    fn sem_open(name: *const c_char, oflag: c_int, ...) -> *mut c_void;
    fn sem_close(sem: *mut c_void) -> c_int;
    fn sem_unlink(name: *const c_char) -> c_int;
    fn shm_unlink(name: *const c_char) -> c_int;
}

const IPC_CREAT: c_int = 0o1000;
const IPC_EXCL: c_int = 0o2000;
const IPC_RMID: c_int = 0;
/// `SEM_FAILED`: `(sem_t *)-1`.
const SEM_FAILED: *mut c_void = usize::MAX as *mut c_void;
/// The prefix of the POSIX names (macOS holds a name to 31 bytes).
const POSIX_PREFIX: &str = "jsi";
const DEFAULTS_KEY: &str = "isolation";

/// Where the pool user is.
pub struct Spot {
    pub home: PathBuf,
    pub tmp: PathBuf,
    pub uid: u32,
}

/// The tool's standard output, or why it failed.
fn tool(program: &str, args: &[&str], home: &Path) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .env("HOME", home)
        .output()
        .map_err(|why| format!("{program}: {why}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {}: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn errno(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn cstring(name: &str) -> CString {
    CString::new(name).expect("a name without NUL")
}

fn key_of(locator: &str) -> Option<c_int> {
    locator.strip_prefix("key=")?.parse().ok()
}

fn file_at(path: &Path, token: &Token) -> Result<String, String> {
    std::fs::write(path, token.as_str()).map_err(|why| format!("{}: {why}", path.display()))?;
    Ok(path.display().to_string())
}

/// One item of `kind`, carrying `token`; its locator, or why it could not be made.
fn seed(kind: Kind, token: &Token, spot: &Spot) -> Result<String, String> {
    let tok = token.as_str();
    match kind {
        Kind::HomeFile => file_at(&spot.home.join(format!("iso-{tok}")), token),
        Kind::ScratchFile => file_at(&spot.tmp.join(format!("iso-{tok}")), token),
        Kind::SystemTempFile => {
            file_at(&Path::new("/private/tmp").join(format!("iso-{tok}")), token)
        }
        Kind::Process => {
            // Detached: its own session, no terminal, the token in its command line.
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "/bin/sleep 600", tok])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: `setsid` is async-signal-safe and touches no memory of ours.
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let child = command.spawn().map_err(|why| format!("/bin/sh: {why}"))?;
            Ok(format!("pid={}", child.id()))
        }
        Kind::Keychain => {
            let path = spot.home.join(format!("iso-{tok}.keychain-db"));
            let shown = path.display().to_string();
            tool(
                "/usr/bin/security",
                &["create-keychain", "-p", tok, &shown],
                &spot.home,
            )?;
            tool(
                "/usr/bin/security",
                &[
                    "add-generic-password",
                    "-a",
                    tok,
                    "-s",
                    tok,
                    "-w",
                    tok,
                    &shown,
                ],
                &spot.home,
            )?;
            Ok(shown)
        }
        Kind::Defaults => {
            let domain = format!("com.komira.iso.{tok}");
            tool(
                "/usr/bin/defaults",
                &["write", &domain, DEFAULTS_KEY, tok],
                &spot.home,
            )?;
            Ok(domain)
        }
        Kind::LaunchAgent => {
            let label = format!("com.komira.iso.{tok}");
            let path = spot.home.join(format!("iso-{tok}.plist"));
            std::fs::write(&path, agent_plist(&label))
                .map_err(|why| format!("{}: {why}", path.display()))?;
            let domain = format!("user/{}", spot.uid);
            tool(
                "/bin/launchctl",
                &["bootstrap", &domain, &path.display().to_string()],
                &spot.home,
            )?;
            Ok(label)
        }
        Kind::SysvShm => {
            // SAFETY: plain integers; nothing of ours is touched.
            let id = unsafe { libc::shmget(token.sysv_key(), 4096, IPC_CREAT | IPC_EXCL | 0o600) };
            if id < 0 {
                return Err(errno("shmget"));
            }
            Ok(format!("key={}", token.sysv_key()))
        }
        Kind::SysvSem => {
            // SAFETY: plain integers.
            let id = unsafe { libc::semget(token.sysv_key(), 1, IPC_CREAT | IPC_EXCL | 0o600) };
            if id < 0 {
                return Err(errno("semget"));
            }
            Ok(format!("key={}", token.sysv_key()))
        }
        Kind::SysvMsg => {
            // SAFETY: plain integers.
            let id = unsafe { msgget(token.sysv_key(), IPC_CREAT | IPC_EXCL | 0o600) };
            if id < 0 {
                return Err(errno("msgget"));
            }
            Ok(format!("key={}", token.sysv_key()))
        }
        Kind::PosixShm => {
            let name = token.posix_name(POSIX_PREFIX);
            let c = cstring(&name);
            // SAFETY: `c` is a NUL-terminated string that outlives the call; the mode is
            // passed as the unsigned int a variadic call promotes it to.
            let fd = unsafe {
                libc::shm_open(
                    c.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o600 as c_uint,
                )
            };
            if fd < 0 {
                return Err(errno("shm_open"));
            }
            // SAFETY: `fd` is the descriptor just opened. The object outlives it.
            unsafe { libc::close(fd) };
            Ok(name)
        }
        Kind::PosixSem => {
            let name = token.posix_name(POSIX_PREFIX);
            let c = cstring(&name);
            // SAFETY: as above; `sem_open` with `O_CREAT` takes a mode and a value, both
            // unsigned ints.
            let sem = unsafe {
                sem_open(
                    c.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL,
                    0o600 as c_uint,
                    0 as c_uint,
                )
            };
            if sem == SEM_FAILED {
                return Err(errno("sem_open"));
            }
            // SAFETY: `sem` was just returned by `sem_open`.
            unsafe { sem_close(sem) };
            Ok(name)
        }
    }
}

fn agent_plist(label: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{label}</string>\n\
         <key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         </dict></plist>\n"
    )
}

/// Run A: one item of every kind, each with a token of its own.
pub fn seed_all(spot: &Spot) -> Vec<Line> {
    Kind::ALL
        .into_iter()
        .map(|kind| {
            let token = Token::fresh();
            match seed(kind, &token, spot) {
                Ok(locator) => Line::new(Verdict::Seeded, kind, token, &locator),
                Err(why) => Line::new(Verdict::Refused, kind, token, &why),
            }
        })
        .collect()
}

/// Run B's look for one seeded item, as the same user (with `home` as its HOME): where it
/// is, if it is. A process
/// and a launch agent are looked for by the test, in `ps` and `launchctl print`.
pub fn probe(seeded: &Line, home: &Path) -> Option<String> {
    let locator = seeded.text.as_str();
    let tok = seeded.token.as_str();
    match seeded.kind {
        Kind::HomeFile | Kind::ScratchFile | Kind::SystemTempFile => {
            Path::new(locator).exists().then(|| locator.to_owned())
        }
        Kind::Process | Kind::LaunchAgent => None,
        Kind::Keychain => {
            if Path::new(locator).exists() {
                return Some(locator.to_owned());
            }
            tool(
                "/usr/bin/security",
                &["find-generic-password", "-s", tok],
                home,
            )
            .ok()
            .map(|_| format!("a generic password for {tok} is in the keychain search list"))
        }
        Kind::Defaults => tool("/usr/bin/defaults", &["read", locator, DEFAULTS_KEY], home)
            .ok()
            .filter(|value| value.trim() == tok)
            .map(|_| format!("{locator} {DEFAULTS_KEY} reads back")),
        Kind::SysvShm => {
            let key = key_of(locator)?;
            // SAFETY: plain integers; size 0 and no flags only look.
            let id = unsafe { libc::shmget(key, 0, 0) };
            (id >= 0).then(|| format!("{locator} id={id}"))
        }
        Kind::SysvSem => {
            let key = key_of(locator)?;
            // SAFETY: plain integers.
            let id = unsafe { libc::semget(key, 0, 0) };
            (id >= 0).then(|| format!("{locator} id={id}"))
        }
        Kind::SysvMsg => {
            let key = key_of(locator)?;
            // SAFETY: plain integers.
            let id = unsafe { msgget(key, 0) };
            (id >= 0).then(|| format!("{locator} id={id}"))
        }
        Kind::PosixShm => {
            let c = cstring(locator);
            // SAFETY: `c` is a NUL-terminated string that outlives the call.
            let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0 as c_uint) };
            if fd < 0 {
                return None;
            }
            // SAFETY: `fd` was just opened.
            unsafe { libc::close(fd) };
            Some(locator.to_owned())
        }
        Kind::PosixSem => {
            let c = cstring(locator);
            // SAFETY: `c` is a NUL-terminated string; without `O_CREAT` no more arguments.
            let sem = unsafe { sem_open(c.as_ptr(), 0) };
            if sem == SEM_FAILED {
                return None;
            }
            // SAFETY: `sem` was just returned by `sem_open`.
            unsafe { sem_close(sem) };
            Some(locator.to_owned())
        }
    }
}

/// Run B: every seeded item looked for, a `Found` line for each that is there.
pub fn look_all(seeded: &[Line], home: &Path) -> Vec<Line> {
    seeded
        .iter()
        .filter(|line| line.verdict == Verdict::Seeded)
        .filter_map(|line| {
            let place = probe(line, home)?;
            Some(Line::new(
                Verdict::Found,
                line.kind,
                line.token.clone(),
                &place,
            ))
        })
        .collect()
}

/// Removes what a seeded item left, as root, after the test has looked: so a failed run
/// does not leave objects on the Mac that the next run would meet. Best effort; a
/// process is ended by the helper's kill of the user, a launch agent by the bootout.
pub fn remove(seeded: &Line) {
    let locator = seeded.text.as_str();
    match seeded.kind {
        Kind::HomeFile | Kind::ScratchFile | Kind::SystemTempFile | Kind::Keychain => {
            let _ = std::fs::remove_file(locator);
        }
        Kind::Process | Kind::LaunchAgent | Kind::Defaults => {}
        Kind::SysvShm => {
            if let Some(key) = key_of(locator) {
                // SAFETY: plain integers; `IPC_RMID` takes no buffer.
                unsafe {
                    let id = libc::shmget(key, 0, 0);
                    if id >= 0 {
                        libc::shmctl(id, IPC_RMID, std::ptr::null_mut());
                    }
                }
            }
        }
        Kind::SysvSem => {
            if let Some(key) = key_of(locator) {
                // SAFETY: plain integers.
                unsafe {
                    let id = libc::semget(key, 0, 0);
                    if id >= 0 {
                        libc::semctl(id, 0, IPC_RMID);
                    }
                }
            }
        }
        Kind::SysvMsg => {
            if let Some(key) = key_of(locator) {
                // SAFETY: plain integers; `IPC_RMID` takes no buffer.
                unsafe {
                    let id = msgget(key, 0);
                    if id >= 0 {
                        msgctl(id, IPC_RMID, std::ptr::null_mut());
                    }
                }
            }
        }
        Kind::PosixShm => {
            let c = cstring(locator);
            // SAFETY: `c` is a NUL-terminated string that outlives the call.
            unsafe { shm_unlink(c.as_ptr()) };
        }
        Kind::PosixSem => {
            let c = cstring(locator);
            // SAFETY: as above.
            unsafe { sem_unlink(c.as_ptr()) };
        }
    }
}
