//! An elastic pool as the check loop and the status routes see it: what the file said
//! about it, the provider and network mode it launches with, what the checks found, and
//! the plan of the launch it would make next.
//!
//! The plan is built from the same pieces a launch is: the network mode's steps
//! ([`NetworkMode::describe`]), the provider's host preparation, the agent and the
//! scheduler's address, rendered by [`crate::bootstrap::render`]. It is rendered with
//! [`Fill::Mask`], so a secret slot prints as `***` and the status routes never hold a
//! secret: nothing here calls [`NetworkMode::prepare`], and no join token exists to
//! render.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, Artifact, BootPlan, BootStep, Demand, Dimension, ElasticProvider, HostName, Offer,
    SchedulerId,
};
use js_types::ExecMode;
use serde::Serialize;

use crate::adapt::SharedClock;
use crate::bootstrap::{Fill, JOIN_TOKEN_SLOT, render};
use crate::checks::{Checker, Health};
use crate::config::TlsFiles;
use crate::file::{ElasticPlan, NetworkPlan};
use crate::kinds::Floor;
use crate::network::{
    DefaultMode, LaunchCtx, NetworkError, NetworkMode, OAuthClient, TailscaleMode,
};

/// How long the key of a launch is said to be valid in the plan shown: ten minutes, as a
/// launch asks for.
const KEY_VALID_FOR: Duration = Duration::from_secs(600);

/// The name the plan shown is made for. A launch names its host from the scheduler's id
/// and a fresh acquire id; the plan shown is for no launch, so its name is a stand-in.
const PREVIEW: &str = "preview";

/// What a launch plan needs that the configuration file does not carry. The server fills
/// it from what it ships; a test or an embedder may replace any of it.
#[derive(Default)]
pub struct LaunchInputs {
    /// The SHA-256 of the agent binary the service serves, by CPU architecture. An offer
    /// of an architecture with no entry has no plan shown: it says why.
    pub agent_sha256: BTreeMap<String, [u8; 32]>,
    /// The Tailscale static build a `tailscale` pool fetches onto its hosts. None is
    /// pinned in this release, so without one a `tailscale` pool is unavailable and says
    /// so.
    pub tailscale: Option<Artifact>,
    /// A network mode that stands in for the one the pool's `network` table names.
    pub network: Option<Arc<dyn NetworkMode>>,
}

/// What a pool is built with besides its plan.
pub(crate) struct Setup {
    /// `server.advertise_url`.
    pub(crate) advertise: Option<String>,
    /// The certificate files of the server.
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) inputs: LaunchInputs,
    pub(crate) clock: SharedClock,
}

/// An elastic pool.
pub(crate) struct Pool {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) network_kind: &'static str,
    pub(crate) exec: Option<ExecMode>,
    pub(crate) launch: &'static str,
    pub(crate) max_instances: usize,
    pub(crate) max_spend_per_hour_cents: u64,
    pub(crate) health: Arc<Mutex<Health>>,
    provider: Arc<dyn ElasticProvider>,
    mode: Result<Arc<dyn NetworkMode>, NetworkError>,
    floor: Floor,
    pub(crate) floor_demand: Demand,
    advertise: Option<String>,
    agent_sha256: BTreeMap<String, [u8; 32]>,
}

/// The network mode of a pool: the stand-in when one is given, else the mode the table
/// names, or why it cannot be built.
fn build_mode(
    network: NetworkPlan,
    setup: &mut Setup,
) -> Result<Arc<dyn NetworkMode>, NetworkError> {
    if let Some(mode) = setup.inputs.network.take() {
        return Ok(mode);
    }
    match network {
        NetworkPlan::Default => match setup.advertise.as_deref() {
            Some(url) => {
                let mode = DefaultMode::new(url, tls_paths(setup.tls.as_ref()));
                Ok(Arc::new(mode))
            }
            None => Err(NetworkError::Misconfigured(
                "server.advertise_url is not set, so a launched host has nowhere to dial"
                    .to_owned(),
            )),
        },
        NetworkPlan::Tailscale { client, tag } => tailscale(client, tag, setup),
    }
}

/// The certificate and key files of the server, which the `default` mode checks can be
/// opened when the advertised URL is `https`.
fn tls_paths(tls: Option<&TlsFiles>) -> Vec<PathBuf> {
    tls.into_iter()
        .flat_map(|tls| [tls.cert.clone(), tls.key.clone()])
        .collect()
}

