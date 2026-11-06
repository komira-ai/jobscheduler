//! The conformance cases that need no fault injection, run against a live provider, with a
//! step that launches, lists, reads and releases one machine, and a cleanup that always
//! runs.
//!
//! A provider crate wires its real client to [`LiveCloud`] and calls [`run`] from a thin
//! binary; this module holds everything else, so the logic is tested here with an
//! in-memory provider. It reads no clock and sleeps nowhere: the provider's calls wait as
//! they wait, and the caller passes a [`LiveArgs`] it parsed from its own command line.
//!
//! [`run`] is a dry run unless the arguments carry `--spend` and the plan hash the dry
//! run printed. A dry run builds the provider, reads the catalog, finds the offer the
//! `--shape` flag names, prints the plan (what will be launched, what it can cost at most,
//! and every provider flag) and the hash of the plan text, runs the one read-only case, lists the machines the scheduler already has, and
//! launches nothing. A spend run launches three machines, one at a time, each released
//! before the next, and then lists and releases whatever the scheduler still has. The plan
//! hash changes when the chosen offer, its price or a provider flag changes, so a spend run
//! never spends more than the plan its hash was taken from said, and never runs with
//! another image or project than the dry run read.
//!
//! The cases that need a fault the cloud has to be made to produce (a refused launch, a
//! lost answer, a failing list) are not run: if a case calls one, [`LiveRig`] panics with
//! the hook's name followed by `is not available against a live cloud`.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};

use sha2::{Digest, Sha256};

use super::{
    Rig, accepted, accepted_means_launched_not_ready, acquire_id,
    acquire_is_idempotent_on_the_acquire_id, block_on,
    capabilities_cover_every_offer_the_catalog_returns,
};
use crate::answer::{Held, HostStatus, Observed, RefusalCode, Released};
use crate::ids::{AcquireId, HostName, HostRef, SchedulerId};
use crate::offer::Offer;
use crate::provider::{AcquireRequest, CapacityProvider};
use crate::secret::BootPayload;

/// The label key every acquire of a live run carries; the value is the run id. A machine
/// that a run left behind is found in the provider's console by this label.
pub const RUN_LABEL: &str = "validation-run";

/// The pool name on the requests of a live run.
const POOL: &str = "live-conformance";

/// The most characters of a run id.
const RUN_ID_MAX: usize = 40;

/// The flags this module reads itself; any other flag is an extra the caller names.
const OWN_FLAGS: [&str; 6] = ["run-id", "region", "zone", "shape", "spend", "plan-hash"];

/// How many machines the spend run launches, one at a time: one per case that launches.
const LAUNCHES: u64 = 3;

/// The arguments of a live run, read from a command line by [`LiveArgs::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveArgs {
    /// The id of this run, 1 to 40 characters of `a-z`, `0-9` and `-`, not starting or
    /// ending with `-`. It names the scheduler of the run and is the value of
    /// [`RUN_LABEL`].
    pub run_id: String,
    /// The place to launch: a region, or a zone, as the provider's offers spell theirs.
    pub location: String,
    /// The offer id to launch. The run never picks an offer itself.
    pub shape: String,
    /// The plan hash a dry run printed. `Some` only when `--spend` was given.
    pub plan_hash: Option<String>,
    /// The values of the extra flags the caller declared, by name without the dashes.
    pub extra: BTreeMap<String, String>,
    /// The scheduler of the run, made once from the run id.
    scheduler: SchedulerId,
}

