//! Putting the scaler together at start: the pool's plan from the configuration file, the
//! server's agent side, and the records file.

use std::path::Path;
use std::sync::Arc;

use js_elastic::{Artifact, ElasticProvider};
use js_registry::OsSessions;
use js_scaler::{LaunchMode, Settings};
use js_types::ExecMode;

use super::{Launcher, Live, RecordsFile, Scaler};
use crate::adapt::SharedClock;
use crate::agents::{Agents, state_path};
use crate::error::StartupError;
use crate::file::{ElasticPlan, Launch, NetworkPlan};
use crate::network::{
    API_BASE, API_TIMEOUT, DefaultMode, NetworkMode, TailscaleApi, TailscaleMode, TailscaleSettings,
};

/// What the scaler is put together with besides the pool's plan.
pub(crate) struct Parts<'a> {
    /// The agent side of the server: the registry and the join tokens.
    pub(crate) agents: Arc<Agents>,
    /// The server's clock.
    pub(crate) clock: SharedClock,
    /// The lease file, beside which the records are kept.
    pub(crate) state_file: &'a Path,
    /// The address launched hosts are told to dial, when the file gives one.
    pub(crate) advertise_url: Option<&'a str>,
}

/// What a plan-mode pool is given where a live pool needs a pin it has not been given. A
/// plan launches nothing, so nothing renders it; a live pool without one is refused when
/// the file is read.
fn unpinned() -> Artifact {
    Artifact {
        url: String::new(),
        sha256: [0; 32],
    }
}

/// The network mode of the pool `pool`.
fn network_mode(
    network: NetworkPlan,
    pool: &str,
    clock: SharedClock,
    advertise_url: &str,
) -> Result<Box<dyn NetworkMode>, StartupError> {
    match network {
        NetworkPlan::Default => Ok(Box::new(DefaultMode::new(advertise_url, Vec::new()))),
        NetworkPlan::Tailscale {
            client,
            tag,
            binary,
            api_url,
        } => {
            let base = api_url.unwrap_or_else(|| API_BASE.to_owned());
            let api = match TailscaleApi::new(&base, "-", client, clock, API_TIMEOUT) {
                Ok(api) => api,
                Err(err) => {
                    let place = format!("pool {pool:?}: network.api_url");
                    return Err(StartupError::at(place, err));
                }
            };
            let binary = binary.unwrap_or_else(unpinned);
            let settings = TailscaleSettings { tag, binary };
            Ok(Box::new(TailscaleMode::unchecked(api, settings)))
        }
    }
}

/// The scaler settings the pool's plan gives: the design's defaults besides the plan's own.
fn settings(plan: &ElasticPlan) -> Settings {
    let mut settings = Settings::new(
        &plan.name,
        plan.max_instances,
        plan.max_spend_per_hour_cents,
        plan.max_spend_per_day_cents,
    );
    settings.launch = match plan.launch {
        Launch::Live => LaunchMode::Live,
        Launch::Plan => LaunchMode::Plan,
    };
    settings.idle = plan.idle.unwrap_or(settings.idle);
    settings.boot_timeout = plan.boot_timeout;
    settings
}

/// The scaler of `plan`, with the records of an earlier run read back, every machine they
/// show as being released marked in the registry, and the registry's `Launchable` set to
/// answer from it.
///
/// # Errors
///
/// [`StartupError`] when the records file cannot be read, or the Tailscale API address is
/// not usable.
pub(crate) fn build(plan: ElasticPlan, parts: Parts<'_>) -> Result<Scaler, StartupError> {
    let file = RecordsFile::new(state_path(parts.state_file, ".scaler"));
    let records = file.load()?;
    let settings = settings(&plan);
    let ElasticPlan {
        name,
        exec,
        network,
        agent,
        scheduler,
        id,
        provider,
        ..
    } = plan;
    let url = parts.advertise_url.unwrap_or_default();
    let mode = network_mode(network, &name, parts.clock.clone(), url)?;
    let provider: Arc<dyn ElasticProvider> = Arc::from(provider);
    let agent = agent.unwrap_or_else(unpinned);
    let launcher = Launcher::new(
        Arc::clone(&provider),
        mode,
        scheduler.clone(),
        url.to_owned(),
        agent,
    );
    let registry = parts.agents.registry.clone();
    let live = Live::new(
        (parts.agents, id, scheduler.clone()),
        provider,
        launcher,
        file,
    );
    let exec = exec.unwrap_or(ExecMode::Container);
    let entropy = Box::new(OsSessions);
    let wiring = (scheduler, exec);
    let scaler = Scaler::new(
        Box::new(live),
        parts.clock,
        entropy,
        wiring,
        settings,
        records,
    );
    registry.set_launchable(Some(scaler.launchable()));
    Ok(scaler)
}
