//! The black-box test itself: one program (`released scenarios`) that is given, as flags, what
//! was installed (the released js-server image and its files, the released js-agent's unit and
//! the two packages of the update) and where each fake listens (the job image on the fake
//! registry, the fake mount provider's name), starts the services under test, runs each
//! scenario and holds it to its pass condition. Installing and wiring is the wrapper's
//! (`released rig`, wrapper.rs); nothing here installs a package but the update under test.
//!
//! The scenarios run in order on one rig, each from a host in a session with the released agent
//! and no job running, and each leaves it so. A scenario that fails is reported and the next
//! runs; J6 and J1 are the start of every other, so when either fails the rest are reported not
//! run. The program exits 1 when any scenario failed or did not run. `--only` runs the named
//! ones (each skipped one is reported with that reason); J6 and J1 always run.

use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use js_blackbox::{
    BACK_WITHIN, HostSeen, JobSeen, LIVE, LOST, OUTAGE, READY_WITHIN, RUNNING, SESSION_WITHIN,
    SUCCEEDED, Seen, Version, host as host_of, job as job_of, result as result_of,
};
use js_client::Client;
use serde_json::json;

use crate::host::{put, run, try_run};
use crate::{agent, server, watch};

/// How long a job of the test may take before the test gives up on it (a bound, not a pass
/// condition: the longest job sleeps 150 s).
const JOB_LIMIT: Duration = Duration::from_secs(600);

/// The flags of `released scenarios`.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// The released js-server image, loaded into Docker (its id).
    #[arg(long)]
    pub server_image: String,
    /// The server's secrets, mounted at /run/secrets.
    #[arg(long)]
    pub server_secrets: PathBuf,
    /// The server's configuration files, mounted at /etc/js-lease-server.
    #[arg(long)]
    pub server_etc: PathBuf,
    /// The server's state directory, mounted at /var/lib/js-lease-server.
    #[arg(long)]
    pub server_state: PathBuf,
    /// The server's command line, one argument a line.
    #[arg(long)]
    pub server_args: PathBuf,
    /// The server's readiness route (plain HTTP).
    #[arg(long)]
    pub server_ready: SocketAddr,
    /// The server's agent port, on this host's loopback.
    #[arg(long)]
    pub server_agent_port: u16,
    /// The name the agent reaches the server by, resolved through /etc/hosts to 127.0.0.1.
    #[arg(long)]
    pub server_name: String,
    /// The job service, `https://host:port`.
    #[arg(long)]
    pub job_service: String,
    /// The job service's client token file.
    #[arg(long)]
    pub client_token: PathBuf,
    /// The CA file the job service's certificate chains to.
    #[arg(long)]
    pub ca_cert: PathBuf,
    /// The released agent's unit.
    #[arg(long)]
    pub agent_unit: String,
    /// The agent's health route (plain HTTP).
    #[arg(long)]
    pub agent_health: SocketAddr,
    /// The host's id.
    #[arg(long)]
    pub host_id: String,
    /// The release under test, MAJOR.MINOR.PATCH: the installed agent's version.
    #[arg(long, value_parser = parse_version)]
    pub released: Version,
    /// The previous release, which the update scenario updates from.
    #[arg(long, value_parser = parse_version)]
    pub previous: Version,
    /// The package of the previous release.
    #[arg(long)]
    pub previous_deb: PathBuf,
    /// The package of the release under test.
    #[arg(long)]
    pub released_deb: PathBuf,
    /// The job image, on the fake registry, pinned by digest.
    #[arg(long)]
    pub image: String,
    /// The fake mount provider's name, as the agent's flags name it.
    #[arg(long)]
    pub provider: String,
    /// Run only these scenarios (besides j6 and j1); repeatable.
    #[arg(long)]
    pub only: Vec<String>,
}

fn parse_version(text: &str) -> Result<Version, String> {
    Version::parse(text).ok_or_else(|| format!("{text:?} is not MAJOR.MINOR.PATCH"))
}

/// The scenarios, in the order they run.
const SCENARIOS: [(&str, fn(&Rig) -> Result<(), String>); 6] = [
    ("j6", j6),
    ("j1", j1),
    ("name-lookup", name_lookup),
    ("server-unreachable", server_unreachable),
    ("server-restart", server_restart),
    ("agent-update", agent_update),
];

/// Runs the scenarios and says how each ended.
pub fn run_all(flags: Flags) -> ExitCode {
    let rig = Rig::start(flags);
    let mut failed = 0u32;
    let mut started = true;
    for (name, scenario) in SCENARIOS {
        let first = matches!(name, "j6" | "j1");
        if !first && !rig.flags.only.is_empty() && !rig.flags.only.iter().any(|o| o == name) {
            println!("SKIP {name}: not named by --only");
            continue;
        }
        if !started {
            println!("NOT RUN {name}: j6 and j1, which every scenario starts from, did not pass");
            failed += 1;
            continue;
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| scenario(&rig))).unwrap_or_else(|panic| {
            Err(panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "a panic with no message".to_owned()))
        });
        match outcome {
            Ok(()) => println!("PASS {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed += 1;
                started &= !first;
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        println!("{failed} scenario(s) failed or did not run");
        ExitCode::FAILURE
    }
}

