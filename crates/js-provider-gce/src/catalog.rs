//! The catalog: the cloud's answers read through the port, selected by the settings and
//! translated into offers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_elastic::{Offer, ProviderError};

use crate::api::{GceApi, Method, PurchaseKind};
use crate::error::{ApiError, CallError, SettingsError};
use crate::gpu;
use crate::model::{
    AcceleratorTypeInfo, AttachedAccelerator, MachineTypeInfo, MachineTypeName, Project, Region,
    Zone,
};
use crate::price::Prices;
use crate::quota::RegionQuotas;
use crate::translate::{self, Dropped};

/// What the configuration selects for one Compute Engine catalog: the project, the zones,
/// the provisioning models in order and the machine types it allows. It selects and
/// limits, and never says what a machine type provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GceCatalogSettings {
    project: Project,
    zones: Vec<Zone>,
    purchase: Vec<PurchaseKind>,
    allowed_machine_types: Option<BTreeSet<MachineTypeName>>,
}

impl GceCatalogSettings {
    /// The settings of the `project` key, the `zones` list, the ordered `purchase` list and
    /// the `allowed_machine_types` list (`None`: not set).
    ///
    /// # Errors
    ///
    /// [`SettingsError`] naming the key: a project that is not a project id, an empty zone
    /// list, a zone that is not a zone name or is listed twice, an empty purchase list, and
    /// a purchase option that is not `on-demand` or `spot` or is listed twice.
    pub fn new(
        project: &str,
        zones: &[&str],
        purchase: &[&str],
        allowed_machine_types: Option<Vec<String>>,
    ) -> Result<Self, SettingsError> {
        let project = Project::new(project)?;
        let mut parsed: Vec<Zone> = Vec::new();
        for name in zones {
            let zone = Zone::new(name)?;
            if parsed.contains(&zone) {
                return Err(SettingsError::ZoneTwice(zone.as_str().to_owned()));
            }
            parsed.push(zone);
        }
        let mut kinds = Vec::new();
        for text in purchase {
            let kind = PurchaseKind::parse(text)?;
            if kinds.contains(&kind) {
                return Err(SettingsError::PurchaseTwice(kind.name().to_owned()));
            }
            kinds.push(kind);
        }
        match (parsed.is_empty(), kinds.is_empty()) {
            (true, _) => Err(SettingsError::NoZone),
            (false, true) => Err(SettingsError::NoPurchase),
            (false, false) => Ok(Self {
                project,
                zones: parsed,
                purchase: kinds,
                allowed_machine_types: allowed_machine_types
                    .map(|names| names.into_iter().map(MachineTypeName).collect()),
            }),
        }
    }

    /// The project.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// The zones, in the configured order.
    pub fn zones(&self) -> &[Zone] {
        &self.zones
    }

    /// The provisioning models, in the configured order.
    pub fn purchase(&self) -> &[PurchaseKind] {
        &self.purchase
    }

    /// The regions of the zones.
    fn regions(&self) -> BTreeSet<Region> {
        self.zones.iter().map(Zone::region).collect()
    }

    /// The accelerators `info` is offered with: with an allow list, nothing for a type it
    /// does not name, and for one it names the type as it is (with or without built-in
    /// accelerators); with none, the type as it is only when it has built-in accelerators.
    /// Then, either way, one per accelerator of `offered` (the zone's accelerator types)
    /// and number of it that the type may have attached. `None` stands for the type as it
    /// is.
    fn candidates(
        &self,
        info: &MachineTypeInfo,
        offered: &[&AcceleratorTypeInfo],
    ) -> Vec<Option<AttachedAccelerator>> {
        let listed = self
            .allowed_machine_types
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&info.name));
        if !listed {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        if self.allowed_machine_types.is_some() || !info.accelerators.is_empty() {
            candidates.push(None);
        }
        for accelerator in offered {
            let counts = gpu::attachable_counts(
                &info.name,
                &accelerator.name,
                accelerator.max_per_instance,
                info.vcpus,
                info.memory_mib,
            );
            candidates.extend(counts.into_iter().map(|count| {
                Some(AttachedAccelerator {
                    type_name: accelerator.name.clone(),
                    count,
                })
            }));
        }
        candidates
    }
}

/// Something the catalog could not read and did without: a region's quotas, so the offers
/// of that region carry no quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogNote {
    /// The region.
    pub region: Region,
    /// How the read failed.
    pub error: ApiError,
}

/// A catalog that was read: the offers, and the notes of what it did without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAnswer {
    /// The offers, ordered by machine type, zone, the accelerators (built in or none
    /// first, then attached ones by name and number) and the provisioning model (in the
    /// configured order).
    pub offers: Vec<Offer>,
    /// What could not be read; the offers stand without it.
    pub notes: Vec<CatalogNote>,
}

/// The Compute Engine catalog of one project's zones over the port: the read half of the
/// Compute Engine provider.
pub struct GceCatalog {
    api: Arc<dyn GceApi>,
    settings: GceCatalogSettings,
}

/// The function that names `method` on an error of its call.
fn failed(method: Method) -> impl Fn(ApiError) -> CallError {
    move |error| CallError { method, error }
}

