//! One launch: the plan rendered with both secrets and handed to the provider as a payload,
//! what happens when the network mode or the renderer cannot do its part, and the cleanup
//! after a release. The provider and the mode are stubs that remember what they were asked.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, Artifact, BootStep, ElasticProvider, HostName, HostRef,
    Offer, ProviderError, Reason, RefusalCode, Released, SchedulerId, Secret, SlotName,
};
use js_lease_server::network::{ApiFailure, Contribution, LaunchCtx, NetworkError, NetworkMode};
use js_lease_server::{KEY_VALID_FOR, Launcher, RETRY_AFTER};
use js_scaler::AcquirePlan;

use crate::fakes::offer;

/// What the provider was asked to launch.
#[derive(Default)]
struct Asked {
    name: Vec<String>,
    acquire: Vec<String>,
    payload: Vec<String>,
}

struct Provider {
    limit: usize,
    asked: Mutex<Asked>,
}

impl ElasticProvider for Provider {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        Ok(Vec::new())
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        vec![BootStep::Run(vec![
            "prepare-image".to_owned(),
            offer.id.clone(),
        ])]
    }

    fn payload_limit(&self) -> usize {
        self.limit
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        let mut asked = self.asked.lock().unwrap();
        asked.name.push(request.name.to_string());
        asked.acquire.push(request.acquire.to_string());
        asked.payload.push(request.payload.expose().to_owned());
        Acquired::Unknown
    }

    fn list(&self) -> Result<Vec<js_elastic::Held>, ProviderError> {
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        Released::Unknown
    }
}

/// What the mode was asked.
#[derive(Default)]
struct Seen {
    launches: Vec<(String, String, Duration)>,
    forgotten: Vec<String>,
}

struct Mode {
    slot: &'static str,
    fail_prepare: bool,
    fail_forget: bool,
    seen: Mutex<Seen>,
}

struct Handle(Arc<Mode>);

impl NetworkMode for Handle {
    fn prepare(&self, launch: &LaunchCtx<'_>) -> Result<Contribution, NetworkError> {
        let entry = (
            launch.pool.to_owned(),
            launch.host.to_string(),
            launch.valid_for,
        );
        self.0.seen.lock().unwrap().launches.push(entry);
        if self.0.fail_prepare {
            return Err(NetworkError::Misconfigured("no key can be made".to_owned()));
        }
        let slot = SlotName(self.0.slot.to_owned());
        Ok(Contribution {
            steps: vec![BootStep::SecretFile {
                path: "/etc/net.key".to_owned(),
                slot: slot.clone(),
            }],
            secrets: vec![(slot, Secret::new("net-secret-value".to_owned()))],
        })
    }

    fn describe(&self, _launch: &LaunchCtx<'_>) -> Vec<BootStep> {
        Vec::new()
    }

    fn check(&self) -> Result<(), NetworkError> {
        Ok(())
    }

    fn forget(&self, host: &HostName) -> Result<(), NetworkError> {
        self.0.seen.lock().unwrap().forgotten.push(host.to_string());
        match self.0.fail_forget {
            true => Err(NetworkError::DeviceDelete(ApiFailure::Rejected(403))),
            false => Ok(()),
        }
    }
}

struct Rig {
    provider: Arc<Provider>,
    mode: Arc<Mode>,
    launcher: Launcher,
}

fn rig(limit: usize, mode: Mode) -> Rig {
    let provider = Arc::new(Provider {
        limit,
        asked: Mutex::new(Asked::default()),
    });
    let mode = Arc::new(mode);
    let launcher = Launcher::new(
        provider.clone(),
        Arc::new(Handle(mode.clone())),
        SchedulerId::new("sched").unwrap(),
        "https://sched.example.org:7301".to_owned(),
        [
            (
                "x86_64".to_owned(),
                Artifact {
                    url: "https://sched.example.org:7301/agent".to_owned(),
                    sha256: [7; 32],
                },
            ),
            (
                "aarch64".to_owned(),
                Artifact {
                    url: "https://sched.example.org:7301/agent-arm".to_owned(),
                    sha256: [9; 32],
                },
            ),
        ]
        .into(),
    );
    Rig {
        provider,
        mode,
        launcher,
    }
}

