//! The catalog: the cloud's answers read through the port, selected by the settings and
//! translated into offers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_elastic::{Offer, ProviderError, Quota, QuotaLimit};

use crate::api::{Action, Ec2Api, PurchaseKind};
use crate::error::{ApiError, CallError, SettingsError};
use crate::model::{BlockOffering, InstanceType, InstanceTypeInfo, Region, Zone};
use crate::price;
use crate::quota::{QuotaFamily, quota_family};
use crate::translate::{self, Dropped, Sale};

/// What the configuration selects for one EC2 catalog: the region, the purchase options in
/// order, the instance types it allows, and the length of a Capacity Block. It selects and
/// limits, and never says what an instance type provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ec2CatalogSettings {
    region: Region,
    purchase: Vec<PurchaseKind>,
    allowed_instance_types: Option<BTreeSet<InstanceType>>,
    capacity_block_hours: Option<u32>,
}

impl Ec2CatalogSettings {
    /// The settings of the `region` key, the ordered `purchase` list, the
    /// `allowed_instance_types` list (`None`: not set) and `capacity_block_hours`, the
    /// length of the Capacity Blocks to offer, read only when `purchase` lists
    /// `capacity-block`.
    ///
    /// # Errors
    ///
    /// [`SettingsError`] naming the key: a region that is not a region code, an empty
    /// purchase list, a purchase option that is not `on-demand`, `spot` or
    /// `capacity-block` or is listed twice, and `capacity-block` without a length of at
    /// least one hour.
    pub fn new(
        region: &str,
        purchase: &[&str],
        allowed_instance_types: Option<Vec<String>>,
        capacity_block_hours: Option<u32>,
    ) -> Result<Self, SettingsError> {
        let region = Region::new(region)?;
        let mut kinds = Vec::new();
        for text in purchase {
            let kind = PurchaseKind::parse(text)?;
            if kinds.contains(&kind) {
                return Err(SettingsError::PurchaseTwice(kind.name().to_owned()));
            }
            kinds.push(kind);
        }
        let blocks = kinds.contains(&PurchaseKind::CapacityBlock);
        let hours = capacity_block_hours.filter(|hours| *hours > 0);
        match (kinds.is_empty(), blocks, hours) {
            (true, _, _) => Err(SettingsError::NoPurchase),
            (false, true, None) => Err(SettingsError::BlockHours),
            (false, blocks, hours) => Ok(Self {
                region,
                purchase: kinds,
                allowed_instance_types: allowed_instance_types
                    .map(|names| names.into_iter().map(InstanceType).collect()),
                capacity_block_hours: hours.filter(|_| blocks),
            }),
        }
    }

    /// The region.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The purchase options, in the configured order.
    pub fn purchase(&self) -> &[PurchaseKind] {
        &self.purchase
    }

    fn wants(&self, kind: PurchaseKind) -> bool {
        self.purchase.contains(&kind)
    }

    /// Whether the catalog lists `info`: a type the allow list names, or, with no allow
    /// list, a type with a GPU.
    fn selects(&self, info: &InstanceTypeInfo) -> bool {
        match &self.allowed_instance_types {
            Some(allowed) => allowed.contains(&info.name),
            None => !info.gpus.is_empty(),
        }
    }
}

/// Something the catalog could not read and did without: a quota read that failed, so
/// the offers of that quota family and purchase option carry no quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogNote {
    /// The quota family.
    pub family: QuotaFamily,
    /// The purchase option.
    pub purchase: PurchaseKind,
    /// How the read failed.
    pub error: ApiError,
}

/// A catalog that was read: the offers, and the notes of what it did without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAnswer {
    /// The offers, ordered by instance type, zone and purchase option (in the configured
    /// order), then Capacity Block offering.
    pub offers: Vec<Offer>,
    /// What could not be read; the offers stand without it.
    pub notes: Vec<CatalogNote>,
}

