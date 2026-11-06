//! The provider: [`CapacityProvider`] over [`Ec2Api`], with the launch guard.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture,
    Claim, Extra, Held, Hello, HostName, HostRef, HostStatus, Observed, Offer, ProviderCaps,
    ProviderError, Purchase, Reason, Refusal, RefusalCode, Released,
};
use js_types::{Model, ProviderId, accelerator_of};

use crate::api::Ec2Api;
use crate::catalog::{Ec2Catalog, Ec2CatalogSettings};
use crate::clock::Clock;
use crate::config::{Ec2Config, Mode};
use crate::error::ApiError;
use crate::gpu::GPU_ROWS;
use crate::identity::{AwsIdentityVerifier, IdentityVerifier};
use crate::launch::{
    ACQUIRE_TAG, BLOCK_END_TAG, BlockPurchase, CapacityReservation, ClientToken, DescribeFilter,
    ImageId, Instance, InstanceId, LaunchPurchase, NAME_TAG, ReservationState, RunRequest,
    SCHEDULER_TAG, Tag, UserData,
};
use crate::model::{InstanceType, Zone};
use crate::own::Owner;
use crate::refusal::launch_answer;

mod records;

use records::Records;

/// The provider's name: the kind a configuration names it by.
pub const PROVIDER_ID: ProviderId = ProviderId::known("ec2");

/// What a catalog answers when its blocking API call did not come back (it panicked).
const CATALOG_CALL_LOST: &str = "the catalog call did not finish";

/// The most bytes of boot payload this provider carries: the user data of a launch, which
/// EC2 limits to 16 KB before it is base64 encoded (the EC2 user guide, "Run commands on
/// your Linux instance at launch").
pub const PAYLOAD_LIMIT: usize = 16_384;

/// The launches sent that got no answer, by acquire id, with the time they were sent. An
/// entry ends when a read shows the instance, when the launch is answered, or when the
/// hold runs out. The map sits behind the launch lock: holding the lock across the read
/// and the launch is what lets one launch run at a time.
type Unknown = BTreeMap<String, Duration>;

/// How an acquire buys its instance, before anything is bought.
#[derive(Debug)]
enum Buy {
    /// On demand.
    OnDemand,
    /// Spot.
    Spot,
    /// A Capacity Block of the offering with this id, bought first.
    Block { offering: String },
}

/// What a launch needs but its purchase: everything of a [`RunRequest`] that is known
/// before a Capacity Block is bought, and how the instance is to be bought.
struct Prepared {
    image: ImageId,
    instance_type: InstanceType,
    zone: Zone,
    buy: Buy,
    user_data: UserData,
    tags: Vec<Tag>,
    client_token: ClientToken,
}

impl Prepared {
    /// The launch of this instance bought as `purchase`.
    fn run(&self, purchase: LaunchPurchase) -> RunRequest {
        RunRequest {
            image: self.image.clone(),
            instance_type: self.instance_type.clone(),
            zone: self.zone.clone(),
            purchase,
            user_data: self.user_data.clone(),
            tags: self.tags.clone(),
            client_token: self.client_token.clone(),
        }
    }
}

/// What a read of one instance by id found.
enum Found {
    /// The instance, in the state the cloud shows.
    Row(Instance),
    /// The cloud said it does not know the instance.
    Gone,
    /// No answer.
    Unknown,
}

impl Found {
    /// What the scheduler reads from this.
    fn observed(&self) -> Observed {
        match self {
            Self::Row(row) => Observed::Present(row.state.status()),
            Self::Gone => Observed::Absent,
            Self::Unknown => Observed::Unknown,
        }
    }
}

/// The EC2 provider for one scheduler.
pub struct Ec2Provider {
    api: Arc<dyn Ec2Api>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Ec2Catalog>,
    config: Ec2Config,
    owner: Owner,
    verifier: Arc<dyn IdentityVerifier>,
    /// The launch lock and what it protects.
    unknown: Mutex<Unknown>,
    dry_run_logged: AtomicBool,
    /// What it knows of the instances it holds, for the calls that make no request. Never
    /// held while a request is in flight, so those calls never wait for one; taken after
    /// the launch lock when both are held.
    records: Mutex<Records>,
}

