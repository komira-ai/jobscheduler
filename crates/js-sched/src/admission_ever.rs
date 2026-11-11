//! Admission of a request to a pool: whether anything could ever serve it (design 5.6,
//! step 2), decided before the request is queued.
//!
//! This is not the `admission` module of this crate. That one admits an action to a
//! worker by measured and predicted load, now, at placement; this one asks only whether
//! some path could serve the request at all, once it is free or launched, and is asked
//! once, before anything is recorded. Its answer is [`Admission::Refuse`] only when no
//! path exists, and that refusal is the plain constant `no-capable-host`. A host whose
//! advertised network ceiling is below the request's level, or lacks one of its host ports,
//! is not capable ([`crate::ceiling`]).
//!
//! [`can_ever_serve`] takes everything as values: the pool's hosts ([`HostView`]) and its
//! machine providers ([`ProviderView`]), each with its catalog as the caller read it
//! ([`CatalogView`]) and the pool's limits on it ([`PoolLimits`]). It reads no clock and
//! calls no provider; the caller reads the catalogs before it asks.

use js_caps::{FLAG_SET, LAYERED_ROOTFS};
use js_elastic::{Offer, ProviderCaps};
use js_types::{Capabilities, DISK_GIB_KEY, EXEC_KEY, HostId, Requirements, Resources, Shape};

use crate::ceiling::{ceiling_of, within_ceiling};

/// One GiB, the unit of the `disk-gib` capability.
const GIB: u64 = 1 << 30;

/// Whether a request is admitted, and by which path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Something could serve it: queue it.
    Accept(Path),
    /// Nothing could ever serve it: refuse it at once with `no-capable-host`, and record
    /// nothing.
    Refuse,
}

/// The path that admitted a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// A capable host of the pool: one that could hold the request once it is free.
    CapableHost,
    /// A machine provider that may offer a machine for it: an offer of its catalog fits
    /// the request and the pool's limits, or its catalog could not be read.
    MayOffer,
}

/// What the pool's host table says of a host's presence and work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// Heard from, and holding no lease.
    Serving,
    /// Heard from, and holding at least one lease.
    Busy,
    /// Silent past its grace: unknown, not absent.
    Lost,
    /// Cordoned: it takes no new work until the cordon ends, which it does by intent.
    Cordoned,
}

/// A host of the pool, as the caller reads it from its host table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    /// The host.
    pub id: HostId,
    /// Its presence and work. Every state is capable: a busy, lost or cordoned host could
    /// serve the request once it is free, heard again or uncordoned.
    pub state: HostState,
    /// Whether it is tainted. A taint ends only when a person reimages the host, so a
    /// tainted host is never capable.
    pub tainted: bool,
    /// Whether it is draining: leaving the pool, so never capable.
    pub draining: bool,
    /// What the pool books of it.
    pub total: Resources,
    /// The capabilities it reports.
    pub facts: Capabilities,
}

/// A machine provider's catalog, as the caller could read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogView {
    /// The catalog was read: these are its offers, unavailable ones included. An empty
    /// list is a catalog that offers nothing.
    Offers(Vec<Offer>),
    /// The catalog was never fetched, or its fetch failed. It may offer anything inside
    /// the provider's capabilities, and is never read as an empty list.
    Unread,
}

/// The pool's limits on what one machine provider may launch for it (its
/// `provider_limits` table).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolLimits {
    /// The instance types (offer ids) it may launch; `None`: any.
    pub allowed_instance_types: Option<Vec<String>>,
    /// The highest price per hour, in cents; `None`: any. An offer with no price is
    /// outside a price limit: the scaler drops unpriced offers when it picks one, so such
    /// an offer would never be launched under the limit.
    pub max_price_cents_per_hour: Option<u64>,
}

impl PoolLimits {
    /// Whether `offer` is inside the limits.
    fn allow(&self, offer: &Offer) -> bool {
        let typed = self
            .allowed_instance_types
            .as_ref()
            .is_none_or(|types| types.contains(&offer.id));
        let priced = self
            .max_price_cents_per_hour
            .is_none_or(|max| offer.price_cents_per_hour.is_some_and(|price| price <= max));
        typed & priced
    }
}

/// A machine provider of the pool, as the caller reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderView {
    /// Whether it is the `static` kind: hosts that register themselves. It launches
    /// nothing and has no catalog, so it never admits a request; its hosts are among the
    /// [`HostView`]s.
    pub kind_is_static: bool,
    /// Whether it may launch for the pool. No key sets this yet; a caller gives `true`.
    pub enabled: bool,
    /// How many machines it may hold for the pool. A provider with no share launches
    /// nothing for it.
    pub share: u32,
    /// What it says it could ever offer. Every offer of its catalog is inside it, so a
    /// request outside it is not served by this provider even when its catalog is
    /// [`CatalogView::Unread`].
    pub capabilities: ProviderCaps,
    /// Its catalog.
    pub catalog: CatalogView,
    /// The pool's limits on it.
    pub limits: PoolLimits,
}

