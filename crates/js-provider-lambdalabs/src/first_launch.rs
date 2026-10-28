//! The first live launch as a program: one instance, launched through [`LambdaProvider`],
//! waited for, optionally asked a question over ssh, and terminated, under hard limits.
//!
//! It is a library module so that every branch runs in a test against [`FakeLambda`] with
//! a [`ManualClock`]; the example `first_launch` only wires the real client, clock, sleep
//! and ssh into [`run`]. Nothing here makes an HTTP request of its own.
//!
//! - A dry run (the default) reads the instance types, picks the cheapest x86_64 GPU
//!   type with capacity (or the one named by flag), prints the plan with the price the
//!   API gave and the limits, and stops. The plan ends in a hash of its text.
//! - `--go --plan-hash H` launches only when the plan read now hashes to `H`; a price,
//!   capacity or flag that changed since the dry run gives a different hash and a refusal.
//! - The state file is created before the launch is sent and rewritten when it answers,
//!   so `--teardown-only <state file>` can find the instance after a crash.
//! - A boot deadline, a wall-clock cap and a spend cap (the API's hourly price times the
//!   time since the launch) each force teardown. Termination is tried a bounded number
//!   of times and each try reads the instance back.

mod args;
mod state;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::api::LambdaApi;
use crate::clock::Clock;
use crate::config::ElasticConfig;
use crate::elastic::{AcquireRequest, Acquired, Candidate, Elastic, GpuRequest, Held, Released};
use crate::http::SetupError;
use crate::key::ApiKey;
use crate::model::{InstanceId, Status};
use crate::options::{GpuClass, OptionTable};
use crate::own::{AcquireId, SchedulerId};
use crate::provider::LambdaProvider;

use args::{Args, Mode, Ssh};
use state::State;

/// The scheduler id and pool name on everything this runner launches.
const OWNER: &str = "first-launch";
/// How often the instance is looked at while booting.
const POLL: Duration = Duration::from_secs(10);
/// The most tries to terminate one instance.
pub const TERMINATE_ATTEMPTS: u32 = 5;
/// The pause between two tries to terminate.
const TERMINATE_PAUSE: Duration = Duration::from_secs(10);
/// The longest the remote command may run.
const SSH_LIMIT: Duration = Duration::from_secs(120);
/// The most of the remote command's output printed.
const SSH_OUTPUT: usize = 4000;
/// What to paste back after a run that did what was asked.
const STOP_DONE: &str = "paste back everything printed above, from the plan line to here.";
/// What to paste back after a run whose check failed but whose instance is gone.
const STOP_CHECK_FAILED: &str = "the run did not finish its check; the instance is terminated. Paste back everything printed above.";
/// The class every row of the generated table carries: the runner asks for any GPU.
const ANY_GPU: &str = "any";

/// Waiting, as the runner needs it.
pub trait Sleeper {
    /// Blocks for `how_long`.
    fn sleep(&self, how_long: Duration);
}

/// Running a command on the instance.
pub trait Remote {
    /// Runs `command` as `user` at `host`, with the private key in `identity`, and
    /// gives up after `limit`. The output, or why it failed.
    ///
    /// # Errors
    ///
    /// A sentence for the operator.
    fn run(
        &self,
        host: &str,
        user: &str,
        identity: &Path,
        command: &str,
        limit: Duration,
    ) -> Result<String, String>;
}

/// Builds the client for a base URL, a key and an optional CA bundle.
pub type Connect<'a> =
    dyn Fn(&str, &ApiKey, Option<&Path>) -> Result<Arc<dyn LambdaApi>, SetupError> + 'a;

/// Everything outside the process that the runner touches, except the state file.
pub struct Seams<'a> {
    /// The monotonic clock the limits read.
    pub clock: Arc<dyn Clock>,
    /// Waits between polls and tries.
    pub sleeper: &'a dyn Sleeper,
    /// Runs the ssh command.
    pub remote: &'a dyn Remote,
    /// Seconds since the Unix epoch, for the state file.
    pub wall_unix: &'a dyn Fn() -> u64,
    /// Builds the API client.
    pub connect: &'a Connect<'a>,
}

fn say(out: &mut dyn Write, line: impl Display) {
    let _ = writeln!(out, "{line}");
}