/// The EC2 catalog of one region over the port: the read half of the EC2 provider.
pub struct Ec2Catalog {
    api: Arc<dyn Ec2Api>,
    settings: Ec2CatalogSettings,
}

/// The answers the catalog cannot do without.
struct Answers {
    types: Vec<InstanceTypeInfo>,
    zones: BTreeMap<InstanceType, BTreeSet<Zone>>,
    on_demand: BTreeMap<InstanceType, u64>,
    spot: BTreeMap<(InstanceType, Zone), u64>,
    blocks: BTreeMap<InstanceType, Vec<(BlockOffering, u64)>>,
}

/// The function that names `action` on an error of its call.
fn failed(action: Action) -> impl Fn(ApiError) -> CallError {
    move |error| CallError { action, error }
}

impl Ec2Catalog {
    /// The catalog of `settings` over `api`.
    pub fn new(api: Arc<dyn Ec2Api>, settings: Ec2CatalogSettings) -> Self {
        Self { api, settings }
    }

    /// What EC2 could launch now in the region, as offers in the scheduler's vocabulary.
    ///
    /// The instance types, the zone offerings and the prices of every purchase option the
    /// settings list (and the Capacity Block offerings, when they list `capacity-block`)
    /// must all be read; quotas need not be. With an allow list the catalog holds every
    /// type it names, GPU or not; with none, only the types with a GPU (every type of a
    /// region is thousands of offers, and a CPU job is served by registered hosts first).
    /// A type with no architecture the scheduler runs, or whose GPU has no name, has no
    /// offer, and is logged.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] naming the action that failed and the cloud's error code when one
    /// of the reads the catalog cannot do without fails, or a price in an answer is not a
    /// decimal. A failure is never an empty catalog, and never a part of one.
    pub fn catalog(&self) -> Result<CatalogAnswer, ProviderError> {
        self.read()
            .map_err(|error| ProviderError::new(error.to_string()))
    }

    fn answers(&self) -> Result<Answers, CallError> {
        let region = self.settings.region();
        let types = self
            .api
            .instance_types(region)
            .map_err(failed(Action::DescribeInstanceTypes))?;
        let mut zones: BTreeMap<InstanceType, BTreeSet<Zone>> = BTreeMap::new();
        for offering in self
            .api
            .zone_offerings(region)
            .map_err(failed(Action::DescribeInstanceTypeOfferings))?
        {
            zones
                .entry(offering.instance_type)
                .or_default()
                .insert(offering.zone);
        }
        let on_demand = match self.settings.wants(PurchaseKind::OnDemand) {
            true => self
                .api
                .on_demand_prices(region)
                .and_then(|prices| price::on_demand(&prices))
                .map_err(failed(Action::GetProducts))?,
            false => BTreeMap::new(),
        };
        let spot = match self.settings.wants(PurchaseKind::Spot) {
            true => self
                .api
                .spot_prices(region)
                .and_then(|prices| price::spot(&prices))
                .map_err(failed(Action::DescribeSpotPriceHistory))?,
            false => BTreeMap::new(),
        };
        let blocks = match self.settings.capacity_block_hours {
            Some(hours) => self
                .api
                .capacity_block_offerings(region, hours)
                .and_then(blocks_by_type)
                .map_err(failed(Action::DescribeCapacityBlockOfferings))?,
            None => BTreeMap::new(),
        };
        Ok(Answers {
            types,
            zones,
            on_demand,
            spot,
            blocks,
        })
    }