fn mode(slot: &'static str) -> Mode {
    Mode {
        slot,
        fail_prepare: false,
        fail_forget: false,
        seen: Mutex::new(Seen::default()),
    }
}

/// The plan of an acquire of the pool `burst`, and the name the launch gives the machine.
fn plan() -> (AcquirePlan, HostName) {
    let acquire = AcquireId::new("burst-0001").unwrap();
    let name = HostName::derive(&SchedulerId::new("sched").unwrap(), &acquire);
    let plan = AcquirePlan {
        name: name.to_string(),
        acquire,
        attempt: 0,
        pool: "burst".to_owned(),
        offer: offer(),
        lifetime: None,
    };
    (plan, name)
}

fn join_token() -> Secret {
    Secret::new("join-token-value".to_owned())
}

/// Asserts what a launch hands the provider: the machine's name derived from the scheduler
/// and the acquire id, the acquire id, and a payload that holds the join token, the network
/// mode's secret, the instance line `<pool>:<name>`, the provider's image steps, the mode's
/// steps and the address of the scheduler; the mode was asked for a secret that lives
/// [`KEY_VALID_FOR`] for that pool and that machine. The answer is the provider's. It
/// catches a launch that leaves a secret out of the payload, one that renders with the
/// mask, and a machine named other than the name its hello will carry.
#[test]
fn a_launch_hands_the_provider_a_payload_with_both_secrets() {
    let rig = rig(1 << 20, mode("network.key"));
    let (plan, name) = plan();
    let answer = rig.launcher.acquire(&plan, join_token());
    assert_eq!(answer, Acquired::Unknown);
    let asked = rig.provider.asked.lock().unwrap();
    assert_eq!(asked.name, [name.to_string()]);
    assert_eq!(asked.acquire, ["burst-0001"]);
    let payload = &asked.payload[0];
    let instance = format!("burst:{name}");
    for wanted in [
        "join-token-value",
        "net-secret-value",
        instance.as_str(),
        "prepare-image",
        "/etc/net.key",
        "https://sched.example.org:7301",
        // The agent registers under the machine's name, which the scaler finds its record
        // by; pulls an image only when it is missing; and runs the toolkit hook.
        format!("--host-id \"{name}\"").as_str(),
        "--container-pull missing",
        "--gpu-toolkit-hook /usr/local/lib/js-agent/gpu-toolkit-hook",
        "exec nvidia-container-cli info",
    ] {
        assert!(payload.contains(wanted), "{wanted} is not in the payload");
    }
    // The name is a host id, so the agent's hello registers under exactly the record's name.
    assert_eq!(
        js_types::HostId::new(name.as_str()).unwrap().as_str(),
        name.as_str()
    );
    assert!(!payload.contains("***"), "the payload was rendered masked");
    let seen = rig.mode.seen.lock().unwrap();
    let launch = ("burst".to_owned(), name.to_string(), KEY_VALID_FOR);
    assert_eq!(seen.launches, [launch]);
}

/// Asserts that a network mode that cannot make its secret sends nothing to the provider
/// and answers with a deferral of [`RETRY_AFTER`], so that the scaler plans the launch
/// again and mints a new token. It catches a launch that goes out without the network
/// step it needed.
#[test]
fn a_network_secret_that_cannot_be_made_defers_the_launch() {
    let failing = Mode {
        fail_prepare: true,
        ..mode("network.key")
    };
    let rig = rig(1 << 20, failing);
    let answer = rig.launcher.acquire(&plan().0, join_token());
    assert_eq!(answer, Acquired::Deferred { wait: RETRY_AFTER });
    assert!(rig.provider.asked.lock().unwrap().payload.is_empty());
}