/// Runs the program with `args` (the command line without the program name), writing
/// everything it has to say to `out`. Returns the exit status: 0 when the run did what
/// was asked, 2 for a usage error, 1 otherwise.
pub fn run(args: &[String], seams: &Seams<'_>, out: &mut dyn Write) -> i32 {
    let args = match args::parse(args) {
        Ok(args) => args,
        Err(why) => {
            say(out, format_args!("error: {why}"));
            say(
                out,
                "STOP: fix the command line and paste back this output.",
            );
            return 2;
        }
    };
    let key = match ApiKey::from_file(&args.key_file) {
        Ok(key) => key,
        Err(why) => {
            say(out, format_args!("error: {why}"));
            say(out, "STOP: fix the key file and paste back this output.");
            return 1;
        }
    };
    let api = match (seams.connect)(&args.base_url, &key, args.ca_bundle.as_deref()) {
        Ok(api) => api,
        Err(why) => {
            say(out, format_args!("error: {why}"));
            say(
                out,
                "STOP: fix the base URL or CA bundle and paste back this output.",
            );
            return 1;
        }
    };
    let mut runner = Runner {
        args: &args,
        seams,
        api,
        out,
    };
    match &args.mode {
        Mode::Teardown { state_file } => runner.teardown_only(state_file),
        Mode::DryRun | Mode::Go { .. } => runner.plan_and_go(),
    }
}

/// What a dry run prints and a launch is checked against.
struct Plan {
    candidate: Candidate,
    key_name: String,
    state_file: std::path::PathBuf,
    text: String,
    hash: String,
}

/// The limits on one launch.
struct Caps {
    wall: Duration,
    spend_after: Duration,
}

impl Caps {
    fn new(args: &Args, price_cents_per_hour: u64) -> Self {
        // The cap is reached when price * time >= cap * 1 hour, in whole milliseconds.
        let millis = (u128::from(args.spend_cap_cents) * 3_600_000)
            .div_ceil(u128::from(price_cents_per_hour));
        Self {
            wall: args.wall_cap,
            spend_after: Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX)),
        }
    }

    fn hit(&self, elapsed: Duration) -> Option<&'static str> {
        match (elapsed >= self.spend_after, elapsed >= self.wall) {
            (true, _) => Some("the spend cap is reached"),
            (false, true) => Some("the wall-clock cap is reached"),
            (false, false) => None,
        }
    }

    fn left(&self, elapsed: Duration) -> Duration {
        self.wall.min(self.spend_after).saturating_sub(elapsed)
    }
}

struct Runner<'a, 'o> {
    args: &'a Args,
    seams: &'a Seams<'a>,
    api: Arc<dyn LambdaApi>,
    out: &'o mut dyn Write,
}

impl Runner<'_, '_> {
    fn say(&mut self, line: impl Display) {
        say(self.out, line);
    }

    fn fail(&mut self, line: impl Display, stop: &str) -> i32 {
        self.say(format_args!("error: {line}"));
        self.say(format_args!("STOP: {stop}"));
        1
    }

    /// A provider that may use the instance types of `rows` (name and GPU count), with
    /// room for one machine, and the class its rows carry.
    fn build(
        &mut self,
        rows: &[(String, u32)],
        key_name: &str,
    ) -> Result<(LambdaProvider, GpuClass), i32> {
        // One row at a time, so that a type name the table would refuse (the API's
        // naming is not ours to trust) only drops that type; the later row of a name
        // seen twice replaces the earlier one.
        let mut valid = BTreeMap::new();
        for (name, gpus) in rows {
            let row = format!(
                "[[option]]\ninstance_type = {name:?}\ngpu_class = \"{ANY_GPU}\"\ngpus = {gpus}\n"
            );
            if OptionTable::parse(&row).is_ok() {
                valid.insert(name.clone(), row);
            }
        }
        let text: String = valid.into_values().collect();
        let Ok(table) = OptionTable::parse(&text) else {
            return Err(self.fail(
                "there is no usable GPU instance type",
                "paste back this output.",
            ));
        };
        let class = table.rows()[0].gpu_class.clone();
        let config = ElasticConfig::new(SchedulerId::made(OWNER), key_name, NonZeroUsize::MIN);
        let Ok(config) = config else {
            return Err(self.fail(
                "the ssh key name must be 1 to 64 printable ASCII characters",
                "fix the command line and paste back this output.",
            ));
        };
        let provider = LambdaProvider::new(
            self.api.clone(),
            self.seams.clock.clone(),
            table,
            config.enable().live(),
        );
        Ok((provider, class))
    }