impl LiveArgs {
    /// Reads the arguments (without the program name).
    ///
    /// The flags are `--run-id ID` (required), exactly one of `--region R` and
    /// `--zone Z` (kept as [`LiveArgs::location`]), `--shape S` (required), the bare `--spend`, which
    /// needs `--plan-hash H` (64 lowercase hexadecimal digits), and `--plan-hash`, which
    /// needs `--spend`. `extra` names more flags, each `--NAME VALUE`, all optional here;
    /// the provider crate asks for the ones it needs with [`LiveArgs::required`]. A name in
    /// `extra` that is also one of the flags above is read as that flag.
    ///
    /// # Errors
    ///
    /// A message naming the flag, for a flag given twice, an unknown flag, an argument
    /// that is no flag, a flag without a value, a run id or plan hash that is badly
    /// formed, and a missing or conflicting `--run-id`, `--region`/`--zone`, `--shape`,
    /// `--spend` or `--plan-hash`.
    pub fn parse(args: &[String], extra: &[&str]) -> Result<Self, String> {
        let mut seen = BTreeSet::new();
        let mut spend = false;
        let mut values = BTreeMap::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!(
                    "unexpected argument {arg}: every argument is a --flag"
                ));
            };
            if !OWN_FLAGS.contains(&name) && !extra.contains(&name) {
                return Err(format!("unknown flag --{name}"));
            }
            if !seen.insert(name) {
                return Err(format!("--{name} is given twice"));
            }
            if name == "spend" {
                spend = true;
                continue;
            }
            match rest.next() {
                Some(value) if !value.is_empty() && !value.starts_with("--") => {
                    values.insert(name.to_owned(), value.clone());
                }
                _ => return Err(format!("--{name} needs a value")),
            }
        }
        let run_id = values.remove("run-id").ok_or("--run-id is required")?;
        let scheduler = valid_run_id(&run_id)
            .then(|| SchedulerId::new(&format!("live-{run_id}")).ok())
            .flatten()
            .ok_or_else(|| {
                format!(
                    "--run-id must be 1 to {RUN_ID_MAX} characters of a-z, 0-9 and '-', \
                     not starting or ending with '-'"
                )
            })?;
        let location = match (values.remove("region"), values.remove("zone")) {
            (Some(place), None) | (None, Some(place)) => place,
            (None, None) => return Err("one of --region or --zone is required".to_owned()),
            (Some(_), Some(_)) => {
                return Err("--region and --zone cannot both be given".to_owned());
            }
        };
        let shape = values.remove("shape").ok_or("--shape is required")?;
        let plan_hash = values.remove("plan-hash");
        if let Some(hash) = &plan_hash {
            let hex = hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !(hex && hash.len() == 64) {
                return Err("--plan-hash must be 64 lowercase hexadecimal digits".to_owned());
            }
        }
        let plan_hash = match (spend, plan_hash) {
            (true, None) => {
                return Err("--spend needs --plan-hash HASH, the hash a dry run printed".to_owned());
            }
            (false, Some(_)) => return Err("--plan-hash is only valid with --spend".to_owned()),
            (_, hash) => hash,
        };
        Ok(Self {
            run_id,
            location,
            shape,
            plan_hash,
            extra: values,
            scheduler,
        })
    }

    /// The scheduler of the run: `live-` and the run id.
    pub fn scheduler(&self) -> SchedulerId {
        self.scheduler.clone()
    }

    /// The value of the extra flag `--name`, which the provider needs.
    ///
    /// # Errors
    ///
    /// A message naming `--name` when it was not given.
    pub fn required(&self, name: &str) -> Result<&str, String> {
        self.extra
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

/// Whether `text` is a run id.
fn valid_run_id(text: &str) -> bool {
    let alphabet = text
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
    alphabet
        && (1..=RUN_ID_MAX).contains(&text.len())
        && !text.starts_with('-')
        && !text.ends_with('-')
}

/// The cloud a live run launches in: how to build a provider on it.
pub trait LiveCloud {
    /// A fresh provider in live mode (launching for real), which remembers nothing, for
    /// `scheduler`, with room for `machines` machines.
    ///
    /// # Errors
    ///
    /// A message when the provider cannot be built (a missing credential, an unreadable
    /// key file).
    fn provider(
        &self,
        scheduler: &SchedulerId,
        machines: NonZeroUsize,
    ) -> Result<Box<dyn CapacityProvider>, String>;

    /// Whether the machine image of the run can boot `offer`. The default is every offer;
    /// a provider whose image covers one architecture says which offers it can boot.
    fn launchable(&self, _offer: &Offer) -> bool {
        true
    }
}

/// The text a panic carried.
fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else {
        "a panic that carries no text".to_owned()
    }
}

/// Writes one line. A failed write is ignored: the run does not depend on what it
/// printed, and a spend run is cleaned up whether or not anyone could read the output.
fn say(out: &mut dyn Write, text: &str) {
    let _ = writeln!(out, "{text}");
}

/// Whether `zone` is in `location`: it is the location (a zone and itself), or the location
/// and one lowercase letter (`us-east-1a` in `us-east-1`), or the location, a `-` and one
/// lowercase letter (`us-central1-a` in `us-central1`). `us-east-10a` is not in `us-east-1`.
fn zone_in(zone: &str, location: &str) -> bool {
    zone.strip_prefix(location)
        .is_some_and(|rest| matches!(rest.as_bytes(), [] | [b'a'..=b'z'] | [b'-', b'a'..=b'z']))
}