impl Ec2Provider {
    /// A provider that calls `api`, reads time from `clock`, offers what `catalog` selects
    /// and obeys `config`. It checks the identity of a machine against the key AWS
    /// publishes for instance identity documents ([`Self::with_verifier`] sets another).
    pub fn new(
        api: Arc<dyn Ec2Api>,
        clock: Arc<dyn Clock>,
        catalog: Ec2CatalogSettings,
        config: Ec2Config,
    ) -> Self {
        let owner = Owner::new(config.scheduler.clone());
        Self {
            catalog: Arc::new(Ec2Catalog::new(Arc::clone(&api), catalog)),
            api,
            clock,
            config,
            owner,
            verifier: Arc::new(AwsIdentityVerifier::new()),
            unknown: Mutex::new(Unknown::new()),
            dry_run_logged: AtomicBool::new(false),
            records: Mutex::new(Records::default()),
        }
    }

    /// The same provider checking the identity of a machine with `verifier`.
    #[must_use]
    pub fn with_verifier(mut self, verifier: Arc<dyn IdentityVerifier>) -> Self {
        self.verifier = verifier;
        self
    }

    fn unknown(&self) -> MutexGuard<'_, Unknown> {
        self.unknown.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn records(&self) -> MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn refuse(code: RefusalCode, reason: Reason) -> Acquired {
        Acquired::Refused(Refusal { code, reason })
    }

    /// What an acquire of `offer` would launch: its instance type (the part of the offer id
    /// before the first `/`), its zone and how it is bought. `None` when the offer has no
    /// zone, is not on demand, spot or a Capacity Block with its terms, or is of a type the
    /// allow list leaves out.
    fn launchable(&self, offer: &Offer) -> Option<(InstanceType, Zone, Buy)> {
        let zone = Zone(offer.zone.clone()?);
        let kind = InstanceType(offer.id.split('/').next().unwrap_or_default().to_owned());
        let purchase = match (&offer.purchase, &offer.block) {
            (Some(Purchase::OnDemand), _) => Buy::OnDemand,
            (Some(Purchase::Spot), _) => Buy::Spot,
            (Some(Purchase::CapacityBlock), Some(terms)) => Buy::Block {
                offering: terms.offering.clone(),
            },
            _ => return None,
        };
        let allowed = self
            .config
            .allowed
            .as_ref()
            .is_none_or(|types| types.contains(&kind));
        allowed.then_some((kind, zone, purchase))
    }

    /// Each of `rows` with the claim its marks make, as the scheduler holds it, and the
    /// image it runs.
    fn claimed(&self, rows: &[Instance]) -> Vec<(Claim, Held, ImageId)> {
        rows.iter()
            .map(|row| (self.owner.claim(row), held_of(row), row.image.clone()))
            .collect()
    }

    /// The tags that select what this scheduler launched, and with `acquire` what it
    /// launched for that acquire.
    fn selecting(&self, acquire: Option<&str>) -> DescribeFilter {
        let scheduler = tag(SCHEDULER_TAG, self.config.scheduler.as_str());
        let acquire = acquire.map(|id| tag(ACQUIRE_TAG, id));
        DescribeFilter::Tags(std::iter::once(scheduler).chain(acquire).collect())
    }

    /// Reads one instance by id.
    fn find(&self, id: &InstanceId) -> Found {
        match self
            .api
            .describe_instances(&DescribeFilter::Ids(vec![id.clone()]))
        {
            Ok(rows) => rows
                .into_iter()
                .find(|row| row.id == *id)
                .map_or(Found::Unknown, Found::Row),
            Err(error) if error.is_not_found() => Found::Gone,
            Err(error) => {
                tracing::warn!(%error, %id, "the read of an instance failed");
                Found::Unknown
            }
        }
    }