    /// The plan for the flags and the account as the API shows it now.
    fn plan(&mut self) -> Result<(Plan, LambdaProvider), i32> {
        let (Some(key_name), Some(state_file)) =
            (self.args.key_name.clone(), self.args.state_file.clone())
        else {
            self.say("error: --key-name and --state-file are required");
            self.say("STOP: fix the command line and paste back this output.");
            return Err(2);
        };
        let offers = match self.api.instance_types() {
            Ok(offers) => offers,
            Err(why) => {
                return Err(self.fail(
                    format_args!("the instance types cannot be read: {why}"),
                    "paste back this output.",
                ));
            }
        };
        let rows: Vec<(String, u32)> = offers.iter().map(|o| (o.name.clone(), o.gpus)).collect();
        let (provider, any) = self.build(&rows, &key_name)?;
        let want = GpuRequest {
            cpu_millis: 0,
            memory_bytes: 0,
            gpus: 1,
            gpu_class: any,
        };
        let options = match provider.launch_options(&want) {
            Ok(options) => options,
            Err(why) => {
                return Err(self.fail(
                    format_args!("the instance types cannot be read: {why}"),
                    "paste back this output.",
                ));
            }
        };
        let args = self.args;
        let chosen = options.into_iter().find(|c| {
            args.instance_type
                .as_ref()
                .is_none_or(|t| *t == c.instance_type)
                && args.region.as_ref().is_none_or(|r| *r == c.region)
        });
        let Some(candidate) = chosen else {
            return Err(self.fail(
                "no x86_64 GPU instance type with capacity fits the flags",
                "nothing was launched; paste back this output.",
            ));
        };
        if candidate.price_cents_per_hour == 0 {
            return Err(self.fail(
                "the API gave no price for the instance type",
                "nothing was launched; paste back this output.",
            ));
        }
        let caps = Caps::new(args, candidate.price_cents_per_hour);
        let how = match (&args.instance_type, &args.region) {
            (None, None) => "the cheapest x86_64 GPU type with capacity",
            _ => "named by flag, with capacity",
        };
        let ssh = match &args.ssh {
            None => "none".to_owned(),
            Some(ssh) => format!("{}@instance: {}", ssh.user, ssh.command),
        };
        let text = format!(
            "first launch plan\n  api: {}\n  choice: {how}\n  instance type: {}\n  region: {}\n  gpus: {}\n  price: {} cents per hour (from the API)\n  ssh key name on the account: {key_name}\n  state file: {}\n  boot deadline: {} s\n  wall-clock cap: {} s\n  spend cap: {} cents, reached {} s after the launch at this price\n  ssh check: {ssh}\n  instances launched: 1\n",
            args.base_url,
            candidate.instance_type,
            candidate.region,
            candidate.gpus,
            candidate.price_cents_per_hour,
            state_file.display(),
            args.boot_deadline.as_secs(),
            args.wall_cap.as_secs(),
            args.spend_cap_cents,
            caps.spend_after.as_secs_f64().ceil(),
        );
        let digest = Sha256::digest(text.as_bytes());
        let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        Ok((
            Plan {
                candidate,
                key_name,
                state_file,
                text,
                hash,
            },
            provider,
        ))
    }

    fn plan_and_go(&mut self) -> i32 {
        let (plan, provider) = match self.plan() {
            Ok(found) => found,
            Err(status) => return status,
        };
        self.say(plan.text.trim_end());
        self.say(format_args!("plan hash: {}", plan.hash));
        let given = match &self.args.mode {
            Mode::Go { plan_hash } => plan_hash.clone(),
            _ => {
                self.say("dry run: nothing was launched.");
                self.say("STOP: paste back this output, the plan hash line included.");
                return 0;
            }
        };
        if given.as_deref() != Some(plan.hash.as_str()) {
            return self.fail(
                "--go needs --plan-hash equal to the hash of the plan above; the plan read now does not match, so nothing was launched",
                "paste back this output.",
            );
        }
        self.launch(&plan, &provider)
    }