/// Whether an offer is in `location`: it has no zone, or its zone is in the location.
fn is_here(offer: &Offer, location: &str) -> bool {
    offer
        .zone
        .as_deref()
        .is_none_or(|zone| zone_in(zone, location))
}

/// The offer the run launches, the one `--shape` names, and its price in US cents per hour.
fn choose<'a>(
    args: &LiveArgs,
    cloud: &dyn LiveCloud,
    offers: &'a [Offer],
) -> Result<(&'a Offer, u64), String> {
    let place = &args.location;
    let shape = &args.shape;
    let offer = offers
        .iter()
        .find(|offer| offer.id == *shape && is_here(offer, place))
        .ok_or_else(|| format!("shape {shape} is not in the catalog for {place}"))?;
    if !offer.available {
        return Err(format!(
            "shape {shape} is in the catalog for {place} but is not available"
        ));
    }
    if !cloud.launchable(offer) {
        return Err(format!(
            "shape {shape} is in the catalog but the machine image cannot boot it"
        ));
    }
    let price = offer
        .price_cents_per_hour
        .ok_or_else(|| format!("shape {shape} has no known price; spending needs one"))?;
    Ok((offer, price))
}

/// The plan text: what the run will launch and the most it can cost. Its hash is the
/// plan hash.
fn plan_text(
    args: &LiveArgs,
    provider: &dyn CapacityProvider,
    offer: &Offer,
    price: u64,
) -> String {
    let zone = offer.zone.as_deref().unwrap_or("none");
    let purchase = offer
        .purchase
        .as_ref()
        .map_or_else(|| "unstated".to_owned(), |purchase| format!("{purchase:?}"));
    let flags = if args.extra.is_empty() {
        "none".to_owned()
    } else {
        args.extra
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let lines = [
        "live conformance plan".to_owned(),
        format!("provider: {}", provider.id()),
        format!("run id: {}", args.run_id),
        format!("scheduler: {}", args.scheduler.as_str()),
        format!("location: {}", args.location),
        format!("provider flags: {flags}"),
        format!(
            "offer: {} zone {zone} arch {} purchase {purchase} price {price} US cents per hour",
            offer.id, offer.arch
        ),
        format!(
            "launches: {LAUNCHES}, one at a time (acquire_is_idempotent_on_the_acquire_id, \
             accepted_means_launched_not_ready, list_and_observe_see_the_machine)"
        ),
        format!(
            "cost bound: {} US cents if each machine is gone within its first hour; \
             this assumes hourly or finer billing and is not a quote",
            price.saturating_mul(LAUNCHES)
        ),
        format!(
            "labels stamped: {RUN_LABEL}={}, plus the scheduler marks the provider adds",
            args.run_id
        ),
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The sha-256 of `text`, in lowercase hexadecimal.
fn hash_of(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Releases every machine the provider lists and returns a line for each answer.
fn sweep(provider: &dyn CapacityProvider) -> Result<Vec<String>, String> {
    let held = provider
        .list()
        .map_err(|err| format!("the list failed: {err}"))?;
    Ok(held
        .iter()
        .map(|machine| {
            format!(
                "released {}: {:?}",
                machine.id,
                provider.release(&machine.id)
            )
        })
        .collect())
}

/// Panics as every fault hook of [`LiveRig`] does: the hook's name, then
/// `is not available against a live cloud`.
fn unavailable(hook: &str) -> ! {
    panic!("{hook} is not available against a live cloud")
}

/// The [`Rig`] of a live run: the provider is the real one, the offer is the one the run
/// chose, and nothing can be scripted, so every fault hook panics.
///
/// [`Rig::make_room`] releases every machine the provider lists, then builds a fresh
/// provider with room for the machines asked for plus those still terminating, which are
/// remembered so that [`Rig::launches`] does not count them. [`Rig::terminates`] panics
/// too: a live rig cannot count the terminate calls that reach the cloud, and the cases
/// [`run`] runs do not read it.
pub struct LiveRig<'a> {
    cloud: &'a dyn LiveCloud,
    scheduler: SchedulerId,
    run_id: String,
    offer: Offer,
    provider: Box<dyn CapacityProvider>,
    baseline: BTreeSet<HostRef>,
    read_only: bool,
}

impl<'a> LiveRig<'a> {
    /// A rig for the run `args` over `provider`, launching `offer`, building the providers
    /// that [`Rig::make_room`] asks for with `cloud`.
    pub fn new(
        args: &LiveArgs,
        cloud: &'a dyn LiveCloud,
        offer: Offer,
        provider: Box<dyn CapacityProvider>,
    ) -> Self {
        Self {
            cloud,
            scheduler: args.scheduler(),
            run_id: args.run_id.clone(),
            offer,
            provider,
            baseline: BTreeSet::new(),
            read_only: false,
        }
    }
}

impl Rig for LiveRig<'_> {
    fn provider(&self) -> &dyn CapacityProvider {
        self.provider.as_ref()
    }

    fn scheduler(&self) -> SchedulerId {
        self.scheduler.clone()
    }

    fn request(&self, acquire: &AcquireId) -> AcquireRequest {
        AcquireRequest {
            acquire: acquire.clone(),
            pool: POOL.to_owned(),
            name: HostName::derive(&self.scheduler, acquire),
            offer: self.offer.clone(),
            lifetime: None,
            labels: vec![(RUN_LABEL.to_owned(), self.run_id.clone())],
            payload: BootPayload::new(String::new()),
        }
    }

    /// Releases every machine the provider lists, remembers those still listed (they are
    /// terminating), and builds a fresh provider with room for `machines` more than
    /// those. A rig that only reads (the dry run's) does nothing here. A list or a build
    /// that fails panics.
    fn make_room(&mut self, machines: u32) {
        if self.read_only {
            return;
        }
        if let Err(err) = sweep(self.provider.as_ref()) {
            panic!("make_room: {err}");
        }
        let baseline: BTreeSet<HostRef> = match self.provider.list() {
            Ok(held) => held.into_iter().map(|machine| machine.id).collect(),
            Err(err) => panic!("make_room: the list after the release failed: {err}"),
        };
        let room = (machines as usize) + baseline.len();
        let room = NonZeroUsize::new(room).unwrap_or(NonZeroUsize::MIN);
        match self.cloud.provider(&self.scheduler, room) {
            Ok(provider) => self.provider = provider,
            Err(err) => panic!("make_room: cannot build a provider: {err}"),
        }
        self.baseline = baseline;
    }

    fn refuse_next(&mut self, _code: RefusalCode) {
        unavailable("refuse_next");
    }

    fn lose_next_answer_that_landed(&mut self) {
        unavailable("lose_next_answer_that_landed");
    }

    fn lose_next_answer_that_did_not_land(&mut self) {
        unavailable("lose_next_answer_that_did_not_land");
    }

    fn lose_next_launch_that_did_not_land(&mut self) {
        unavailable("lose_next_launch_that_did_not_land");
    }

    fn fail_read_after_next_terminate(&mut self) {
        unavailable("fail_read_after_next_terminate");
    }

    fn foreign_machine_exists(&mut self) -> Vec<HostRef> {
        unavailable("foreign_machine_exists")
    }

    fn fail_list(&mut self) {
        unavailable("fail_list");
    }

    fn forget(&mut self, _id: &HostRef) {
        unavailable("forget");
    }

    fn throttle_reads(&mut self) {
        unavailable("throttle_reads");
    }

    fn outwait_unresolved(&mut self) {
        unavailable("outwait_unresolved");
    }

    /// The machines the provider lists that were not listed at the last
    /// [`Rig::make_room`]. A list that fails panics.
    fn launches(&self) -> usize {
        match self.provider.list() {
            Ok(held) => held
                .iter()
                .filter(|machine| !self.baseline.contains(&machine.id))
                .count(),
            Err(err) => panic!("launches: the list failed: {err}"),
        }
    }

    fn terminates(&self) -> usize {
        unavailable("terminates")
    }
}

/// A machine launched, listed, read and released: the list holds it with a name that
/// carries the scheduler's prefix, a read by id finds it present, a release answers
/// `Terminating` or `Gone`, and a read after the release finds it terminating, terminated
/// or absent, never still up. Fails on a provider whose list leaves out what it launched or
/// names it wrongly, or whose release leaves the machine running.
///
/// A cloud whose reads lag behind its writes can fail this case on a machine that is
/// fine; the case reads each answer once.
pub fn list_and_observe_see_the_machine(rig: &mut dyn Rig) {
    const CASE: &str = "list_and_observe_see_the_machine";
    rig.make_room(1);
    let request = rig.request(&acquire_id("seen"));
    let held = accepted(CASE, &rig.provider().acquire(&request));
    let listed = rig.provider().list();
    assert!(listed.is_ok(), "{CASE}: the list answers {listed:?}");
    let listed: Vec<Held> = listed.unwrap_or_default();
    let found = listed.iter().find(|machine| machine.id == held.id);
    let Some(found) = found else {
        panic!(
            "{CASE}: the list {listed:?} does not hold the machine {}",
            held.id
        );
    };
    let prefix = HostName::prefix(&rig.scheduler());
    assert!(
        found
            .name
            .as_deref()
            .is_some_and(|name| name.starts_with(&prefix)),
        "{CASE}: the listed name {:?} does not start with the prefix {prefix}",
        found.name
    );
    let seen = rig.provider().observe(&held.id);
    assert!(
        matches!(seen, Observed::Present(_)),
        "{CASE}: a read of the launched machine answers {seen:?}"
    );
    let released = rig.provider().release(&held.id);
    assert!(
        matches!(released, Released::Terminating(_) | Released::Gone),
        "{CASE}: a release of the launched machine answers {released:?}"
    );
    let after = rig.provider().observe(&held.id);
    assert!(
        matches!(
            after,
            Observed::Absent | Observed::Present(HostStatus::Terminating | HostStatus::Terminated)
        ),
        "{CASE}: after {released:?} a read answers {after:?}; the machine must be \
         terminating, terminated or gone"
    );
}

/// A case of the live run.
type Case = fn(&mut dyn Rig);

/// The cases of the spend run, in order; each launches one machine.
const SPEND_CASES: [(&str, Case); 3] = [
    (
        "acquire_is_idempotent_on_the_acquire_id",
        acquire_is_idempotent_on_the_acquire_id,
    ),
    (
        "accepted_means_launched_not_ready",
        accepted_means_launched_not_ready,
    ),
    (
        "list_and_observe_see_the_machine",
        list_and_observe_see_the_machine,
    ),
];

/// Runs `case` and prints PASS or FAIL for it. A panic is the case failing.
fn attempt(out: &mut dyn Write, name: &str, case: impl FnOnce()) -> bool {
    match catch_unwind(AssertUnwindSafe(case)) {
        Ok(()) => {
            say(out, &format!("PASS {name}"));
            true
        }
        Err(payload) => {
            say(
                out,
                &format!("FAIL {name}: {}", panic_text(payload.as_ref())),
            );
            false
        }
    }
}

/// The read-only part of a run: the catalog case, and a list of the machines the scheduler
/// already has. Returns whether both worked.
fn read_only_checks(rig: &mut LiveRig<'_>, out: &mut dyn Write) -> bool {
    let name = "capabilities_cover_every_offer_the_catalog_returns";
    let covered = attempt(out, name, || {
        capabilities_cover_every_offer_the_catalog_returns(rig);
    });
    let listing = catch_unwind(AssertUnwindSafe(|| rig.provider.list()));
    let listed = match listing {
        Ok(Ok(held)) => {
            say(
                out,
                &format!("machines already listed for this scheduler: {}", held.len()),
            );
            for machine in &held {
                say(out, &format!("  {} {:?}", machine.id, machine.status));
            }
            true
        }
        Ok(Err(err)) => {
            say(out, &format!("FAIL the list of existing machines: {err}"));
            false
        }
        Err(payload) => {
            say(
                out,
                &format!(
                    "FAIL the list of existing machines panicked: {}",
                    panic_text(payload.as_ref())
                ),
            );
            false
        }
    };
    covered && listed
}

/// Releases every machine the scheduler has, always, whatever the cases did: builds a fresh
/// provider, lists, releases each machine, lists again. Returns whether nothing is left
/// running. A panic in the pass (a provider's list or release) is printed as a failed
/// cleanup that names the label to look for, and is not let out of the run.
fn cleanup(args: &LiveArgs, cloud: &dyn LiveCloud, out: &mut dyn Write) -> bool {
    match catch_unwind(AssertUnwindSafe(|| cleanup_pass(args, cloud, &mut *out))) {
        Ok(clean) => clean,
        Err(payload) => {
            say(
                out,
                &format!(
                    "CLEANUP FAILED: cleanup panicked: {}; check the console for machines \
                     labelled {RUN_LABEL}={}",
                    panic_text(payload.as_ref()),
                    args.run_id
                ),
            );
            false
        }
    }
}

/// The cleanup pass of [`cleanup`], which lets a panic out.
fn cleanup_pass(args: &LiveArgs, cloud: &dyn LiveCloud, out: &mut dyn Write) -> bool {
    say(out, "cleanup: releasing every machine of this scheduler");
    let find = format!(
        "terminate it in the provider console; the label {RUN_LABEL}={} finds it",
        args.run_id
    );
    let provider = match cloud.provider(&args.scheduler, NonZeroUsize::MIN) {
        Ok(provider) => provider,
        Err(err) => {
            say(
                out,
                &format!("CLEANUP FAILED: cannot build a provider to look for machines: {err}"),
            );
            say(out, &format!("Machines of this run may be running: {find}"));
            return false;
        }
    };
    match sweep(provider.as_ref()) {
        Ok(lines) => lines
            .iter()
            .for_each(|line| say(out, &format!("cleanup: {line}"))),
        Err(err) => {
            say(out, &format!("CLEANUP FAILED: {err}"));
            say(out, &format!("Machines of this run may be running: {find}"));
            return false;
        }
    }
    let left = match provider.list() {
        Ok(held) => held,
        Err(err) => {
            say(
                out,
                &format!("CLEANUP FAILED: the list after the release failed: {err}"),
            );
            say(out, &format!("Machines of this run may be running: {find}"));
            return false;
        }
    };
    let mut clean = true;
    for machine in &left {
        if !matches!(
            machine.status,
            HostStatus::Terminating | HostStatus::Terminated
        ) {
            say(
                out,
                &format!(
                    "LEFT OVER: machine {} is still {:?} and may be billing; {find}",
                    machine.id, machine.status
                ),
            );
            clean = false;
        }
    }
    if clean {
        say(
            out,
            &format!(
                "cleanup: {} machine(s) listed, none left running",
                left.len()
            ),
        );
    }
    clean
}

/// Runs the live conformance: the plan, the read-only checks, and, when `args` carries the
/// plan hash this plan has, the spend cases and a cleanup. Everything is printed to `out`.
///
/// Returns 0 when the run passed (a dry run: when its checks passed), and 1 when a case
/// failed, the provider or the catalog could not be read, the offer `--shape` names is
/// missing, unavailable, unbootable or unpriced, the plan hash did not match the plan, or a
/// machine was left over or the cleanup could not look (or panicked).
pub fn run(args: &LiveArgs, cloud: &dyn LiveCloud, out: &mut dyn Write) -> i32 {
    let provider = match cloud.provider(&args.scheduler, NonZeroUsize::MIN) {
        Ok(provider) => provider,
        Err(err) => {
            say(out, &format!("cannot build the provider: {err}"));
            return 1;
        }
    };
    let offers = match block_on(provider.catalog()) {
        Ok(offers) => offers,
        Err(err) => {
            say(out, &format!("the catalog failed: {err}"));
            return 1;
        }
    };
    let (offer, price) = match choose(args, cloud, &offers) {
        Ok(chosen) => chosen,
        Err(message) => {
            say(out, &message);
            return 1;
        }
    };
    let plan = plan_text(args, provider.as_ref(), offer, price);
    let hash = hash_of(&plan);
    let _ = out.write_all(plan.as_bytes());
    say(out, &format!("plan hash: {hash}"));

    let mut rig = LiveRig::new(args, cloud, offer.clone(), provider);
    rig.read_only = true;
    let checked = read_only_checks(&mut rig, out);
    match &args.plan_hash {
        None => {
            say(out, "dry run: nothing was launched");
            return i32::from(!checked);
        }
        Some(expected) if *expected != hash => {
            say(
                out,
                &format!(
                    "the plan changed: this plan hashes to {hash}, not {expected}; \
                     nothing was launched"
                ),
            );
            return 1;
        }
        Some(_) => {}
    }
    if !checked {
        say(out, "a read-only check failed: nothing was launched");
        return 1;
    }

    rig.read_only = false;
    let mut failed = 0;
    for (name, case) in SPEND_CASES {
        if !attempt(out, name, || case(&mut rig)) {
            failed += 1;
        }
    }
    say(
        out,
        &format!(
            "cases: {} passed, {failed} failed",
            SPEND_CASES.len() - failed
        ),
    );
    drop(rig);
    let clean = cleanup(args, cloud, out);
    i32::from(failed > 0 || !clean)
}