    /// Terminates one instance, if it is ours, and reads it back.
    fn release_instance(&self, id: &InstanceId) -> Released {
        let row = match self.find(id) {
            Found::Row(row) => row,
            Found::Gone => return Released::Gone,
            Found::Unknown => return Released::Unknown,
        };
        let claim = self.owner.claim(&row);
        if claim != Claim::Ours {
            return Released::Refused(claim);
        }
        // Tags can change between this read and the terminate below. The window is one
        // request wide and is accepted.
        if let Err(error) = self.api.terminate_instances(std::slice::from_ref(id)) {
            tracing::warn!(%error, %id, "terminate failed");
            return Released::Unknown;
        }
        match self.find(id) {
            Found::Row(after) => Released::Terminating(after.state.status()),
            Found::Gone => Released::Gone,
            Found::Unknown => Released::Unknown,
        }
    }

    /// The Capacity Block for the acquire `wanted`, under the launch lock held as
    /// `unknown`: the reservation an earlier attempt of this acquire bought, found by its
    /// tags, else the one a purchase of `offering` makes. A block bought twice is paid
    /// twice and the purchase has no client token, so the read comes first, and a read that
    /// fails buys nothing. The reservation is returned only when it is active; any other
    /// answer is the acquire's, with the launch not sent.
    fn reserve(
        &self,
        unknown: &mut Unknown,
        wanted: &str,
        offering: &str,
    ) -> Result<CapacityReservation, Box<Acquired>> {
        let tags = vec![
            tag(SCHEDULER_TAG, self.config.scheduler.as_str()),
            tag(ACQUIRE_TAG, wanted),
        ];
        let found = match self.api.capacity_reservations(&tags) {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "no launch: the capacity reservations of this acquire cannot be listed");
                unknown.remove(wanted);
                return Err(Box::new(Acquired::Unknown));
            }
        };
        let reservation = match found.into_iter().next() {
            Some(earlier) => earlier,
            None => {
                tracing::info!(offering, "buying a capacity block");
                let purchase = BlockPurchase {
                    offering: offering.to_owned(),
                    tags,
                };
                match self.api.purchase_capacity_block(&purchase) {
                    Ok(bought) => bought,
                    Err(error) => {
                        // A purchase that got no answer may have landed; the unknown entry
                        // stays, so a retry is deferred until the hold passes, and then
                        // reads the reservations before it buys.
                        let answer = launch_answer(&error);
                        if matches!(answer, Acquired::Refused(_)) {
                            unknown.remove(wanted);
                        }
                        tracing::warn!(%error, "the purchase of a capacity block failed");
                        return Err(Box::new(answer));
                    }
                }
            }
        };
        if reservation.state != ReservationState::Active {
            unknown.remove(wanted);
            tracing::warn!(
                id = %reservation.id,
                state = reservation.state.name(),
                "no launch: the capacity reservation is not active"
            );
            return Err(Box::new(Self::refuse(
                RefusalCode::Capacity,
                Reason::Api(reservation.state.name().to_owned()),
            )));
        }
        Ok(reservation)
    }

    /// The launch of `prepared` for `request`, under the launch lock held as `unknown`: it
    /// answers the machine a read of the acquire shows, defers while an earlier launch of
    /// it is unresolved, refuses at the cap and half-marked instances, buys the Capacity
    /// Block of a block offer, and otherwise sends the launch.
    fn launch_locked(
        &self,
        unknown: &mut Unknown,
        request: &AcquireRequest,
        prepared: &Prepared,
    ) -> Acquired {
        let wanted = request.acquire.as_str();
        let rows = match self.api.describe_instances(&self.selecting(Some(wanted))) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "no launch: the instances of this acquire cannot be listed");
                return Acquired::Unknown;
            }
        };
        let now = self.clock.now();
        let hold = self.config.unknown_hold;
        unknown.retain(|_, sent| now.saturating_sub(*sent) < hold);
        self.records().unresolved(unknown.keys().cloned().collect());
        let claimed = self.claimed(&rows);
        let live: Vec<&(Claim, Held, ImageId)> = claimed
            .iter()
            .filter(|(_, held, _)| held.status != HostStatus::Terminated)
            .collect();
        let half = live
            .iter()
            .filter(|(claim, _, _)| *claim == Claim::Suspect)
            .count();
        let earlier = live
            .iter()
            .find(|(claim, _, _)| *claim == Claim::Ours)
            .map(|(_, held, _)| held.clone());
        self.records().seen(claimed);
        if half > 0 {
            tracing::error!(
                count = half,
                "no launch: instances of this acquire carry some of this scheduler's marks, not all"
            );
            return Self::refuse(RefusalCode::Denied, Reason::HalfMarked { count: half });
        }
        if let Some(held) = earlier {
            unknown.remove(wanted);
            return Acquired::Accepted {
                held,
                extras: Vec::new(),
            };
        }
        if let Some(sent) = unknown.get(wanted) {
            // An earlier launch of this acquire got no answer and no read shows it yet: it
            // may have landed. A second launch would be a second instance under the same
            // acquire id, and one `max_machines` does not count.
            let wait = (*sent + hold).saturating_sub(now);
            tracing::warn!(
                ?wait,
                "no launch: an earlier launch of this acquire is unresolved"
            );
            return Acquired::Deferred { wait };
        }
        let counted = self.records().counted();
        let max_machines = self.config.max_machines.get();
        if counted >= max_machines {
            tracing::warn!(
                held = counted,
                max_machines,
                "no launch: max_machines is reached"
            );
            return Self::refuse(
                RefusalCode::Denied,
                Reason::AtMaxMachines {
                    held: counted,
                    max_machines,
                },
            );
        }
        unknown.insert(wanted.to_owned(), now);
        let (run, ends) = match &prepared.buy {
            Buy::OnDemand => (prepared.run(LaunchPurchase::OnDemand), None),
            Buy::Spot => (prepared.run(LaunchPurchase::Spot), None),
            Buy::Block { offering } => match self.reserve(unknown, wanted, offering) {
                Ok(reservation) => {
                    let mut run = prepared.run(LaunchPurchase::CapacityBlock(reservation.id));
                    run.tags.push(tag(
                        BLOCK_END_TAG,
                        &reservation.end_unix_seconds.to_string(),
                    ));
                    (run, Some(reservation.end_unix_seconds))
                }
                Err(answer) => return *answer,
            },
        };
        tracing::info!(
            instance_type = %run.instance_type,
            zone = %run.zone.as_str(),
            name = %request.name,
            purchase = ?run.purchase,
            "launching one instance"
        );
        let ids = match self.api.run_instances(&run) {
            Ok(ids) => ids,
            Err(error) => {
                let answer = launch_answer(&error);
                if matches!(answer, Acquired::Refused(_)) {
                    unknown.remove(wanted);
                }
                tracing::warn!(%error, "the launch failed");
                return answer;
            }
        };
        let Some((first, rest)) = ids.split_first() else {
            tracing::warn!("the launch answered success with no instance");
            return Acquired::Unknown;
        };
        unknown.remove(wanted);
        let held = Held {
            id: first.host_ref(),
            acquire: wanted.to_owned(),
            name: Some(request.name.to_string()),
            offer: request.offer.id.clone(),
            zone: Some(run.zone.as_str().to_owned()),
            status: HostStatus::Booting,
            ends_unix_seconds: ends,
        };
        // A launch asks for one instance; any other id it returns is an extra, released at
        // once and counted all the same.
        let mut extras = Vec::new();
        for id in rest {
            tracing::error!(%id, "a launch started more than one instance; releasing the extra");
            // A not-found right after a launch may be a read that lags the write, so for
            // an extra it is not evidence of "gone".
            let released = match self.release_instance(id) {
                Released::Gone => Released::Unknown,
                other => other,
            };
            extras.push(Extra {
                id: id.host_ref(),
                released,
            });
        }
        let mut records = self.records();
        records.launched(held.clone(), run.image.clone());
        for extra in &extras {
            let more = Held {
                id: extra.id.clone(),
                ..held.clone()
            };
            records.launched(more, run.image.clone());
            records.released(&extra.id, &extra.released);
        }
        Acquired::Accepted { held, extras }
    }
}