/// The `tailscale` mode, when a static build is pinned to fetch.
fn tailscale(
    client: OAuthClient,
    tag: String,
    setup: &mut Setup,
) -> Result<Arc<dyn NetworkMode>, NetworkError> {
    match setup.inputs.tailscale.take() {
        Some(binary) => {
            let mode = TailscaleMode::for_client(client, tag, binary, setup.clock.clone())?;
            Ok(Arc::new(mode))
        }
        None => Err(NetworkError::Misconfigured(
            "tailscale: no static build is pinned in this release".to_owned(),
        )),
    }
}

impl Pool {
    pub(crate) fn new(plan: ElasticPlan, mut setup: Setup) -> Self {
        let network_kind = plan.network.kind();
        let mode = build_mode(plan.network, &mut setup);
        Self {
            network_kind,
            mode,
            launch: plan.launch.as_str(),
            name: plan.name,
            kind: plan.kind,
            exec: plan.exec,
            max_instances: plan.max_instances,
            max_spend_per_hour_cents: plan.max_spend_per_hour_cents,
            health: Arc::new(Mutex::new(Health::default())),
            provider: plan.provider,
            floor: plan.floor,
            floor_demand: plan.floor_demand,
            advertise: setup.advertise,
            agent_sha256: setup.inputs.agent_sha256,
        }
    }

    /// The check loop of this pool.
    pub(crate) fn checker(&self, clock: SharedClock) -> Checker {
        Checker {
            pool: self.name.clone(),
            provider: self.provider.clone(),
            mode: self.mode.clone(),
            floor: self.floor.clone(),
            clock,
            health: self.health.clone(),
        }
    }

    /// The launch that would serve `wanted` out of `offers`, or why none would.
    pub(crate) fn next_launch(&self, wanted: &Wanted, offers: &[Offer]) -> Next {
        match choose(offers, &wanted.demand) {
            Ok(offer) => Next::Planned(self.planned(offer)),
            Err(dimension) => Next::NoOffer(NoOffer {
                request: wanted.request.clone(),
                dimension,
            }),
        }
    }

    fn planned(&self, offer: &Offer) -> NextLaunch {
        let host = preview_host();
        let name = host.to_string();
        let (plan, plan_error, secret_slots) = match self.plan(offer, host) {
            Ok((text, slots)) => (Some(text), None, slots),
            Err(reason) => (None, Some(reason), Vec::new()),
        };
        NextLaunch {
            offer: offer.id.clone(),
            zone: offer.zone.clone(),
            name,
            plan,
            plan_error,
            secret_slots,
        }
    }

