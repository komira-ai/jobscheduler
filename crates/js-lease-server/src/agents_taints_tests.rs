//! The taint rows of the job store put back on the host bindings at start
//! ([`restore_job_taints`]): in file mode and in the store, a binding that holds a taint
//! keeping it, a host with no binding skipped and its row kept, the rows of the other hosts
//! deleted, and a refusal when the rows cannot be read or deleted or the bindings cannot be
//! written.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use js_agent_wire::Taint;
use js_jobs::HOST_JOB_TAINT;
use js_jobs::interfere::Interfere;
use js_registry::{Binding, Credential};
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use js_testkit::dir::TestDir;
use js_types::HostId;

use super::restore_job_taints;
use crate::agent_api::job_rig::{BOOKED, placed, spec, tables};
use crate::agent_state::BindingStore;
use crate::api::rig::{Lines, process_sink};
use crate::job_store::ServerJobs;

/// The flag a refusal names.
const FLAG: &str = "--job-listen";

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// The taint a host job `job` leaves.
fn host_job(job: &str) -> Taint {
    Taint {
        reason: HOST_JOB_TAINT.to_owned(),
        run_id: Some(job.to_owned()),
        check: None,
    }
}

/// A taint some other reason left.
fn residue() -> Taint {
    Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    }
}

/// A binding to a token, with `taint`.
fn binding(taint: Option<Taint>) -> Binding {
    Binding::new(Credential::of_token("token"), None).with_taint(taint)
}

/// Where the tests keep what they write: nothing is written in file mode here.
fn files(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/job_taints");
    TestDir::under(root, name)
}

/// A store for the bindings that can fail a commit.
fn kept_store() -> Arc<Interfere<Fake>> {
    Arc::new(Interfere::new(Fake::with_flaw(Arc::default(), Flaw::None)))
}

/// Job tables where `job-a`, `job-b` and `job-c` ran on the machine itself, placed on
/// `host-a`, `host-b` and `host-c`.
fn host_jobs_placed() -> (Arc<Interfere<Fake>>, ServerJobs) {
    let (store, jobs) = tables();
    let mut on_the_machine = spec();
    on_the_machine.constraints.isolation = js_jobs::Isolation::Host;
    let placements = [
        ("job-a", "host-a"),
        ("job-b", "host-b"),
        ("job-c", "host-c"),
    ];
    for (job, host) in placements {
        placed(&jobs, job, host, on_the_machine.clone(), BOOKED);
    }
    (store, jobs)
}

/// The bindings of `host-a` (no taint) and `host-b` (`taint`).
fn bindings(taint: Option<Taint>) -> BTreeMap<HostId, Binding> {
    BTreeMap::from([
        (host("host-a"), binding(None)),
        (host("host-b"), binding(taint)),
    ])
}

/// Asserts the reconcile in file mode: a taint row whose host's binding holds none is put on
/// the binding (reason `host-job`, run id the job's id); a binding that holds a taint keeps
/// it; a row of a host with no binding is skipped, logged with its host and job, makes no
/// binding and is kept; the rows of the hosts with a binding are deleted, and the file holds
/// the bindings as merged. It catches a row that replaces the taint a binding holds, a host
/// with no binding that fails the start, loses its row or is skipped without a word, and a
/// row deleted while the file does not hold its taint.
#[test]
fn a_taint_row_the_binding_lacks_is_put_on_it_and_a_held_taint_stands() {
    let lines = Lines::default();
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let dir = files("reconcile_file");
    let (_store, jobs) = host_jobs_placed();
    let mut held = bindings(Some(residue()));
    let store = BindingStore::new(dir.path().join("bindings"), held.clone());
    restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap();
    let text = lines.take();
    let said: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("its taint is not restored"))
        .collect();
    assert_eq!(said.len(), 1, "{text}");
    for part in [
        "host=host-c",
        "job=job-c",
        "a host job tainted a host with no binding",
    ] {
        assert!(said[0].contains(part), "{text}");
    }
    assert_eq!(held.len(), 2);
    assert_eq!(held[&host("host-a")].taint(), Some(&host_job("job-a")));
    assert_eq!(held[&host("host-b")].taint(), Some(&residue()));
    let rows: Vec<HostId> = jobs.host_taints().unwrap().into_keys().collect();
    assert_eq!(rows, [host("host-c")]);
    let saved = crate::agent_state::load(&dir.path().join("bindings")).unwrap();
    assert_eq!(saved, held);
}

/// Asserts the reconcile with the store: the bindings that gain a taint are committed in one
/// transaction, and read back as the next start reads them; a second reconcile has nothing to
/// write and commits nothing. It catches a taint that is put on the binding in memory only,
/// one transaction per host, and a write repeated at every start.
#[test]
fn the_restored_taints_are_committed_to_the_store_in_one_transaction() {
    let kept = kept_store();
    let dynamic: Arc<dyn Store> = kept.clone();
    let (store, _) = BindingStore::in_store(Arc::clone(&dynamic)).unwrap();
    let (_jobs_store, jobs) = host_jobs_placed();
    let mut held = bindings(None);
    let before = kept.commit_stats().txns;
    restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap();
    assert_eq!(kept.commit_stats().txns, before + 1);

    let (_, read) = BindingStore::in_store(Arc::clone(&dynamic)).unwrap();
    assert_eq!(read.len(), 2);
    assert_eq!(read[&host("host-a")].taint(), Some(&host_job("job-a")));
    assert_eq!(read[&host("host-b")].taint(), Some(&host_job("job-b")));
    restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap();
    assert_eq!(kept.commit_stats().txns, before + 1);
}

/// Asserts that a start is refused when the taint rows cannot be read, when the bindings
/// cannot be committed (leaving the bindings as they were: the store holds none of them) and
/// when the rows cannot be deleted (the rows stay), each naming the job service's flag, and
/// that the same call succeeds once the store works. It catches a failure that is taken as no
/// rows, a start that goes on with taints it could not keep, and rows lost to a failed delete.
#[test]
fn a_taint_table_that_cannot_be_read_or_bindings_that_cannot_be_committed_refuse_the_start() {
    let kept = kept_store();
    let dynamic: Arc<dyn Store> = kept.clone();
    let (store, _) = BindingStore::in_store(Arc::clone(&dynamic)).unwrap();
    let (jobs_store, jobs) = host_jobs_placed();
    let mut held = bindings(None);

    jobs_store.fail_read_after(0);
    let unread = restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap_err();
    assert!(unread.to_string().starts_with("--job-listen: "), "{unread}");
    assert_eq!(held, bindings(None));

    kept.fail_next();
    let unwritten = restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap_err();
    let wanted = "--job-listen: cannot write the host bindings: ";
    assert!(unwritten.to_string().starts_with(wanted), "{unwritten}");
    assert_eq!(held, bindings(None));
    let (_, read) = BindingStore::in_store(Arc::clone(&dynamic)).unwrap();
    assert!(read.is_empty());

    jobs_store.fail_next();
    let undeleted = restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap_err();
    assert!(
        undeleted.to_string().starts_with("--job-listen: "),
        "{undeleted}"
    );
    assert_eq!(jobs.host_taints().unwrap().len(), 3);

    restore_job_taints(&jobs, &store, &mut held, FLAG).unwrap();
    assert_eq!(jobs.host_taints().unwrap().len(), 1);
    assert_eq!(held[&host("host-a")].taint(), Some(&host_job("job-a")));
}