    fn launch(&mut self, plan: &Plan, provider: &LambdaProvider) -> i32 {
        let acquire = AcquireId::made(format!("{OWNER}-{}", plan.hash));
        let mut state = State {
            acquire: acquire.clone(),
            launched_unix: (self.seams.wall_unix)(),
            key_name: plan.key_name.clone(),
            instance_type: plan.candidate.instance_type.clone(),
            region: plan.candidate.region.clone(),
            price_cents_per_hour: plan.candidate.price_cents_per_hour,
            instances: Vec::new(),
        };
        if let Err(why) = state.create(&plan.state_file) {
            return self.fail(
                format_args!("{why}; nothing was launched"),
                "remove or move the state file only if you know no instance runs, then paste back this output.",
            );
        }
        let launched_at = self.seams.clock.now();
        let request = AcquireRequest {
            acquire: acquire.clone(),
            pool: OWNER.to_owned(),
            labels: Vec::new(),
            candidate: plan.candidate.clone(),
        };
        let ids = match provider.acquire(&request) {
            Acquired::Accepted { held, extras } => {
                let mut ids = vec![held.id];
                ids.extend(extras.into_iter().map(|extra| extra.id));
                ids
            }
            Acquired::Unknown => {
                self.say("the launch got no answer; looking for the instance by its acquire id");
                let found = provider.list().map(|all| matching(&all, &acquire));
                match found {
                    Ok(ids) if !ids.is_empty() => ids,
                    _ => {
                        return self.fail(
                            "the launch got no answer and no instance shows yet; the state file is kept",
                            "wait a minute, run teardown with the state file, and check the provider console; paste back this output.",
                        );
                    }
                }
            }
            other => {
                let _ = std::fs::remove_file(&plan.state_file);
                return self.fail(
                    format_args!("nothing was launched: {other:?}"),
                    "paste back this output.",
                );
            }
        };
        state.instances.clone_from(&ids);
        let recorded = state.replace(&plan.state_file);
        let names: Vec<&str> = ids.iter().map(InstanceId::as_str).collect();
        self.say(format_args!("launched: {}", names.join(" ")));
        self.say(format_args!("state file: {}", plan.state_file.display()));
        let caps = Caps::new(self.args, plan.candidate.price_cents_per_hour);
        let mut ok = true;
        let reason = match recorded {
            Err(why) => Some(format!("the state file could not be updated: {why}")),
            Ok(()) => self.wait_and_ask(provider, &ids, launched_at, &caps, &mut ok),
        };
        if let Some(reason) = &reason {
            self.say(format_args!("teardown forced: {reason}"));
            ok = false;
        }
        let torn = self.terminate_all(provider, &ids);
        let spent = (u128::from(plan.candidate.price_cents_per_hour)
            * self
                .seams
                .clock
                .now()
                .saturating_sub(launched_at)
                .as_millis())
        .div_ceil(3_600_000);
        self.say(format_args!(
            "result: {} after {} s, at most {spent} cents spent at {} cents per hour",
            if torn {
                "terminated"
            } else {
                "TERMINATION NOT CONFIRMED"
            },
            self.seams.clock.now().saturating_sub(launched_at).as_secs(),
            plan.candidate.price_cents_per_hour,
        ));
        self.finish(ok && torn, torn, &plan.state_file)
    }

    fn finish(&mut self, ok: bool, torn: bool, state_file: &Path) -> i32 {
        let stop = match (ok, torn) {
            (true, _) => STOP_DONE.to_owned(),
            (false, true) => STOP_CHECK_FAILED.to_owned(),
            (false, false) => format!(
                "AN INSTANCE MAY STILL BE RUNNING AND BILLING. Run the teardown command with {}, check the provider console, and paste back everything printed above.",
                state_file.display()
            ),
        };
        self.say(format_args!("STOP: {stop}"));
        i32::from(!ok)
    }