/// The order of the records of machine and accelerator types: by name, then zone.
fn by_name_and_zone<'a, N: Ord>(name: &'a N, zone: &'a Zone) -> (&'a N, &'a Zone) {
    (name, zone)
}

impl GceCatalog {
    /// The catalog of `settings` over `api`.
    pub fn new(api: Arc<dyn GceApi>, settings: GceCatalogSettings) -> Self {
        Self { api, settings }
    }

    /// What Compute Engine could launch now in the zones, as offers in the scheduler's
    /// vocabulary.
    ///
    /// The machine types, the accelerator types and the prices of every region of the
    /// zones must all be read; quotas need not be. With an allow list the catalog holds
    /// every type it names, with or without accelerators; with none, only the offers with
    /// an accelerator (a zone has hundreds of machine types, and the list is how the
    /// configuration selects CPU types). A type whose architecture cannot be determined,
    /// or whose accelerator has no name, has no offer, and is logged.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] naming the method that failed and the cloud's error reason when
    /// one of the reads the catalog cannot do without fails, or a price in an answer
    /// cannot be read. A failure is never an empty catalog, and never a part of one.
    pub fn catalog(&self) -> Result<CatalogAnswer, ProviderError> {
        self.read()
            .map_err(|error| ProviderError::new(error.to_string()))
    }

    fn read(&self) -> Result<CatalogAnswer, CallError> {
        let settings = &self.settings;
        let project = settings.project();
        let mut types = self
            .api
            .machine_types(project, settings.zones())
            .map_err(failed(Method::MachineTypes))?;
        let mut accelerators = self
            .api
            .accelerator_types(project, settings.zones())
            .map_err(failed(Method::AcceleratorTypes))?;
        let mut prices = BTreeMap::new();
        for region in settings.regions() {
            let read = self
                .api
                .prices(&region)
                .and_then(|skus| Prices::read(&skus))
                .map_err(failed(Method::Prices))?;
            prices.insert(region, read);
        }
        types.retain(|info| settings.zones().contains(&info.zone));
        types.sort_by(|a, b| {
            by_name_and_zone(&a.name, &a.zone).cmp(&by_name_and_zone(&b.name, &b.zone))
        });
        types.dedup_by(|a, b| (&a.name, &a.zone) == (&b.name, &b.zone));
        accelerators.sort_by(|a, b| {
            by_name_and_zone(&a.name, &a.zone).cmp(&by_name_and_zone(&b.name, &b.zone))
        });
        accelerators.dedup_by(|a, b| (&a.name, &a.zone) == (&b.name, &b.zone));
        let none = Prices::default();
        let mut quotas = Quotas::default();
        let mut offers = Vec::new();
        let mut unknown = BTreeSet::new();
        let mut no_arch = 0usize;
        for info in &types {
            let offered: Vec<&AcceleratorTypeInfo> = accelerators
                .iter()
                .filter(|accelerator| accelerator.zone == info.zone)
                .collect();
            let candidates = settings.candidates(info, &offered);
            if candidates.is_empty() {
                continue;
            }
            let region = info.zone.region();
            let region_prices = prices.get(&region).unwrap_or(&none);
            let region_quotas = quotas.of(&*self.api, project, &region);
            for attached in &candidates {
                let accelerator = attached.as_ref().or(info.accelerators.first());
                let machine = match translate::machine(info, accelerator) {
                    Ok(machine) => machine,
                    Err(Dropped::Arch) => {
                        no_arch += 1;
                        break;
                    }
                    Err(Dropped::Unnamed) => {
                        tracing::warn!(machine_type = %info.name, "an accelerator with no name: no offer");
                        continue;
                    }
                };
                if let Some(spelling) = &machine.unknown_accelerator
                    && unknown.insert(spelling.clone())
                {
                    tracing::warn!(
                        spelling = %spelling,
                        "an accelerator name the translation does not know passes through as its spelling"
                    );
                }
                offers.extend(translate::offers_for(
                    info,
                    attached.as_ref(),
                    &machine,
                    region_prices,
                    region_quotas,
                    settings.purchase(),
                ));
            }
        }
        if no_arch > 0 {
            tracing::warn!(
                count = no_arch,
                "machine types whose architecture cannot be determined have no offer"
            );
        }
        Ok(CatalogAnswer {
            offers,
            notes: quotas.notes,
        })
    }
}

/// The quotas read in one catalog call, each region once, and the notes of the reads that
/// failed.
#[derive(Default)]
struct Quotas {
    read: BTreeMap<Region, Option<RegionQuotas>>,
    notes: Vec<CatalogNote>,
}

impl Quotas {
    /// The quotas of `region`, read the first time a region is asked for. A read that
    /// fails leaves the region without quotas and adds a note.
    fn of(
        &mut self,
        api: &dyn GceApi,
        project: &Project,
        region: &Region,
    ) -> Option<&RegionQuotas> {
        let notes = &mut self.notes;
        self.read
            .entry(region.clone())
            .or_insert_with(|| match api.region_quotas(project, region) {
                Ok(metrics) => Some(RegionQuotas::read(&metrics)),
                Err(error) => {
                    notes.push(CatalogNote {
                        region: region.clone(),
                        error,
                    });
                    None
                }
            })
            .as_ref()
    }
}