/// Asserts that a plan that cannot be rendered is refused as a shape error and sent
/// nowhere: a payload over the provider's limit, and a mode that fills the slot of the
/// join token. The reason names the fault and holds neither secret. It catches a payload
/// the provider would cut or refuse sent anyway, and a mode able to overwrite the join
/// token.
#[test]
fn a_plan_that_cannot_be_rendered_is_refused_and_not_sent() {
    let small = rig(10, mode("network.key"));
    let answer = small.launcher.acquire(&plan().0, join_token());
    let Acquired::Refused(refusal) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(refusal.code, RefusalCode::Shape);
    let Reason::Boot(why) = refusal.reason else {
        panic!("not a boot reason");
    };
    assert!(why.contains("provider carries at most 10"), "{why}");
    assert!(small.provider.asked.lock().unwrap().payload.is_empty());

    let twice = rig(1 << 20, mode("join_token"));
    let answer = twice.launcher.acquire(&plan().0, join_token());
    let Acquired::Refused(refusal) = answer else {
        panic!("{answer:?}");
    };
    let Reason::Boot(why) = refusal.reason else {
        panic!("not a boot reason");
    };
    assert!(why.contains("filled twice"), "{why}");
    for secret in ["join-token-value", "net-secret-value"] {
        assert!(!why.contains(secret), "{why}");
    }
    assert!(twice.provider.asked.lock().unwrap().payload.is_empty());
}

/// Asserts that forgetting a released host asks the mode to clean up after exactly that
/// host, and that a cleanup that fails is not an error: the release goes on.
#[test]
fn a_released_host_is_forgotten_and_a_failed_cleanup_is_not_fatal() {
    let (_, name) = plan();
    let kept = rig(1 << 20, mode("network.key"));
    kept.launcher.forget(&name);
    assert_eq!(kept.mode.seen.lock().unwrap().forgotten, [name.to_string()]);
    let failing = Mode {
        fail_forget: true,
        ..mode("network.key")
    };
    let broken = rig(1 << 20, failing);
    broken.launcher.forget(&name);
    assert_eq!(broken.mode.seen.lock().unwrap().forgotten.len(), 1);
}

/// Asserts that each architecture gets its own agent: an x86_64 offer is launched with the
/// x86_64 address and hash, an aarch64 offer with the aarch64 ones and not the others'; and
/// an offer of an architecture with no pin is refused `Shape` and nothing is sent, never
/// launched with another architecture's binary. It catches a launch that uses the wrong
/// binary for a machine, which would boot and never join.
#[test]
fn each_architecture_gets_its_own_agent_and_one_with_none_is_refused() {
    let rig = rig(1 << 20, mode("network.key"));
    let (mut plan, _) = plan();
    rig.launcher.acquire(&plan, join_token());
    plan.offer.arch = "aarch64".to_owned();
    rig.launcher.acquire(&plan, join_token());
    {
        let asked = rig.provider.asked.lock().unwrap();
        let (x86, arm) = (&asked.payload[0], &asked.payload[1]);
        assert!(
            x86.contains("7301/agent") && !x86.contains("agent-arm"),
            "{x86}"
        );
        assert!(x86.contains(&"07".repeat(32)) && !x86.contains(&"09".repeat(32)));
        assert!(arm.contains("7301/agent-arm"), "{arm}");
        assert!(arm.contains(&"09".repeat(32)) && !arm.contains(&"07".repeat(32)));
    }

    plan.offer.arch = "riscv64".to_owned();
    let answer = rig.launcher.acquire(&plan, join_token());
    let Acquired::Refused(refusal) = answer else {
        panic!("not refused: {answer:?}");
    };
    assert_eq!(refusal.code, RefusalCode::Shape);
    assert!(matches!(refusal.reason, Reason::Boot(why) if why.contains("riscv64")));
    assert_eq!(rig.provider.asked.lock().unwrap().payload.len(), 2);
}
