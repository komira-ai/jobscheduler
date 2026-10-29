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
    pub ssh: Option<Ssh>,
    pub mode: Mode,
}

/// The remote command to run once the instance is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ssh {
    pub identity: PathBuf,
    pub user: String,
    pub command: String,
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
const DEFAULT_COMMAND: &str = "nvidia-smi";

/// Reads `args` (without the program name). The error is a sentence for the operator;
/// it never quotes an argument that is not a flag name.
pub(super) fn parse(args: &[String]) -> Result<Args, String> {
    let mut found = Found::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let name = flag.strip_prefix("--").ok_or("unexpected argument")?;
        match name {
            "go" => found.go = true,
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
    ssh_key: Option<PathBuf>,
    ssh_user: Option<String>,
    ssh_command: Option<String>,
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
            "ssh-key" => self.ssh_key = Some(path()),
            "ssh-user" => self.ssh_user = Some(text()),
            "ssh-command" => self.ssh_command = Some(text()),
            "plan-hash" => self.plan_hash = Some(text()),
            "teardown-only" => self.teardown_only = Some(path()),
            _ => return Err(format!("unknown flag --{name}")),
        }
        Ok(())
    }

    fn finish(self) -> Result<Args, String> {
        let base_url = self.base_url.ok_or("--base-url is required")?;
        let key_file = self.key_file.ok_or("--key-file is required")?;
        let ssh = match (self.ssh_key, self.ssh_user, self.ssh_command.is_some()) {
            (Some(identity), Some(user), _) => Some(Ssh {
                identity,
                user,
                command: self
                    .ssh_command
                    .unwrap_or_else(|| DEFAULT_COMMAND.to_owned()),
            }),
            (None, None, false) => None,
            _ => {
                return Err(
                    "--ssh-key and --ssh-user are given together, and --ssh-command needs both"
                        .to_owned(),
                );
            }
        };
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
            ssh,
            mode,
        })
    }
}
