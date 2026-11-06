//! The provider: [`CapacityProvider`] over [`GceApi`], with the launch guard.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture,
    Claim, Held, Hello, HostName, HostRef, HostStatus, Observed, Offer, ProviderCaps,
    ProviderError, Purchase, Reason, Refusal, RefusalCode, Released,
};
use js_types::{Model, ProviderId, accelerator_of};

use crate::api::GceApi;
use crate::catalog::{GceCatalog, GceCatalogSettings};
use crate::clock::Clock;
use crate::config::{GceConfig, Mode};
use crate::error::ApiError;
use crate::gpu::ACCELERATOR_ROWS;
use crate::identity::{GoogleIdentityVerifier, IdentityVerifier};
use crate::launch::{
    HostMaintenance, InsertRequest, Instance, InstanceId, InstanceKey, InstanceName,
    InstanceRef, Label, ListFilter, Operation, Payload, ProvisioningModel, RequestId, Scheduling,
    TerminationAction, host_ref, parse_host_ref,
};
use crate::model::{AcceleratorTypeName, AttachedAccelerator, MachineTypeName, Zone};
use crate::own::Owner;
use crate::refusal::launch_answer;

mod records;

use records::Records;

/// The provider's name: the kind a configuration names it by.
pub const PROVIDER_ID: ProviderId = ProviderId::known("gce");

/// What a catalog answers when its blocking API call did not come back (it panicked).
const CATALOG_CALL_LOST: &str = "the catalog call did not finish";

/// The most bytes of boot payload this provider carries: one metadata value, which
/// Compute Engine limits to 256 KB (the Compute Engine documentation, "About VM metadata":
/// "the size limit of a single metadata value is 256 KB").
pub const PAYLOAD_LIMIT: usize = 256 * 1024;

/// Bytes in a GiB: a boot disk is sized in whole GiB (`diskSizeGb`).
const GIB: u64 = 1 << 30;

/// The inserts sent that got no answer, by acquire id, with the time they were sent. An
/// entry ends when a read shows the instance, when the insert is answered, or when the
/// hold runs out. The map sits behind the launch lock: holding the lock across the read
/// and the insert is what lets one launch run at a time.
type Unknown = BTreeMap<String, Duration>;

/// What a read of one instance found.
enum Found {
    /// The instance, in the status the cloud shows.
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
            Self::Row(row) => Observed::Present(row.status.status()),
            Self::Gone => Observed::Absent,
            Self::Unknown => Observed::Unknown,
        }
    }
}

/// What an acquire of an offer would launch.
struct Launchable {
    zone: Zone,
    machine_type: MachineTypeName,
    accelerators: Option<AttachedAccelerator>,
    model: ProvisioningModel,
}

