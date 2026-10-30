//! The abort: terminate everything that carries one scheduler's marks, on the CEO's word.
//!
//! A dry run is the default and calls nothing that changes anything: it prints what would
//! be stopped and what it costs per hour. `apply` stops each instance through the
//! target's own stop, which refuses what is not wholly the scheduler's and reads the
//! instance back. There is no force. Instances with some of the marks, not all, are listed
//! and never touched.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{ElasticProvider, HostRef, Released, SchedulerId};
use js_provider_lambdalabs::{Clock, ElasticConfig, LambdaApi, LambdaProvider, OptionTable};

/// An instance the abort would stop (or leave alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doomed {
    /// The cloud's id for it.
    pub id: String,
    /// Its type.
    pub kind: String,
    /// The region or zone it runs in.
    pub region: String,
    /// What it costs per hour in US cents, when known.
    pub cents_per_hour: Option<u64>,
}

/// What an abort would do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Instances wholly the scheduler's and still running: these are stopped.
    pub doomed: Vec<Doomed>,
    /// Instances with some of the marks, not all: listed, never touched.
    pub left_alone: Vec<Doomed>,
}

/// A call to the target failed. The words hold no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TargetError(String);

impl TargetError {
    /// An error that reads as `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// A cloud that can be aborted. [`Target::plan`] reads; only [`Target::stop`] changes
/// anything, and [`abort`] calls it only when told to apply.
pub trait Target: Send + Sync {
    /// The cloud's name.
    fn cloud(&self) -> &str;

    /// What an abort would do now.
    ///
    /// # Errors
    ///
    /// [`TargetError`] when the cloud cannot be listed whole.
    fn plan(&self) -> Result<Plan, TargetError>;

    /// Stops one instance, if it is wholly the scheduler's.
    ///
    /// # Errors
    ///
    /// [`TargetError`] when the instance was refused or the answer is not known; run the
    /// abort again.
    fn stop(&self, id: &str) -> Result<(), TargetError>;
}

/// What an abort printed and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The lines to print, in order.
    pub lines: Vec<String>,
    /// How many stops the cloud took (always 0 in a dry run).
    pub stopped: usize,
    /// How many stops failed.
    pub failed: usize,
}

fn price(cents: u64) -> String {
    format!("{}.{:02} USD", cents / 100, cents % 100)
}

fn describe(each: &Doomed) -> String {
    let cost = each.cents_per_hour.map_or_else(
        || "price unknown".to_owned(),
        |cents| format!("{}/h", price(cents)),
    );
    format!("{} {} {} {cost}", each.id, each.kind, each.region)
}

/// Stops everything `target` plans to stop when `apply` is true, and only prints what it
/// would stop when it is false.
///
/// # Errors
///
/// [`TargetError`] when the plan cannot be made; nothing was stopped.
pub fn abort(target: &dyn Target, scheduler: &str, apply: bool) -> Result<Outcome, TargetError> {
    let plan = target.plan()?;
    let mode = if apply { "APPLY" } else { "dry run" };
    let verb = if apply { "stopping" } else { "would stop" };
    let mut lines = vec![format!(
        "abort ({mode}): cloud {}, scheduler {scheduler}",
        target.cloud()
    )];
    let (mut stopped, mut failed) = (0, 0);
    for each in &plan.doomed {
        lines.push(format!("{verb} {}", describe(each)));
        if !apply {
            continue;
        }
        match target.stop(&each.id) {
            Ok(()) => stopped += 1,
            Err(why) => {
                failed += 1;
                lines.push(format!("  FAILED {}: {why}", each.id));
            }
        }
    }
    for each in &plan.left_alone {
        lines.push(format!(
            "left alone (some marks, not all; never touched): {}",
            describe(each)
        ));
    }
    let known: u64 = plan
        .doomed
        .iter()
        .filter_map(|each| each.cents_per_hour)
        .sum();
    let unpriced = plan
        .doomed
        .iter()
        .filter(|each| each.cents_per_hour.is_none())
        .count();
    lines.push(format!(
        "total: {} instance(s), {}/hour ({unpriced} without a price)",
        plan.doomed.len(),
        price(known)
    ));
    lines.push(match apply {
        true => format!("stopped {stopped}, failed {failed}; list again to see them gone"),
        false => "dry run: nothing was stopped; run again with --apply to stop them".to_owned(),
    });
    Ok(Outcome {
        lines,
        stopped,
        failed,
    })
}

/// Lambda, through [`LambdaProvider`]: the release the scaler uses, which terminates only
/// an instance whose tags and name are both the scheduler's and reads it back. The
/// provider is built disabled and in dry-run mode, so it cannot launch even if asked.
pub struct LambdaTarget {
    api: Arc<dyn LambdaApi>,
    provider: LambdaProvider,
}

impl LambdaTarget {
    /// A target for `scheduler`'s instances over `api`.
    pub fn new(api: Arc<dyn LambdaApi>, scheduler: SchedulerId, clock: Arc<dyn Clock>) -> Self {
        let config = ElasticConfig::with_account_key(scheduler, NonZeroUsize::MIN);
        let provider = LambdaProvider::new(Arc::clone(&api), clock, OptionTable::empty(), config);
        Self { api, provider }
    }
}

impl Target for LambdaTarget {
    fn cloud(&self) -> &str {
        "lambda"
    }

    fn plan(&self) -> Result<Plan, TargetError> {
        // With nothing booked every running instance of ours is an orphan to the sweep.
        let sweep = self
            .provider
            .sweep(&[])
            .map_err(|why| TargetError::new(why.to_string()))?;
        // A price is a nicety: an instance is listed without one.
        let offers = self.api.instance_types().unwrap_or_default();
        let prices: BTreeMap<String, u64> = offers
            .into_iter()
            .map(|offer| (offer.name, offer.price_cents_per_hour))
            .collect();
        let each = |held: js_elastic::Held| Doomed {
            id: held.id.as_str().to_owned(),
            cents_per_hour: prices.get(&held.offer).copied(),
            kind: held.offer,
            region: held.zone.unwrap_or_default(),
        };
        Ok(Plan {
            doomed: sweep.orphans.into_iter().map(each).collect(),
            left_alone: sweep.suspects.into_iter().map(each).collect(),
        })
    }

    fn stop(&self, id: &str) -> Result<(), TargetError> {
        match self.provider.release(&HostRef(id.to_owned())) {
            Released::Gone | Released::Terminating(_) => Ok(()),
            Released::Refused(claim) => Err(TargetError::new(format!(
                "refused: the instance is not wholly the scheduler's ({claim:?})"
            ))),
            Released::Unknown => Err(TargetError::new("no answer; run the abort again")),
        }
    }
}
