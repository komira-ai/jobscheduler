//! The first live launch: one GPU instance through the provider, with limits.
//!
//! Dry run by default. Run it with no flag but the required ones to see the plan; run it
//! again with `--go --plan-hash <hash printed by the dry run>` to launch. See
//! `js_provider_lambdalabs::first_launch` for what each flag does. Flags:
//!
//! ```text
//! --base-url URL            required: scheme://host[:port] of the API
//! --key-file PATH           required: the API key, in a file only its owner can use
//! --key-name NAME           the name of the ssh key the account holds (dry run and --go)
//! --state-file PATH         where to record the instance (dry run and --go)
//! --ca-bundle PATH          PEM roots to trust, for an https base URL
//! --instance-type NAME      launch this type instead of the cheapest with capacity
//! --region NAME             launch in this region
//! --boot-deadline-secs N    give up waiting for active after N seconds (900)
//! --wall-cap-secs N         tear down N seconds after the launch (1800)
//! --spend-cap-cents N       tear down when the price times the time reaches N (100)
//! --ssh-key PATH --ssh-user USER [--ssh-command CMD]
//!                           run CMD (nvidia-smi) over ssh once active
//! --go --plan-hash HASH     launch the plan the dry run printed
//! --teardown-only FILE      terminate what a state file names
//! ```
//!
//! SIGINT, SIGTERM and SIGHUP do not kill the process: they set a flag the runner reads on
//! every wait iteration and around the ssh step. After a launch it then terminates the
//! instance (bounded tries, read back) and exits 1; before a launch it launches nothing.
//! Only SIGKILL leaves an instance behind, which is why the teardown command is printed
//! as soon as the state file exists.
//!
//! This file only wires the real client, clock, sleep and ssh into the library module,
//! where the behaviour is tested against an in-memory account.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use js_provider_lambdalabs::first_launch::{Remote, Seams, Sleeper, run};
use js_provider_lambdalabs::{ApiKey, HttpApi, LambdaApi, SystemClock};

/// Each API call is answered or fails within this long.
const CALL_LIMIT: Duration = Duration::from_secs(30);

/// Set by the signal handler; read by the runner through its `stop` seam.
static STOP: AtomicBool = AtomicBool::new(false);

/// The handler for SIGINT, SIGTERM and SIGHUP. An atomic store is async-signal-safe and
/// is all it does.
extern "C" fn on_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Installs [`on_signal`] for the three signals that end a session, so that none of them
/// kills the process while an instance is live: the runner sees the flag and tears down.
fn install_handlers() -> Result<(), String> {
    for (signal, name) in [
        (libc::SIGINT, "SIGINT"),
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGHUP, "SIGHUP"),
    ] {
        // SAFETY: `on_signal` is an `extern "C"` function that only stores to an atomic,
        // which is async-signal-safe, and `signal` is called with a valid signal number.
        let handler: extern "C" fn(libc::c_int) = on_signal;
        let previous = unsafe { libc::signal(signal, handler as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(format!("cannot handle {name}"));
        }
    }
    Ok(())
}

/// Sleeps `total` in short steps, ending early once a stop is requested.
fn nap(total: Duration, stop: &dyn Fn() -> bool) {
    let step = Duration::from_millis(200);
    let started = Instant::now();
    while started.elapsed() < total && !stop() {
        thread::sleep(step.min(total));
    }
}

struct RealSleep;

impl Sleeper for RealSleep {
    fn sleep(&self, how_long: Duration) {
        thread::sleep(how_long);
    }
}

struct Ssh;

impl Ssh {
    /// One attempt: the exit code (None when it ran out of time) and the output.
    fn once(
        host: &str,
        user: &str,
        identity: &Path,
        command: &str,
        limit: Duration,
        stop: &dyn Fn() -> bool,
    ) -> Result<(Option<i32>, String), String> {
        let mut child = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes"])
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .args(["-o", "ConnectTimeout=20"])
            .arg("-i")
            .arg(identity)
            .arg(format!("{user}@{host}"))
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("ssh cannot start: {}", err.kind()))?;
        let stdout = child.stdout.take();
        let reader = thread::spawn(move || {
            let mut text = String::new();
            if let Some(out) = stdout {
                let _ = out.take(64 * 1024).read_to_string(&mut text);
            }
            text
        });
        let started = Instant::now();
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) if !stop() && started.elapsed() < limit => {
                    thread::sleep(Duration::from_millis(200));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let text = reader.join().unwrap_or_default();
        Ok((code, text))
    }
}

impl Remote for Ssh {
    fn run(
        &self,
        host: &str,
        user: &str,
        identity: &Path,
        command: &str,
        limit: Duration,
        stop: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        let started = Instant::now();
        loop {
            let left = limit.saturating_sub(started.elapsed());
            let (code, text) = Self::once(host, user, identity, command, left, stop)?;
            match code {
                Some(0) => return Ok(text),
                // 255 is ssh's own failure, such as sshd not up yet: try again while
                // there is time.
                Some(255) if !stop() && started.elapsed() + Duration::from_secs(30) < limit => {
                    nap(Duration::from_secs(10), stop);
                }
                Some(code) => return Err(format!("the command ended with status {code}")),
                None if stop() => return Err("the command was stopped".to_owned()),
                None => return Err("the command ran out of time".to_owned()),
            }
        }
    }
}

fn main() {
    if let Err(why) = install_handlers() {
        println!("error: {why}");
        println!("STOP: nothing was launched; paste back this output.");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wall = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    };
    let connect = |base: &str, key: &ApiKey, ca: Option<&Path>| {
        HttpApi::connect(base, key, ca, CALL_LIMIT).map(|api| Arc::new(api) as Arc<dyn LambdaApi>)
    };
    let stop = || STOP.load(Ordering::SeqCst);
    let seams = Seams {
        clock: Arc::new(SystemClock::new()),
        sleeper: &RealSleep,
        remote: &Ssh,
        wall_unix: &wall,
        connect: &connect,
        stop: &stop,
    };
    let status = run(&args, &seams, &mut std::io::stdout());
    let _ = std::io::stdout().flush();
    std::process::exit(status);
}