/// The Compute Engine provider for one scheduler.
pub struct GceProvider {
    api: Arc<dyn GceApi>,
    clock: Arc<dyn Clock>,
    catalog: Arc<GceCatalog>,
    zones: Vec<Zone>,
    config: GceConfig,
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

impl GceProvider {
    /// A provider that calls `api`, reads time from `clock`, offers what `catalog` selects
    /// and obeys `config`. It checks the identity of a machine against the keys Google
    /// signs identity tokens with that the crate pins ([`Self::with_verifier`] sets
    /// another check).
    pub fn new(
        api: Arc<dyn GceApi>,
        clock: Arc<dyn Clock>,
        catalog: GceCatalogSettings,
        config: GceConfig,
    ) -> Self {
        let owner = Owner::new(&config.scheduler);
        Self {
            zones: catalog.zones().to_vec(),
            catalog: Arc::new(GceCatalog::new(Arc::clone(&api), catalog)),
            api,
            clock,
            config,
            owner,
            verifier: Arc::new(GoogleIdentityVerifier::new()),
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

    /// What an acquire of `offer` would launch: the machine type and the attached
    /// accelerators its id names (`<type>/<model>` or `<type>+<accelerator>x<n>/<model>`),
    /// its zone and how it is bought. `None` when the offer has no zone or one the
    /// configuration does not list, is not on demand or Spot, or is of a type the allow
    /// list leaves out.
    fn launchable(&self, offer: &Offer) -> Option<Launchable> {
        let zone = Zone::new(offer.zone.as_deref()?).ok()?;
        let model = match offer.purchase {
            Some(Purchase::OnDemand) => ProvisioningModel::Standard,
            Some(Purchase::Spot) => ProvisioningModel::Spot,
            _ => return None,
        };
        let head = offer.id.split('/').next().unwrap_or_default();
        let (machine_type, accelerators) = match head.split_once('+') {
            None => (head, None),
            Some((machine_type, attached)) => {
                let (name, count) = attached.rsplit_once('x')?;
                let count = count.parse::<u32>().ok().filter(|count| *count > 0)?;
                let accelerator = AttachedAccelerator {
                    type_name: AcceleratorTypeName(name.to_owned()),
                    count,
                };
                (machine_type, Some(accelerator))
            }
        };
        let machine_type = MachineTypeName(machine_type.to_owned());
        let allowed = self
            .config
            .allowed
            .as_ref()
            .is_none_or(|types| types.contains(&machine_type));
        let listed = self.zones.contains(&zone);
        (allowed & listed & !machine_type.as_str().is_empty()).then_some(Launchable {
            zone,
            machine_type,
            accelerators,
            model,
        })
    }

    /// Each of `rows` with the claim its marks make, as the scheduler holds it.
    fn claimed(&self, rows: &[Instance]) -> Vec<(Claim, Held)> {
        rows.iter()
            .map(|row| (self.owner.claim(row), held_of(row)))
            .collect()
    }

    /// The labels that select what this scheduler launched, and with `acquire` what it
    /// launched for that acquire.
    fn selecting(&self, acquire: Option<&AcquireId>) -> Vec<Label> {
        let acquire = acquire.map(Owner::acquire_label);
        std::iter::once(self.owner.scheduler_label())
            .chain(acquire)
            .collect()
    }

    /// Every instance of the project that carries `labels`, page by page to the end.
    ///
    /// # Errors
    ///
    /// The error of the first page that fails, and [`ApiError::Incomplete`] when a page
    /// names a scope it could not reach or the cloud answers the token it was just given.
    fn walk(&self, labels: Vec<Label>) -> Result<Vec<Instance>, ApiError> {
        let mut filter = ListFilter {
            project: self.config.project.clone(),
            labels,
            page_token: None,
        };
        let mut all = Vec::new();
        loop {
            let page = self.api.list_instances(&filter)?;
            if page.unreachable {
                return Err(ApiError::Incomplete);
            }
            all.extend(page.items);
            match page.next_page_token {
                None => return Ok(all),
                Some(next) if filter.page_token.as_deref() == Some(next.as_str()) => {
                    return Err(ApiError::Incomplete);
                }
                Some(next) => filter.page_token = Some(next),
            }
        }
    }

    /// The reference of the instance `id` in `zone` of the project.
    fn instance(&self, zone: Zone, instance: InstanceKey) -> InstanceRef {
        InstanceRef {
            project: self.config.project.clone(),
            zone,
            instance,
        }
    }

    /// Reads one instance.
    fn find(&self, reference: &InstanceRef) -> Found {
        match self.api.get_instance(reference) {
            Ok(row) => Found::Row(row),
            Err(error) if error.is_not_found() => Found::Gone,
            Err(error) => {
                tracing::warn!(%error, instance = reference.instance.as_str(), "the read of an instance failed");
                Found::Unknown
            }
        }
    }

    /// Deletes one instance, if it is ours, and reads it back.
    fn release_instance(&self, zone: Zone, id: InstanceId) -> Released {
        let reference = self.instance(zone, InstanceKey::Id(id));
        let row = match self.find(&reference) {
            Found::Row(row) => row,
            Found::Gone => return Released::Gone,
            Found::Unknown => return Released::Unknown,
        };
        let claim = self.owner.claim(&row);
        if claim != Claim::Ours {
            return Released::Refused(claim);
        }
        // Labels can change between this read and the delete below. The window is one
        // request wide and is accepted.
        if let Err(error) = self.api.delete_instance(&reference) {
            tracing::warn!(%error, instance = reference.instance.as_str(), "the delete failed");
            return Released::Unknown;
        }
        match self.find(&reference) {
            Found::Row(after) => Released::Terminating(after.status.status()),
            Found::Gone => Released::Gone,
            Found::Unknown => Released::Unknown,
        }
    }

    /// The answer of an insert whose operation finished with an error: the first of its
    /// codes that is a refusal, else unknown. A refusal ends the acquire's hold.
    fn failed_operation(unknown: &mut Unknown, wanted: &str, operation: &Operation) -> Acquired {
        let answer = operation
            .error_codes
            .iter()
            .map(|code| launch_answer(&ApiError::answered(code.as_str())))
            .find(|answer| matches!(answer, Acquired::Refused(_)))
            .unwrap_or(Acquired::Unknown);
        if matches!(answer, Acquired::Refused(_)) {
            unknown.remove(wanted);
        }
        let codes: Vec<&str> = operation.error_codes.iter().map(|code| code.as_str()).collect();
        tracing::warn!(?codes, "the insert's operation finished with an error");
        answer
    }

    /// The answer of an insert the cloud refused because the name is taken: the instance
    /// of that name, read by name, when it is ours; refused when it carries only some of
    /// our marks; unknown when it cannot be read.
    fn name_taken(&self, unknown: &mut Unknown, wanted: &str, insert: &InsertRequest) -> Acquired {
        let reference = self.instance(insert.zone.clone(), InstanceKey::Name(insert.name.clone()));
        let row = match self.find(&reference) {
            Found::Row(row) => row,
            Found::Gone | Found::Unknown => {
                tracing::warn!("the name of the insert is taken and the instance cannot be read");
                return Acquired::Unknown;
            }
        };
        let claim = self.owner.claim(&row);
        let held = held_of(&row);
        self.records().seen([(claim, held.clone())]);
        unknown.remove(wanted);
        match claim {
            Claim::Ours => Acquired::Accepted {
                held,
                extras: Vec::new(),
            },
            Claim::Suspect | Claim::Foreign => {
                tracing::error!("no launch: the instance of this name carries some of this scheduler's marks, not all");
                Self::refuse(RefusalCode::Denied, Reason::HalfMarked { count: 1 })
            }
        }
    }

    /// The launch of `insert` for `request`, under the launch lock held as `unknown`: it
    /// answers the machine a read of the acquire shows, defers while an earlier launch of
    /// it is unresolved, refuses at the cap and half-marked instances, and otherwise sends
    /// the insert and reads its operation once.
    fn launch_locked(
        &self,
        unknown: &mut Unknown,
        request: &AcquireRequest,
        insert: &InsertRequest,
    ) -> Acquired {
        let wanted = request.acquire.as_str();
        let rows = match self.walk(self.selecting(Some(&request.acquire))) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "no launch: the instances of this acquire cannot be listed");
                return Acquired::Unknown;
            }
        };
        let now = self.clock.now();
        let hold = self.config.unknown_hold;
        unknown.retain(|_, sent| now.saturating_sub(*sent) < hold);
        self.records().unresolved(self.unresolved_names(unknown));
        let claimed = self.claimed(&rows);
        let live: Vec<&(Claim, Held)> = claimed
            .iter()
            .filter(|(_, held)| held.status != HostStatus::Terminated)
            .collect();
        let half = live
            .iter()
            .filter(|(claim, _)| *claim == Claim::Suspect)
            .count();
        let earlier = live
            .iter()
            .find(|(claim, _)| *claim == Claim::Ours)
            .map(|(_, held)| held.clone());
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
            // An earlier insert of this acquire got no answer and no read shows it yet: it
            // may have landed. A second insert would be a second instance under the same
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
        tracing::info!(
            machine_type = %insert.machine_type,
            zone = %insert.zone,
            name = %insert.name,
            model = insert.scheduling.provisioning_model.name(),
            "inserting one instance"
        );
        let operation = match self.api.insert_instance(insert) {
            Ok(operation) => operation,
            Err(error) if error.is_already_exists() => {
                return self.name_taken(unknown, wanted, insert);
            }
            Err(error) => {
                let answer = launch_answer(&error);
                if matches!(answer, Acquired::Refused(_)) {
                    unknown.remove(wanted);
                }
                tracing::warn!(%error, "the insert failed");
                return answer;
            }
        };
        if operation.failed() {
            return Self::failed_operation(unknown, wanted, &operation);
        }
        // The insert's answer says the operation was accepted, not that it went through:
        // its outcome is read once.
        let read = match self.api.get_operation(&operation.reference) {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%error, "the insert's operation cannot be read");
                return Acquired::Unknown;
            }
        };
        if read.failed() {
            return Self::failed_operation(unknown, wanted, &read);
        }
        let Some(id) = read.target_id.or(operation.target_id) else {
            tracing::warn!("the insert's operation names no instance");
            return Acquired::Unknown;
        };
        unknown.remove(wanted);
        let held = Held {
            id: host_ref(&insert.zone, &id),
            acquire: wanted.to_owned(),
            name: Some(insert.name.to_string()),
            offer: request.offer.id.clone(),
            zone: Some(insert.zone.as_str().to_owned()),
            status: HostStatus::Booting,
        };
        self.records().launched(held.clone());
        Acquired::Accepted {
            held,
            extras: Vec::new(),
        }
    }

    /// The names of the instances of the acquires with an insert that got no answer.
    fn unresolved_names(&self, unknown: &Unknown) -> BTreeSet<String> {
        unknown
            .keys()
            .filter_map(|id| AcquireId::new(id).ok())
            .map(|acquire| HostName::derive(&self.config.scheduler, &acquire).to_string())
            .collect()
    }
}

