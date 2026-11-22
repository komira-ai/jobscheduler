//! What a crun agent takes before its jobs start and what its hello reports: the host and
//! scratch-root claims, its egress, and the runtime's report. Part of [`super`], whose items
//! it re-exports.

use js_agent_wire::Hello;

use super::{Backend, Start};
use crate::contract;
use crate::lifecycle::Local;
use crate::{Failure, Services, crun, integrity};

/// The roots a crun agent holds while it runs: the claim of its host id and of its scratch
/// root, released when dropped.
pub(crate) type Claims = Option<(crun::HostClaim, crun::HostClaim)>;

/// What a crun agent takes before its jobs start: the image store and its claim files go in
/// the directory above the scratch root, and one crun agent may hold each scratch root and
/// each host id (the start-up sweep and the lease names go by them); then its egress starts,
/// once (after the claims: the start keeps what an earlier process left and loads one transaction,
/// and `Egress::settle`, called after the runs are adopted, removes what no run took over; an
/// agent that still runs must not be another's, so the claims come first; mode none starts
/// nothing).
/// An agent of the engine, or one that runs no job, takes nothing.
///
/// # Errors
///
/// [`Failure::Usage`] naming the flag at fault.
pub(crate) fn claim(
    local: &mut Local,
    services: &Services<'_>,
    host_id: &str,
) -> Result<Claims, Failure> {
    let crun = local.backend.as_ref().and_then(|b| b.settings().crun());
    let claims = match crun {
        Some(plan) => {
            // The store may be on another file system: a lease's directories (its `/work` and
            // home) are made in the scratch root, beside the run's own directory, and the store
            // holds only the images and the helpers' state, which no job writes.
            crun::prepare_store(&plan.claim_dir, &plan.store).map_err(Failure::Usage)?;
            let (dir, root) = (&plan.claim_dir, &plan.scratch_root);
            crun::require_trusted_ancestors(root, services.euid, services.ancestors_floor)
                .map_err(|why| Failure::Usage(format!("--scratch-root: {why}")))?;
            let host = crun::HostClaim::take(dir, host_id);
            let host = host.map_err(|why| Failure::Usage(why.to_string()))?;
            let scratch = crun::HostClaim::scratch_root(dir, root);
            Some((
                host,
                scratch.map_err(|why| Failure::Usage(why.to_string()))?,
            ))
        }
        None => None,
    };
    let Local {
        backend,
        egress,
        state,
        ..
    } = local;
    let plan = backend
        .as_mut()
        .and_then(|Backend::Container(s)| s.crun_mut());
    // A machine that supplies the egress (a test's) has the runtime use that, and the agent
    // starts none.
    if let Some(plan) = plan.filter(|_| services.machine.egress_port().is_none()) {
        let mode = egress.mode();
        let usage = |why: String| Failure::Usage(format!("--egress {mode}: {why}"));
        // Each run's record of what its wiring is goes in the state root, where the next
        // process of the agent reads it.
        let wired = state.as_ref().filter(|_| mode != crate::egress::Mode::None);
        let store = wired.map(|state| crate::egress::store::Store::open(state.path()));
        let store = store
            .transpose()
            .map_err(|why| usage(format!("the egress store: {why}")))?;
        let started =
            crun::start_egress(egress, |config| crate::egress::Egress::start(config, store));
        plan.egress = started.map_err(|why| usage(why.to_string()))?;
    }
    Ok(claims)
}

/// Puts what the agent's runtime does into `hello`, and the warnings the operator should read:
/// the agent must see what its containers do (they run where its own `/proc` and mounts show
/// them), and the hello names the runtime only after that holds. An agent that runs no job
/// says nothing. `default_gib` is the scratch quota's default, when one is enforced.
///
/// # Errors
///
/// [`Failure::Usage`]: the agent does not see the host.
pub(crate) fn report(
    hello: &mut Hello,
    local: &Local,
    start: &Start,
    services: &Services<'_>,
    default_gib: Option<u64>,
) -> Result<Option<Vec<String>>, Failure> {
    let Some(settings) = local.backend.as_ref().map(Backend::settings) else {
        return Ok(None);
    };
    let files = services.files;
    let (link, read) = (|path: &str| files.link(path), |path: &str| files.read(path));
    integrity::require_host_namespaces(&link, &read)
        .map_err(|why| Failure::Usage(format!("--container-bin: {why}")))?;
    let crun = settings.crun();
    // A crun agent given a device list gives jobs the GPUs they book, and uploads live logs.
    let passthrough = crun.is_some_and(|plan| plan.gpus.is_some());
    let mut found = contract::report(
        hello,
        start.toolkit.as_ref(),
        services.commands,
        local.files.is_some(),
        default_gib,
        (passthrough, &local.egress.ceiling(), crun.is_some()),
    );
    contract::report_host_jobs(hello, local.allow_host_jobs);
    let fuse = crate::fuse_env::probe(&|path| services.files.read(path), crate::crun::read_node);
    found.extend(contract::report_fuse(hello, local.allow_fuse_jobs, &fuse));
    let layered = crun.is_some_and(|plan| plan.layered);
    contract::report_layered(hello, layered);
    contract::report_mount_providers(hello, &local.providers, layered);
    Ok(Some(found))
}
