//! Putting the scaler together at start: the pool's plan from the configuration file, the
//! server's agent side, the pool's checks and network mode, and the records file.

use std::path::Path;
use std::sync::{Arc, Mutex};

use js_elastic::Artifact;
use js_registry::OsSessions;
use js_scaler::{LaunchMode, Settings};
use js_types::ExecMode;

use super::{Launcher, Live, RecordsFile, Scaler};
use crate::adapt::SharedClock;
use crate::agents::{Agents, state_path};
use crate::checks::Health;
use crate::error::StartupError;
use crate::file::{ElasticPlan, Launch};
use crate::network::{DefaultMode, NetworkError, NetworkMode};

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
    /// What the pool's checks have found: the scaler launches only while they pass, and
    /// launches from the offers they read.
    pub(crate) health: Arc<Mutex<Health>>,
    /// The pool's network mode, or why it could not be built.
    pub(crate) mode: Result<Arc<dyn NetworkMode>, NetworkError>,
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

/// The network mode the scaler launches with. A pool in `plan` mode launches nothing, so
/// when its mode could not be built (the checks show why) a stand-in that is never called
/// serves; a live pool whose mode could not be built does not start.
fn network_mode(
    built: Result<Arc<dyn NetworkMode>, NetworkError>,
    plan: &ElasticPlan,
) -> Result<Arc<dyn NetworkMode>, StartupError> {
    match (built, plan.launch) {
        (Ok(mode), _) => Ok(mode),
        (Err(_), Launch::Plan) => Ok(Arc::new(DefaultMode::new("", Vec::new()))),
        (Err(err), Launch::Live) => {
            let place = format!("pool {:?}: network", plan.name);
            Err(StartupError::at(place, err))
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
/// [`StartupError`] when the records file cannot be read, or a live pool's network mode
/// could not be built.
pub(crate) fn build(plan: &ElasticPlan, parts: Parts<'_>) -> Result<Scaler, StartupError> {
    let file = RecordsFile::new(state_path(parts.state_file, ".scaler"));
    let records = file.load()?;
    let mode = network_mode(parts.mode, plan)?;
    let url = parts.advertise_url.unwrap_or_default();
    let agent = plan.agent.clone().unwrap_or_else(unpinned);
    let launcher = Launcher::new(
        Arc::clone(&plan.provider),
        mode,
        plan.scheduler.clone(),
        url.to_owned(),
        agent,
    );
    let registry = parts.agents.registry.clone();
    let live = Live::new(
        (parts.agents, plan.id.clone(), plan.scheduler.clone()),
        Arc::clone(&plan.provider),
        launcher,
        (file, parts.health),
    );
    let wiring = (
        plan.scheduler.clone(),
        plan.exec.unwrap_or(ExecMode::Container),
    );
    let scaler = Scaler::new(
        Box::new(live),
        parts.clock,
        Box::new(OsSessions),
        wiring,
        settings(plan),
        records,
    );
    registry.set_launchable(Some(scaler.launchable()));
    Ok(scaler)
}
