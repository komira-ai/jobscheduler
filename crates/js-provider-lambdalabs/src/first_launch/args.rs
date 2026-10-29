//! The flags of the first-launch runner.

use std::path::PathBuf;
use std::time::Duration;

/// What the flags ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Args {
    pub base_url: String,
    pub key_file: PathBuf,
    pub ca_bundle: Option<PathBuf>,
    pub key_name: Option<String>,
    pub instance_type: Option<String>,
    pub region: Option<String>,
    pub state_file: Option<PathBuf>,
    pub boot_deadline: Duration,
    pub wall_cap: Duration,
    pub spend_cap_cents: u64,
    pub mode: Mode,
}

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mode {
    /// Print the plan and stop.
    DryRun,
    /// Launch the plan whose hash this is.
    Go { plan_hash: Option<String> },
    /// Terminate what the state file names.
    Teardown { state_file: PathBuf },
}

const DEFAULT_BOOT_SECS: u64 = 15 * 60;
const DEFAULT_WALL_SECS: u64 = 30 * 60;
const DEFAULT_SPEND_CENTS: u64 = 100;
/// Why the ssh flags are refused: the launch carries the ruleset that lets nothing in.
const NO_SSH: &str = "has no effect and is refused: the launch has no inbound access, so nothing can connect over ssh";

/// Reads `args` (without the program name). The error is a sentence for the operator;
/// it never quotes an argument that is not a flag name.
pub(super) fn parse(args: &[String]) -> Result<Args, String> {
    let mut found = Found::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let name = flag.strip_prefix("--").ok_or("unexpected argument")?;
        match name {
            "go" => found.go = true,
            "ssh-key" | "ssh-user" | "ssh-command" => {
                return Err(format!("--{name} {NO_SSH}"));
            }
            _ => {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                found.set(name, value)?;
            }
        }
    }
    found.finish()
}

#[derive(Default)]
struct Found {
    go: bool,
    base_url: Option<String>,
    key_file: Option<PathBuf>,
    ca_bundle: Option<PathBuf>,
    key_name: Option<String>,
    instance_type: Option<String>,
    region: Option<String>,
    state_file: Option<PathBuf>,
    boot: Option<u64>,
    wall: Option<u64>,
    spend: Option<u64>,
    plan_hash: Option<String>,
    teardown_only: Option<PathBuf>,
}

fn number(name: &str, value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("--{name} must be a whole number above zero")),
    }
}

impl Found {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let text = || value.to_owned();
        let path = || PathBuf::from(value);
        match name {
            "base-url" => self.base_url = Some(text()),
            "key-file" => self.key_file = Some(path()),
            "ca-bundle" => self.ca_bundle = Some(path()),
            "key-name" => self.key_name = Some(text()),
            "instance-type" => self.instance_type = Some(text()),
            "region" => self.region = Some(text()),
            "state-file" => self.state_file = Some(path()),
            "boot-deadline-secs" => self.boot = Some(number(name, value)?),
            "wall-cap-secs" => self.wall = Some(number(name, value)?),
            "spend-cap-cents" => self.spend = Some(number(name, value)?),
            "plan-hash" => self.plan_hash = Some(text()),
            "teardown-only" => self.teardown_only = Some(path()),
            _ => return Err(format!("unknown flag --{name}")),
        }
        Ok(())
    }

    fn finish(self) -> Result<Args, String> {
        let base_url = self.base_url.ok_or("--base-url is required")?;
        let key_file = self.key_file.ok_or("--key-file is required")?;
        let mode = match (self.teardown_only, self.go, self.plan_hash) {
            (Some(state_file), false, None) => Mode::Teardown { state_file },
            (Some(_), _, _) => return Err("--teardown-only stands alone".to_owned()),
            (None, true, plan_hash) => Mode::Go { plan_hash },
            (None, false, None) => Mode::DryRun,
            (None, false, Some(_)) => return Err("--plan-hash goes with --go".to_owned()),
        };
        Ok(Args {
            base_url,
            key_file,
            ca_bundle: self.ca_bundle,
            key_name: self.key_name,
            instance_type: self.instance_type,
            region: self.region,
            state_file: self.state_file,
            boot_deadline: Duration::from_secs(self.boot.unwrap_or(DEFAULT_BOOT_SECS)),
            wall_cap: Duration::from_secs(self.wall.unwrap_or(DEFAULT_WALL_SECS)),
            spend_cap_cents: self.spend.unwrap_or(DEFAULT_SPEND_CENTS),
            mode,
        })
    }
}