    fn read(&self) -> Result<CatalogAnswer, CallError> {
        let Answers {
            mut types,
            zones,
            on_demand,
            spot,
            blocks,
        } = self.answers()?;
        types.retain(|info| self.settings.selects(info));
        types.sort_by(|a, b| a.name.cmp(&b.name));
        types.dedup_by(|a, b| a.name == b.name);
        let mut quotas = Quotas::default();
        let mut offers = Vec::new();
        let mut unrunnable = 0usize;
        let none = BTreeSet::new();
        for info in &types {
            let machine = match translate::machine(info) {
                Ok(machine) => machine,
                Err(Dropped::Arch) => {
                    unrunnable += 1;
                    continue;
                }
                Err(Dropped::Unnamed) => {
                    tracing::warn!(instance_type = %info.name, "a GPU with no name: no offer");
                    continue;
                }
            };
            if let Some(spelling) = &machine.unknown_gpu {
                tracing::warn!(
                    instance_type = %info.name,
                    spelling = %spelling,
                    "a GPU name the translation does not know passes through as its spelling"
                );
            }
            let offered = zones.get(&info.name).unwrap_or(&none);
            let sale = Sale {
                zones: offered.iter().collect(),
                on_demand: on_demand.get(&info.name).copied(),
                spot: spot
                    .iter()
                    .filter(|((kind, _), _)| *kind == info.name)
                    .map(|((_, zone), cents)| (zone, *cents))
                    .collect(),
                quota: quotas.of(&*self.api, &self.settings, info, !offered.is_empty()),
                blocks: blocks
                    .get(&info.name)
                    .map(|list| {
                        list.iter()
                            .map(|(offering, cents)| (offering, *cents))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            offers.extend(translate::offers_for(
                &machine,
                &info.name,
                &sale,
                self.settings.purchase(),
            ));
        }
        if unrunnable > 0 {
            tracing::warn!(
                count = unrunnable,
                "instance types with no architecture the scheduler runs have no offer"
            );
        }
        Ok(CatalogAnswer {
            offers,
            notes: quotas.notes,
        })
    }
}

/// The Capacity Block offerings of each type with their upfront price in cents, sorted by
/// zone and offering id.
fn blocks_by_type(
    offerings: Vec<BlockOffering>,
) -> Result<BTreeMap<InstanceType, Vec<(BlockOffering, u64)>>, ApiError> {
    let mut table: BTreeMap<InstanceType, Vec<(BlockOffering, u64)>> = BTreeMap::new();
    for offering in offerings {
        let cents = price::upfront(&offering)?;
        table
            .entry(offering.instance_type.clone())
            .or_default()
            .push((offering, cents));
    }
    for list in table.values_mut() {
        list.sort_by(|(a, _), (b, _)| (&a.zone, &a.id).cmp(&(&b.zone, &b.id)));
    }
    Ok(table)
}

/// The quotas read in one catalog call, each (family, purchase option) once, and the notes
/// of the reads that failed.
#[derive(Default)]
struct Quotas {
    read: BTreeMap<(QuotaFamily, PurchaseKind), Option<Quota>>,
    notes: Vec<CatalogNote>,
}

impl Quotas {
    /// The quota of each of on-demand and spot that the settings list, for `info`, read
    /// once per family and option. None is read for a type that has no zone to sell it in
    /// or no quota family. A read that fails leaves the option without a quota and adds a
    /// note.
    fn of(
        &mut self,
        api: &dyn Ec2Api,
        settings: &Ec2CatalogSettings,
        info: &InstanceTypeInfo,
        offered: bool,
    ) -> BTreeMap<PurchaseKind, Quota> {
        let family = quota_family(&info.name).filter(|_| offered);
        let Some(family) = family else {
            return BTreeMap::new();
        };
        [PurchaseKind::OnDemand, PurchaseKind::Spot]
            .into_iter()
            .filter(|kind| settings.wants(*kind))
            .filter_map(|kind| {
                let quota = self.read.entry((family, kind)).or_insert_with(|| {
                    match api.vcpu_quota(settings.region(), family, kind) {
                        Ok(quota) => quota.map(|quota| Quota {
                            vcpus: Some(QuotaLimit {
                                limit: quota.limit,
                                in_use: quota.in_use,
                            }),
                            accelerators: None,
                        }),
                        Err(error) => {
                            self.notes.push(CatalogNote {
                                family,
                                purchase: kind,
                                error,
                            });
                            None
                        }
                    }
                });
                (*quota).map(|quota| (kind, quota))
            })
            .collect()
    }
}