impl CapacityProvider for Ec2Provider {
    fn id(&self) -> ProviderId {
        PROVIDER_ID
    }

    fn can_launch(&self) -> bool {
        true
    }

    /// Linux, `x86_64` and `arm64`, and the canonical accelerator model of each GPU the
    /// translation table of the crate names. A GPU the table does not name passes through
    /// as its own spelling and reaches the catalog as such; once a catalog has carried it,
    /// it is in the capabilities too, so every offer the catalog has returned is inside
    /// them. No call is made.
    fn capabilities(&self) -> ProviderCaps {
        let records = self.records();
        let mut arch = records.arches();
        arch.extend(["x86_64".to_owned(), "arm64".to_owned()]);
        let mut accelerators: BTreeSet<Model> = GPU_ROWS
            .iter()
            .filter_map(|(_, _, _, spelling)| Model::parse(spelling).ok())
            .map(|spelling| accelerator_of(&spelling, 1, None).model)
            .collect();
        accelerators.extend(records.models());
        ProviderCaps {
            os: BTreeSet::from(["linux".to_owned()]),
            arch,
            accelerators,
            // A machine runs the agent, so it can be had whole and its jobs may declare
            // host ports.
            whole_host: true,
            host_ports: true,
        }
    }

    /// The API calls are blocking, so the catalog runs on tokio's blocking pool; awaiting
    /// the answer needs a tokio runtime context. A call that panics answers an error. The
    /// quota reads that failed are logged and the offers stand without a quota.
    fn catalog(&self) -> CatalogFuture<'_> {
        Box::pin(async move {
            let catalog = Arc::clone(&self.catalog);
            let answer = tokio::task::spawn_blocking(move || catalog.catalog())
                .await
                .map_err(|_| ProviderError::new(CATALOG_CALL_LOST))??;
            for note in &answer.notes {
                tracing::warn!(
                    family = ?note.family,
                    purchase = ?note.purchase,
                    error = %note.error,
                    "a quota read failed; the offers carry no quota"
                );
            }
            self.records().offered(&answer.offers);
            Ok(answer.offers)
        })
    }

    /// From what the provider last listed, launched, released and read by id; `can_add` is
    /// what `max_machines` leaves once the launch guard's count is taken off it (ours and
    /// not terminated, the suspects and the launches with no answer).
    fn capacity(&self) -> Capacity {
        self.records().capacity(self.config.max_machines.get())
    }

    /// Nothing: the machine image carries the driver stack and the agent's requirements,
    /// so the boot payload has no step to add for an offer.
    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        PAYLOAD_LIMIT
    }

    // `request.lifetime` is not read: `RunInstances` has no run-duration limit, so only
    // `max_age` with a live scheduler bounds the cost. An instance in a Capacity Block ends
    // with the block, whatever the lifetime.
    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        if !self.config.enabled {
            return Self::refuse(RefusalCode::Denied, Reason::Disabled);
        }
        let Some((instance_type, zone, buy)) = self.launchable(&request.offer) else {
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        };
        if !self.owner.owns_name(&request.name) {
            return Self::refuse(RefusalCode::Shape, Reason::Name);
        }
        let (len, limit) = (request.payload.len(), self.payload_limit());
        if len > limit {
            return Self::refuse(RefusalCode::Shape, Reason::PayloadTooLarge { len, limit });
        }
        let Some(image) = self.config.images.get(&request.offer.arch) else {
            tracing::warn!(
                arch = %request.offer.arch,
                instance_type = %instance_type,
                "no image is configured for this architecture"
            );
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        };
        let tags = match self
            .owner
            .tags(&request.acquire, &request.name, &request.labels)
        {
            Ok(tags) => tags,
            Err(why) => return Self::refuse(RefusalCode::Shape, Reason::Label(why.to_string())),
        };
        if self.config.mode == Mode::DryRun {
            // Once at info level, then quietly: a pool asks again every tick.
            match self.dry_run_logged.swap(true, Ordering::SeqCst) {
                false => tracing::info!(
                    instance_type = %instance_type,
                    zone = %zone.as_str(),
                    name = %request.name,
                    tags = tags.len(),
                    "dry run: a launch would be sent"
                ),
                true => tracing::debug!(name = %request.name, "dry run: a launch would be sent"),
            }
            return Self::refuse(RefusalCode::Denied, Reason::DryRun);
        }
        let prepared = Prepared {
            image: image.clone(),
            instance_type,
            zone,
            buy,
            user_data: UserData::new(request.payload.expose().as_bytes().to_vec()),
            tags,
            client_token: ClientToken::derive(&request.acquire, &self.config.scheduler),
        };
        let mut unknown = self.unknown();
        let answer = self.launch_locked(&mut unknown, request, &prepared);
        self.records().unresolved(unknown.keys().cloned().collect());
        answer
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let rows = self
            .api
            .describe_instances(&self.selecting(None))
            .map_err(provider_error)?;
        let claimed = self.claimed(&rows);
        let ours = claimed
            .iter()
            .filter(|(claim, held, _)| {
                matches!(claim, Claim::Ours) & (held.status != HostStatus::Terminated)
            })
            .map(|(_, held, _)| held.clone())
            .collect();
        self.records().listed(claimed);
        Ok(ours)
    }

    fn release(&self, id: &HostRef) -> Released {
        let released = match InstanceId::new(id.as_str()) {
            Some(instance) => self.release_instance(&instance),
            // Not an id of this cloud, so not an instance of it and not ours: a definite
            // answer, never "gone" and never one to retry. Nothing is asked of the cloud.
            None => Released::Refused(Claim::Foreign),
        };
        self.records().released(id, &released);
        released
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        let unknown = self.unknown();
        unknown
            .keys()
            .filter_map(|id| AcquireId::new(id).ok())
            .collect()
    }

    /// A reference that is no id of the cloud is not read and is `Unknown`; only a
    /// not-found answer is `Absent`.
    fn observe(&self, id: &HostRef) -> Observed {
        let Some(instance) = InstanceId::new(id.as_str()) else {
            return Observed::Unknown;
        };
        let observed = self.find(&instance).observed();
        self.records().observed(id, &observed);
        observed
    }

    /// The instance launched, or listed, under the hello's name, which must be one this
    /// scheduler derives: with the hello's instance id when it carries one, else the one
    /// such instance that is not ending (none when several are). A hello that carries a
    /// signed identity must also carry one the verifier accepts, naming that instance, the
    /// configured region and the image the instance was launched from.
    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        let name = HostName::parse(&self.config.scheduler, &hello.name)?;
        let found = self.records().verify(&name, hello.instance_id.as_deref())?;
        let Some(identity) = &hello.identity else {
            return Some(found);
        };
        let document = self.verifier.verify(identity)?;
        let records = self.records();
        let image = records.image_of(&found);
        [
            document.instance_id == found.as_str(),
            document.region == self.config.region.as_str(),
            image.is_some_and(|image| image.as_str() == document.image_id),
        ]
        .into_iter()
        .all(|same| same)
        .then_some(found)
    }
}

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn provider_error(error: ApiError) -> ProviderError {
    ProviderError::new(error.to_string())
}

/// The scheduler's view of an instance. Its offer is the instance type: a read of an
/// instance does not say which offer it was launched from. Its block end is the number in
/// the [`BLOCK_END_TAG`] tag; an instance with no such tag, or one that holds no number,
/// has none.
fn held_of(instance: &Instance) -> Held {
    Held {
        id: instance.id.host_ref(),
        acquire: Owner::acquire_of(instance).unwrap_or_default().to_owned(),
        name: instance.tag(NAME_TAG).map(str::to_owned),
        offer: instance.instance_type.to_string(),
        zone: Some(instance.zone.as_str().to_owned()),
        status: instance.state.status(),
        ends_unix_seconds: instance.tag(BLOCK_END_TAG).and_then(|end| end.parse().ok()),
    }
}