/// The `scheduling` of an instance of `model`: no restart and termination on host
/// maintenance for every instance, since a restarted machine would boot the agent again
/// with a spent join token; deletion when the cloud ends it, for Spot (so a preempted
/// machine does not linger as `TERMINATED`) and whenever the pool bounds its `lifetime`,
/// which becomes the run duration, in whole seconds rounded up.
fn scheduling(model: ProvisioningModel, lifetime: Option<Duration>) -> Scheduling {
    let ends = (model == ProvisioningModel::Spot) | lifetime.is_some();
    Scheduling {
        provisioning_model: model,
        on_host_maintenance: HostMaintenance::Terminate,
        automatic_restart: false,
        termination_action: ends.then_some(TerminationAction::Delete),
        max_run_duration: lifetime
            .map(|span| Duration::from_secs(span.as_secs() + u64::from(span.subsec_nanos() > 0))),
    }
}

impl CapacityProvider for GceProvider {
    fn id(&self) -> ProviderId {
        PROVIDER_ID
    }

    fn can_launch(&self) -> bool {
        true
    }

    /// Linux, `x86_64` and `arm64`, and the canonical accelerator model of each accelerator
    /// the translation table of the crate names. An accelerator the table does not name
    /// passes through as its own spelling and reaches the catalog as such; once a catalog
    /// has carried it, it is in the capabilities too, so every offer the catalog has
    /// returned is inside them. No call is made.
    fn capabilities(&self) -> ProviderCaps {
        let records = self.records();
        let mut arch = records.arches();
        arch.extend(["x86_64".to_owned(), "arm64".to_owned()]);
        let mut accelerators: BTreeSet<Model> = ACCELERATOR_ROWS
            .iter()
            .filter_map(|(_, spelling, _)| Model::parse(spelling).ok())
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
                    region = %note.region,
                    error = %note.error,
                    "a quota read failed; the offers carry no quota"
                );
            }
            self.records().offered(&answer.offers);
            Ok(answer.offers)
        })
    }

    /// From what the provider last listed, launched, released and read; `can_add` is what
    /// `max_machines` leaves once the launch guard's count is taken off it (ours and not
    /// terminated, the suspects and the inserts with no answer).
    fn capacity(&self) -> Capacity {
        self.records().capacity(self.config.max_machines.get())
    }

    /// Nothing: the machine image carries the agent's requirements, so the boot payload
    /// has no step to add for an offer.
    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        PAYLOAD_LIMIT
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        if !self.config.enabled {
            return Self::refuse(RefusalCode::Denied, Reason::Disabled);
        }
        let Some(launch) = self.launchable(&request.offer) else {
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        };
        let name = InstanceName::new(request.name.as_str())
            .filter(|_| self.owner.owns_name(request.name.as_str()));
        let Some(name) = name else {
            return Self::refuse(RefusalCode::Shape, Reason::Name);
        };
        let (len, limit) = (request.payload.len(), self.payload_limit());
        if len > limit {
            return Self::refuse(RefusalCode::Shape, Reason::PayloadTooLarge { len, limit });
        }
        let Some(image) = self.config.images.get(&request.offer.arch) else {
            tracing::warn!(
                arch = %request.offer.arch,
                machine_type = %launch.machine_type,
                "no image is configured for this architecture"
            );
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        };
        let labels = match self.owner.labels(&request.acquire, &request.labels) {
            Ok(labels) => labels,
            Err(why) => return Self::refuse(RefusalCode::Shape, Reason::Label(why.to_string())),
        };
        if self.config.mode == Mode::DryRun {
            // Once at info level, then quietly: a pool asks again every tick.
            match self.dry_run_logged.swap(true, Ordering::SeqCst) {
                false => tracing::info!(
                    machine_type = %launch.machine_type,
                    zone = %launch.zone,
                    name = %name,
                    labels = labels.len(),
                    "dry run: an insert would be sent"
                ),
                true => tracing::debug!(name = %name, "dry run: an insert would be sent"),
            }
            return Self::refuse(RefusalCode::Denied, Reason::DryRun);
        }
        let disk = request.offer.disk_bytes;
        let insert = InsertRequest {
            project: self.config.project.clone(),
            zone: launch.zone,
            name,
            machine_type: launch.machine_type,
            scheduling: scheduling(launch.model, request.lifetime),
            image: image.clone(),
            disk_size_gb: (disk > 0).then(|| disk.div_ceil(GIB)),
            accelerators: launch.accelerators,
            labels,
            payload: Payload::new(request.payload.expose().to_owned()),
            network: self.config.network.clone(),
            request_id: RequestId::derive(&request.acquire, &self.config.scheduler),
        };
        let mut unknown = self.unknown();
        let answer = self.launch_locked(&mut unknown, request, &insert);
        self.records().unresolved(self.unresolved_names(&unknown));
        answer
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let rows = self.walk(self.selecting(None)).map_err(provider_error)?;
        let claimed = self.claimed(&rows);
        let ours = claimed
            .iter()
            .filter(|(claim, held)| {
                matches!(claim, Claim::Ours) & (held.status != HostStatus::Terminated)
            })
            .map(|(_, held)| held.clone())
            .collect();
        self.records().listed(claimed);
        Ok(ours)
    }

    fn release(&self, id: &HostRef) -> Released {
        let released = match parse_host_ref(id) {
            Some((zone, instance)) => self.release_instance(zone, instance),
            // Not a reference of this cloud, so not an instance of it and not ours: a
            // definite answer, never "gone" and never one to retry. Nothing is asked of
            // the cloud.
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

    /// A reference that is no reference of the cloud is not read and is `Unknown`; only a
    /// `notFound` answer is `Absent`.
    fn observe(&self, id: &HostRef) -> Observed {
        let Some((zone, instance)) = parse_host_ref(id) else {
            return Observed::Unknown;
        };
        let observed = self.find(&self.instance(zone, InstanceKey::Id(instance))).observed();
        self.records().observed(id, &observed);
        observed
    }

    /// The instance launched, or listed, under the hello's name, which must be one this
    /// scheduler derives: with the hello's instance id when it carries one (the reference
    /// `<zone>/<id>`, or the id alone), else the one such instance that is not ending
    /// (none when several are). A hello that carries a signed identity must also carry one
    /// the verifier accepts, whose claims name the configured project and that instance's
    /// zone, id and name.
    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        let name = HostName::parse(&self.config.scheduler, &hello.name)?;
        let found = self.records().verify(&name, hello.instance_id.as_deref())?;
        let Some(identity) = &hello.identity else {
            return Some(found);
        };
        let claims = self.verifier.verify(identity)?;
        let (zone, id) = parse_host_ref(&found)?;
        [
            claims.project_id == self.config.project.as_str(),
            claims.zone == zone.as_str(),
            claims.instance_id == id.as_str(),
            claims.instance_name == name.as_str(),
        ]
        .into_iter()
        .all(|same| same)
        .then_some(found)
    }
}

fn provider_error(error: ApiError) -> ProviderError {
    ProviderError::new(error.to_string())
}

/// The scheduler's view of an instance. Its offer is the machine type: a read of an
/// instance does not say which offer it was launched from.
fn held_of(instance: &Instance) -> Held {
    Held {
        id: instance.host_ref(),
        acquire: Owner::acquire_of(instance).unwrap_or_default().to_owned(),
        name: Some(instance.name.clone()),
        offer: instance.machine_type.to_string(),
        zone: Some(instance.zone.as_str().to_owned()),
        status: instance.status.status(),
    }
}
