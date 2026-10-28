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
//! This file only wires the real client, clock, sleep and ssh into the library module,
//! where the behaviour is tested against an in-memory account.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use js_provider_lambdalabs::first_launch::{Remote, Seams, Sleeper, run};
use js_provider_lambdalabs::{ApiKey, HttpApi, LambdaApi, SystemClock};

/// Each API call is answered or fails within this long.
const CALL_LIMIT: Duration = Duration::from_secs(30);

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
                Ok(None) if started.elapsed() < limit => thread::sleep(Duration::from_millis(200)),
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
    ) -> Result<String, String> {
        let started = Instant::now();
        loop {
            let left = limit.saturating_sub(started.elapsed());
            let (code, text) = Self::once(host, user, identity, command, left)?;
            match code {
                Some(0) => return Ok(text),
                // 255 is ssh's own failure, such as sshd not up yet: try again while
                // there is time.
                Some(255) if started.elapsed() + Duration::from_secs(30) < limit => {
                    thread::sleep(Duration::from_secs(10));
                }
                Some(code) => return Err(format!("the command ended with status {code}")),
                None => return Err("the command ran out of time".to_owned()),
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wall = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    };
    let connect = |base: &str, key: &ApiKey, ca: Option<&Path>| {
        HttpApi::connect(base, key, ca, CALL_LIMIT).map(|api| Arc::new(api) as Arc<dyn LambdaApi>)
    };
    let seams = Seams {
        clock: Arc::new(SystemClock::new()),
        sleeper: &RealSleep,
        remote: &Ssh,
        wall_unix: &wall,
        connect: &connect,
    };
    let status = run(&args, &seams, &mut std::io::stdout());
    let _ = std::io::stdout().flush();
    std::process::exit(status);
}