/// The services under test, started, and what the scenarios measured at their start.
struct Rig {
    flags: Flags,
    /// The job service's client, once the server is ready.
    client: Option<Client>,
    /// J6: how long after its container started the server answered `/readyz` 200, or why not.
    ready: Result<Duration, String>,
    /// J1: how long after its unit started the host was in a session, or why not.
    session: Result<Duration, String>,
}

impl Rig {
    /// Starts the server's container and, once it is ready, the agent's unit, measuring each.
    fn start(flags: Flags) -> Rig {
        server::start(
            &flags.server_image,
            &flags.server_secrets,
            &flags.server_etc,
            &flags.server_state,
            &flags.server_args,
        );
        let ready = watch(|w| {
            w.until(
                READY_WITHIN,
                &mut || match server::ready(flags.server_ready) {
                    Ok(()) => Seen::Yes,
                    Err(e) => Seen::No(e),
                },
            )
        })
        .within("/readyz answers 200", READY_WITHIN)
        .map_err(|e| format!("{e}\n{}", server::logs()));
        let mut rig = Rig {
            flags,
            client: None,
            ready,
            session: Err("not started: the server was not ready".to_owned()),
        };
        if rig.ready.is_err() {
            return rig;
        }
        rig.client = Some(
            Client::connect(
                &rig.flags.job_service,
                &rig.flags.client_token,
                Some(rig.flags.ca_cert.as_path()),
                Duration::from_secs(10),
            )
            .expect("the job service's client"),
        );
        agent::start(&rig.flags.agent_unit);
        rig.session = rig.in_session(SESSION_WITHIN, rig.flags.released);
        rig
    }

    fn client(&self) -> &Client {
        self.client.as_ref().expect("the server is ready")
    }

    fn journal(&self) -> String {
        agent::journal(&self.flags.agent_unit)
    }

    /// The host as the job service lists it, or why it cannot be read.
    fn host(&self) -> Result<HostSeen, String> {
        let reply = self
            .client()
            .get("/v1/hosts")
            .map_err(|e| format!("GET /v1/hosts: {e}"))?;
        if reply.status != 200 {
            return Err(format!("GET /v1/hosts: {} {}", reply.status, reply.body));
        }
        host_of(&reply.body, &self.flags.host_id)
            .ok_or_else(|| format!("{} is not listed", self.flags.host_id))
    }

    /// Waits up to `limit` for the host to be in a session with an agent of `version`: listed
    /// live with that version, and the agent's health route 200. How long it took, or why not.
    fn in_session(&self, limit: Duration, version: Version) -> Result<Duration, String> {
        let want = version.to_string();
        watch(|w| {
            w.until(
                limit,
                &mut || match (self.host(), agent::healthy(self.flags.agent_health)) {
                    (Ok(h), Ok(())) if h.state == LIVE && h.agent_version == want => Seen::Yes,
                    (h, health) => Seen::No(format!("host {h:?}, health {health:?}")),
                },
            )
        })
        .within(&format!("in a session with js-agent {want}"), limit)
        .map_err(|e| format!("{e}\n{}", self.journal()))
    }

    /// The start of every scenario after J1: the host in a session with the released agent.
    fn steady(&self) -> Result<(), String> {
        self.in_session(SESSION_WITHIN, self.flags.released)
            .map(|_| ())
            .map_err(|e| format!("the rig is not in a session: {e}"))
    }