    /// Waits for active within the limits and runs the ssh check. Returns why the run
    /// must be torn down early, or `None` when it reached active and was asked what it
    /// was meant to be asked (`ok` is cleared when the ssh check failed).
    fn wait_and_ask(
        &mut self,
        provider: &LambdaProvider,
        ids: &[InstanceId],
        launched_at: Duration,
        caps: &Caps,
        ok: &mut bool,
    ) -> Option<String> {
        let clock = self.seams.clock.clone();
        let mut shown: Option<Status> = None;
        loop {
            let elapsed = clock.now().saturating_sub(launched_at);
            match provider.list() {
                Ok(all) => {
                    let status = all
                        .iter()
                        .find(|h| h.id == ids[0])
                        .map(|h| h.status.clone());
                    if status != shown {
                        self.say(format_args!("t+{} s: status {status:?}", elapsed.as_secs()));
                        shown.clone_from(&status);
                    }
                    match status {
                        Some(Status::Active) => break,
                        Some(Status::Terminating | Status::Terminated | Status::Preempted) => {
                            return Some("the instance ended on its own".to_owned());
                        }
                        _ => {}
                    }
                }
                Err(why) => self.say(format_args!(
                    "t+{} s: list failed: {why}",
                    elapsed.as_secs()
                )),
            }
            if let Some(cap) = caps.hit(elapsed) {
                return Some(cap.to_owned());
            }
            if elapsed >= self.args.boot_deadline {
                return Some("the boot deadline passed before the instance was active".to_owned());
            }
            self.seams.sleeper.sleep(POLL);
        }
        let elapsed = clock.now().saturating_sub(launched_at);
        if let Some(cap) = caps.hit(elapsed) {
            return Some(cap.to_owned());
        }
        if let Some(ssh) = &self.args.ssh {
            *ok = self.ask(ssh, &ids[0], caps.left(elapsed));
        }
        None
    }

    fn ask(&mut self, ssh: &Ssh, id: &InstanceId, left: Duration) -> bool {
        let host = self.api.get_instance(id).ok().and_then(|i| i.ip);
        let Some(host) = host else {
            self.say("ssh check failed: the instance has no public address");
            return false;
        };
        let limit = left.min(SSH_LIMIT);
        match self
            .seams
            .remote
            .run(&host, &ssh.user, &ssh.identity, &ssh.command, limit)
        {
            Ok(output) => {
                let shown: String = output.chars().take(SSH_OUTPUT).collect();
                self.say(format_args!(
                    "ssh output of {}:\n{}",
                    ssh.command,
                    shown.trim_end()
                ));
                true
            }
            Err(why) => {
                self.say(format_args!("ssh check failed: {why}"));
                false
            }
        }
    }

    /// Terminates every instance of `ids`, a bounded number of tries each, reading each
    /// back. True when every one is confirmed going or gone.
    fn terminate_all(&mut self, provider: &LambdaProvider, ids: &[InstanceId]) -> bool {
        let mut all = true;
        for id in ids {
            let mut done = false;
            for attempt in 1..=TERMINATE_ATTEMPTS {
                let released = provider.release(id);
                self.say(format_args!("terminate {id}, try {attempt}: {released:?}"));
                match released {
                    Released::Gone
                    | Released::Terminating(Status::Terminating | Status::Terminated) => {
                        done = true;
                        break;
                    }
                    Released::Refused(_) => break,
                    Released::Terminating(_) | Released::Unknown => {}
                }
                if attempt < TERMINATE_ATTEMPTS {
                    self.seams.sleeper.sleep(TERMINATE_PAUSE);
                }
            }
            all &= done;
        }
        all
    }

    fn teardown_only(&mut self, state_file: &Path) -> i32 {
        let state = match State::read(state_file) {
            Ok(state) => state,
            Err(why) => {
                return self.fail(why, "fix the state file path and paste back this output.");
            }
        };
        let rows = [(state.instance_type.clone(), 1)];
        let (provider, _) = match self.build(&rows, &state.key_name) {
            Ok(built) => built,
            Err(status) => return status,
        };
        let mut ids = state.instances.clone();
        match provider.list() {
            Ok(all) => {
                for id in matching(&all, &state.acquire) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            Err(why) => self.say(format_args!("the instances could not be listed: {why}")),
        }
        if ids.is_empty() {
            return self.fail(
                "no instance of this state file was found; if a launch landed it may not be listed yet",
                "check the provider console, then paste back this output.",
            );
        }
        let names: Vec<&str> = ids.iter().map(InstanceId::as_str).collect();
        self.say(format_args!("tearing down: {}", names.join(" ")));
        let torn = self.terminate_all(&provider, &ids);
        self.say(format_args!(
            "result: {}",
            if torn {
                "terminated"
            } else {
                "TERMINATION NOT CONFIRMED"
            }
        ));
        self.finish(torn, torn, state_file)
    }
}

fn matching(held: &[Held], acquire: &AcquireId) -> Vec<InstanceId> {
    held.iter()
        .filter(|h| h.acquire == acquire.as_str())
        .map(|h| h.id.clone())
        .collect()
}