    /// The masked render of the plan for `offer` on `host`, and the slots it names.
    fn plan(&self, offer: &Offer, host: HostName) -> Result<(String, Vec<String>), String> {
        let Some(base) = self.advertise.as_deref() else {
            return Err("server.advertise_url is not set".to_owned());
        };
        let Some(sha256) = self.agent_sha256.get(&offer.arch) else {
            return Err(format!(
                "the agent binary for {} is not available",
                offer.arch
            ));
        };
        let mode = match &self.mode {
            Ok(mode) => mode,
            Err(error) => return Err(error.to_string()),
        };
        let base = base.trim_end_matches('/');
        let ctx = LaunchCtx {
            pool: &self.name,
            host: &host,
            valid_for: KEY_VALID_FOR,
        };
        let plan = BootPlan {
            agent: Artifact {
                url: format!("{base}/v1/bootstrap/agent/{}", offer.arch),
                sha256: *sha256,
            },
            scheduler_url: base.to_owned(),
            pool: self.name.clone(),
            network: mode.describe(&ctx),
            host_prep: self.provider.host_prep(offer),
            host,
        };
        match render(&plan, Fill::Mask) {
            Ok(text) => Ok((text, secret_slots(&plan))),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The name of the host the plan shown is for.
///
/// # Panics
///
/// Never: [`PREVIEW`] is a valid id, which the unit test of this module keeps true.
fn preview_host() -> HostName {
    let scheduler = SchedulerId::new(PREVIEW).expect("PREVIEW is a valid scheduler id");
    let acquire = AcquireId::new(PREVIEW).expect("PREVIEW is a valid acquire id");
    HostName::derive(&scheduler, &acquire)
}

/// The slots a plan names: the join token, and each secret file a step writes.
fn secret_slots(plan: &BootPlan) -> Vec<String> {
    let mut slots = vec![JOIN_TOKEN_SLOT.to_owned()];
    for step in plan.host_prep.iter().chain(&plan.network) {
        if let BootStep::SecretFile { slot, .. } = step {
            slots.push(slot.0.clone());
        }
    }
    slots
}

/// What a launch would be for.
pub(crate) struct Wanted {
    /// The request, or `floor`.
    pub(crate) request: String,
    pub(crate) demand: Demand,
}

/// The next launch, or why there is none.
pub(crate) enum Next {
    Planned(NextLaunch),
    NoOffer(NoOffer),
}

/// The plan of the next launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct NextLaunch {
    pub(crate) offer: String,
    pub(crate) zone: Option<String>,
    pub(crate) name: String,
    /// The boot script with every secret printed as `***`; `None` when it cannot be made.
    pub(crate) plan: Option<String>,
    /// Why there is no plan.
    pub(crate) plan_error: Option<String>,
    pub(crate) secret_slots: Vec<String>,
}

/// A request no offer would serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct NoOffer {
    pub(crate) request: String,
    /// The dimension that fails: `catalog` when there are no offers, `available` when an
    /// offer fits and none has capacity, else the first dimension an offer fails.
    pub(crate) dimension: String,
}

/// The names of the dimensions an offer can fail, as the status page spells them.
const DIMENSIONS: [(Dimension, &str); 8] = [
    (Dimension::Os, "os"),
    (Dimension::Arch, "arch"),
    (Dimension::Exec, "exec"),
    (Dimension::CpuMillis, "cpu_millis"),
    (Dimension::MemoryBytes, "memory_bytes"),
    (Dimension::DiskBytes, "disk_bytes"),
    (Dimension::Gpus, "gpus"),
    (Dimension::GpuClass, "gpu_class"),
];

/// The cheapest offer with capacity that serves `demand`: lowest price, then fewest GPUs,
/// then id, then zone. Else the dimension that fails.
fn choose<'a>(offers: &'a [Offer], demand: &Demand) -> Result<&'a Offer, String> {
    let fits: Vec<(&Offer, Result<(), Dimension>)> =
        offers.iter().map(|o| (o, o.fit(demand))).collect();
    let buyable = fits
        .iter()
        .filter(|(offer, fit)| fit.is_ok() && offer.available)
        .map(|(offer, _)| *offer);
    let cheapest = buyable.min_by_key(|o| {
        let price = o.price_cents_per_hour.unwrap_or(u64::MAX);
        (price, o.gpus, o.id.clone(), o.zone.clone())
    });
    match cheapest {
        Some(offer) => Ok(offer),
        None => Err(missing(&fits)),
    }
}

/// Why no offer serves: the first dimension some offer fails, else that the ones that fit
/// have no capacity, else that there is no offer.
fn missing(fits: &[(&Offer, Result<(), Dimension>)]) -> String {
    let miss = fits.iter().find_map(|(_, fit)| fit.err());
    let named = DIMENSIONS
        .iter()
        .find(|(dimension, _)| Some(*dimension) == miss)
        .map(|(_, name)| *name);
    let word = match (fits.is_empty(), named) {
        (true, _) => "catalog",
        (false, Some(name)) => name,
        (false, None) => "available",
    };
    word.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the certificate and key of the server are the files the `default`
    /// mode is told to open, in that order, and that a server with no TLS names none.
    #[test]
    fn the_tls_files_are_the_certificate_then_the_key() {
        let tls = TlsFiles {
            cert: PathBuf::from("/c.pem"),
            key: PathBuf::from("/k.pem"),
        };
        assert_eq!(
            tls_paths(Some(&tls)),
            [PathBuf::from("/c.pem"), PathBuf::from("/k.pem")]
        );
        assert!(tls_paths(None).is_empty());
    }

    /// Asserts that the name the plan shown is made for is a host name of a scheduler:
    /// the constant it is derived from is a valid id, and the name has the prefix of one.
    #[test]
    fn the_preview_name_is_a_host_name() {
        let name = preview_host().to_string();
        assert!(name.starts_with("js-"), "{name}");
        assert_eq!(name, preview_host().to_string());
    }
}