    /// Submits a job that sleeps `seconds` and ends 0, with `mounts` (gateway JSON): its id.
    fn submit(&self, name: &str, seconds: u32, mounts: serde_json::Value) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst));
        let body = json!({"spec": {
            "jobId": id,
            "image": self.flags.image,
            "command": "/bin/sleep",
            "args": [seconds.to_string()],
            "wallLimitSeconds": "900",
            "needs": {"cpuMillis": "250", "memoryBytes": "134217728"},
            "mounts": mounts
        }});
        let reply = self
            .client()
            .post("/v1/jobs", &body)
            .unwrap_or_else(|e| panic!("POST /v1/jobs: {e}"));
        assert!(
            reply.status == 200 || reply.status == 201,
            "POST /v1/jobs: {} {}",
            reply.status,
            reply.body
        );
        id
    }

    /// The job `id` as the job service answers it.
    fn job(&self, id: &str) -> Result<JobSeen, String> {
        let reply = self
            .client()
            .get(&format!("/v1/jobs/{id}"))
            .map_err(|e| format!("GET /v1/jobs/{id}: {e}"))?;
        job_of(&reply.body).map_err(|e| format!("GET /v1/jobs/{id}: {} {e}", reply.status))
    }

    /// Waits for the job to run on the host.
    fn running(&self, id: &str) -> Result<JobSeen, String> {
        let mut seen = None;
        watch(|w| {
            w.until(JOB_LIMIT, &mut || match self.job(id) {
                Ok(j) if j.state == RUNNING && j.host_id == self.flags.host_id => {
                    seen = Some(j);
                    Seen::Yes
                }
                other => Seen::No(format!("{other:?}")),
            })
        })
        .within(
            &format!("job {id} runs on {}", self.flags.host_id),
            JOB_LIMIT,
        )
        .map_err(|e| format!("{e}\n{}", self.journal()))?;
        seen.ok_or_else(|| format!("job {id} was not seen running"))
    }

    /// Waits for the job to end, then holds its result to exit code 0 in its first attempt.
    fn ends_zero(&self, id: &str) -> Result<(), String> {
        let mut ended = None;
        watch(|w| {
            w.until(JOB_LIMIT, &mut || match self.job(id) {
                Ok(j) if j.ended() => {
                    ended = Some(j);
                    Seen::Yes
                }
                other => Seen::No(format!("{other:?}")),
            })
        })
        .within(&format!("job {id} ends"), JOB_LIMIT)
        .map_err(|e| format!("{e}\n{}", self.journal()))?;
        let reply = self
            .client()
            .get(&format!("/v1/jobs/{id}/result"))
            .map_err(|e| format!("GET /v1/jobs/{id}/result: {e}"))?;
        let result = result_of(&reply.body)?;
        if result.state == SUCCEEDED && result.exit_code == Some(0) && result.attempt == 1 {
            Ok(())
        } else {
            Err(format!(
                "job {id} did not end with exit code 0 in its first attempt: {result:?} (job {ended:?})\n{}",
                self.journal()
            ))
        }
    }
}

/// A firewall rule that drops what reaches a port of this host, removed when dropped.
struct Cut(u16);