impl ProviderView {
    /// Whether it may offer a machine for `req`: it launches, is enabled and has a share,
    /// `req` is inside its capabilities, it forwards host ports when `req` declares any
    /// ([`ProviderCaps::host_ports`]; a platform without it is passed over), and its
    /// catalog is unread or has an offer that fits `req` ([`Offer::fit`], which does not ask
    /// whether the offer is available now) and the limits. A provider has no network level
    /// to compare, so `req`'s level is not checked against it.
    fn may_offer(&self, req: &Requirements) -> bool {
        let launches = !self.kind_is_static & self.enabled & (self.share > 0);
        let offered = match &self.catalog {
            CatalogView::Unread => true,
            CatalogView::Offers(offers) => offers
                .iter()
                .any(|offer| offer.fit(req).is_ok() & self.limits.allow(offer)),
        };
        let ports = req.host_ports.is_empty() | self.capabilities.host_ports;
        launches & inside(&self.capabilities, req) & ports & offered
    }
}

/// Whether anything of the pool could ever serve `req`, in the design's order: a capable
/// host in `hosts` admits it ([`Path::CapableHost`]); otherwise a provider of `providers`
/// that may offer a machine for it ([`Path::MayOffer`]); otherwise it is refused.
///
/// A host is capable when it is neither tainted nor draining, what the pool books of it holds
/// `req`'s resources, and its facts satisfy `req`'s `os`, `arch`, `isa_level`, `disk-gib`,
/// `exec` and `layered-rootfs` (as [`Shape::satisfies`] compares them), and the network ceiling its facts advertise
/// ([`ceiling_of`]) covers `req`'s network level and host ports ([`within_ceiling`]; a host that
/// advertises none has the level `none` and no port). Whether it is busy, lost or cordoned does not
/// matter. A provider may offer only if it forwards host ports when `req` declares any; its level
/// is not checked. A pool with no provider that launches admits only through a capable host.
pub fn can_ever_serve(
    req: &Requirements,
    hosts: &[HostView],
    providers: &[ProviderView],
) -> Admission {
    let required = required(req);
    let capable = hosts.iter().any(|host| {
        let shape = Shape::new(host.total.clone(), host.facts.clone());
        let ceiling = ceiling_of(&host.facts);
        let within = within_ceiling(&ceiling, req.network, &req.host_ports).is_ok();
        !host.tainted & !host.draining & shape.satisfies(&req.resources, &required) & within
    });
    let offered = providers.iter().any(|provider| provider.may_offer(req));
    match (capable, offered) {
        (true, _) => Admission::Accept(Path::CapableHost),
        (false, true) => Admission::Accept(Path::MayOffer),
        (false, false) => Admission::Refuse,
    }
}

/// Whether `req` is inside `caps`: its operating system and architecture, when it names
/// them, are in the sets, and so is the canonical family of the model it needs (its
/// `accelerator.model`, else its booked GPU class, resolved through the alias table), as
/// the provider's accelerators are canonical. A booked class that is not a model name is
/// in no provider's set; a need that names no model is inside.
fn inside(caps: &ProviderCaps, req: &Requirements) -> bool {
    let os = req.os.as_ref().is_none_or(|os| caps.os.contains(os));
    let arch = req
        .arch
        .as_ref()
        .is_none_or(|arch| caps.arch.contains(arch));
    let model = req
        .accelerator
        .as_ref()
        .and_then(|need| need.model.as_ref());
    let class = match model {
        Some(model) => caps.accelerators.contains(&model.canonical().model),
        None => req.resources.gpu_class().is_none(),
    };
    os & arch & class
}

/// The capabilities a host must report for `req`, as a lease request names them: `os`,
/// `arch`, `disk-gib` (rounded up to a whole GiB), `isa_level` and `exec`, each only when
/// `req` names it, and `layered-rootfs=true` when `req` names layers, mounts or host ports
/// ([`Requirements::layered_rootfs`]).
fn required(req: &Requirements) -> Capabilities {
    let named = [
        ("os", req.os.clone()),
        ("arch", req.arch.clone()),
        (
            DISK_GIB_KEY,
            req.disk_bytes.map(|bytes| bytes.div_ceil(GIB).to_string()),
        ),
        ("isa_level", req.isa_level.map(|level| level.to_string())),
        (EXEC_KEY, req.exec.map(|mode| mode.name().to_owned())),
        (
            LAYERED_ROOTFS,
            req.layered_rootfs.then(|| FLAG_SET.to_owned()),
        ),
    ];
    named
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect()
}

#[cfg(test)]
#[path = "admission_ever_tests.rs"]
mod tests;