impl Cut {
    fn args(&self, op: &str) -> Vec<String> {
        let port = self.0.to_string();
        [
            op,
            "INPUT",
            "--protocol",
            "tcp",
            "--dport",
            port.as_str(),
            "--jump",
            "DROP",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn new(port: u16) -> Cut {
        let cut = Cut(port);
        let args = cut.args("--insert");
        run(
            "iptables",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        cut
    }
}

impl Drop for Cut {
    fn drop(&mut self) {
        let args = self.args("--delete");
        let _ = try_run(
            "iptables",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
    }
}

/// `/etc/hosts` without the lines that name `name`, put back when dropped.
struct Unresolved(String);

impl Unresolved {
    fn new(name: &str) -> Unresolved {
        let hosts = std::fs::read_to_string("/etc/hosts").expect("read /etc/hosts");
        let without: String = hosts
            .lines()
            .filter(|l| !l.split_whitespace().skip(1).any(|n| n == name))
            .map(|l| format!("{l}\n"))
            .collect();
        put(Path::new("/etc/hosts"), without, 0o644, (0, 0));
        Unresolved(hosts)
    }
}

impl Drop for Unresolved {
    fn drop(&mut self) {
        put(Path::new("/etc/hosts"), &self.0, 0o644, (0, 0));
    }
}

/// J6. The released js-server image, started as the control host starts it, answers `/readyz`
/// 200 within 60 s of its container's start, and runs a job through the released agent: the job
/// ends with exit code 0.
fn j6(rig: &Rig) -> Result<(), String> {
    let ready = rig.ready.clone()?;
    println!(
        "j6: /readyz answered 200 {} ms after the container started",
        ready.as_millis()
    );
    rig.session.clone()?;
    let id = rig.submit("j6", 1, json!([]));
    rig.ends_zero(&id)
}

/// J1. The released `.deb`, installed and started under systemd with flags of the farm's shape,
/// joins the released server: the host is in a session within 60 s of the unit's start, and a
/// job that mounts a tree from the fake mount provider ends with exit code 0.
fn j1(rig: &Rig) -> Result<(), String> {
    let session = rig.session.clone()?;
    println!(
        "j1: in a session {} ms after the unit started",
        session.as_millis()
    );
    let mounts = json!([{
        "provider": {"provider": rig.flags.provider, "ref": "blackbox-tree"},
        "target": "/input",
        "mode": "MOUNT_MODE_READ_ONLY"
    }]);
    let id = rig.submit("j1", 1, mounts);
    rig.ends_zero(&id)
}

/// The server's name cannot be looked up for 60 s after the first contact (its line is taken out
/// of `/etc/hosts`): the agent keeps its session, the host listed live and the agent's health
/// route 200 at every probe, and a job submitted in the outage ends with exit code 0.
fn name_lookup(rig: &Rig) -> Result<(), String> {
    rig.steady()?;
    let name = rig.flags.server_name.as_str();
    let _unresolved = Unresolved::new(name);
    if try_run("getent", &["hosts", name]).is_ok() {
        return Err(format!(
            "{name} is still found after its line left /etc/hosts"
        ));
    }
    let id = rig.submit("name-lookup", 30, json!([]));
    watch(|w| {
        w.hold(OUTAGE, &mut || -> Result<(), String> {
            let h = rig.host()?;
            if h.state != LIVE {
                return Err(format!("the host is listed {}", h.state));
            }
            agent::healthy(rig.flags.agent_health)
        })
    })
    .kept("the session is kept while the name is not found")
    .map_err(|e| format!("{e}\n{}", rig.journal()))?;
    rig.ends_zero(&id)
}

/// The server is unreachable for 60 s while a job runs (what reaches its agent port is dropped):
/// the job keeps running through the outage, the agent is back in a session within 30 s of the
/// server's return, and the job ends with exit code 0.
fn server_unreachable(rig: &Rig) -> Result<(), String> {
    rig.steady()?;
    let id = rig.submit("server-unreachable", 150, json!([]));
    rig.running(&id)?;
    {
        let _cut = Cut::new(rig.flags.server_agent_port);
        watch(|w| {
            w.hold(OUTAGE, &mut || match rig.job(&id) {
                Ok(j) if j.ended() => Err(format!("the job ended in the outage: {j:?}")),
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            })
        })
        .kept("the job keeps running while the server is unreachable")
        .map_err(|e| format!("{e}\n{}", rig.journal()))?;
    }
    let back = rig
        .in_session(BACK_WITHIN, rig.flags.released)
        .map_err(|e| format!("after the server's return: {e}"))?;
    println!(
        "server-unreachable: back in a session {} ms after the server's return",
        back.as_millis()
    );
    rig.ends_zero(&id)
}

/// The server is restarted while a job runs: the job ends with exit code 0 in its first
/// attempt, and no listing of the hosts the server gives after the restart shows the host lost.
fn server_restart(rig: &Rig) -> Result<(), String> {
    rig.steady()?;
    let id = rig.submit("server-restart", 90, json!([]));
    rig.running(&id)?;
    server::restart();
    // From the restart on, until the job ends: every listing the server gives must show the host
    // not lost, and the job not lost nor its host silent. A probe the server does not answer
    // (still starting) shows nothing and passes.
    let mut seen = 0u32;
    watch(|w| {
        w.hold_until(JOB_LIMIT, &mut || -> Result<bool, String> {
            if let Ok(h) = rig.host() {
                seen += 1;
                if h.state == LOST {
                    return Err(format!("the host is listed {LOST} ({h:?})"));
                }
            }
            match rig.job(&id) {
                Ok(j) if j.host_silent || j.state == "JOB_STATE_LOST" => {
                    Err(format!("the job shows its host lost: {j:?}"))
                }
                Ok(j) => Ok(j.ended()),
                Err(_) => Ok(false),
            }
        })
    })
    .kept("the host is never shown lost")
    .map_err(|e| format!("{e}\n{}", server::logs()))?;
    if seen == 0 {
        return Err("the restarted server never listed the hosts".to_owned());
    }
    rig.ends_zero(&id)
}

/// The agent is updated while a job runs, from the previous release's package to this release's,
/// the way the update hook does it on a farm host (the new package installed, the handover file
/// written, the unit restarted): the job is handed over to the new agent and ends with exit code
/// 0 in its first attempt, and the host lists the new version within 60 s of the restart.
fn agent_update(rig: &Rig) -> Result<(), String> {
    let flags = &rig.flags;
    rig.steady()?;
    agent::install_file(&flags.previous_deb, flags.previous);
    agent::restart(&flags.agent_unit);
    rig.in_session(SESSION_WITHIN, flags.previous)
        .map_err(|e| format!("the previous release {}: {e}", flags.previous))?;
    let id = rig.submit("agent-update", 90, json!([]));
    let before = rig.running(&id)?;
    agent::install_file(&flags.released_deb, flags.released);
    agent::ask_for_handover();
    agent::restart(&flags.agent_unit);
    rig.in_session(SESSION_WITHIN, flags.released)
        .map_err(|e| format!("after the update: {e}"))?;
    let now = rig.job(&id)?;
    if (now.attempt, now.host_id.as_str()) != (before.attempt, flags.host_id.as_str()) {
        return Err(format!(
            "the job left its attempt or its host in the update: {now:?}"
        ));
    }
    rig.ends_zero(&id)
}
