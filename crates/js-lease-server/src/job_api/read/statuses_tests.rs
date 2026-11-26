//! The read of the callback statuses the read calls show beside the jobs' records.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{CallbackKey, CallbackState, JobCallback, JobId, JobSpec, NewJob, Retention, keys};
use js_state::{Cond, ManualClock, Store, Txn};
use js_types::SigningKey;

use super::{asking, read};
use crate::agent_api::job_rig::{START_MS, spec, tables_on};

/// Asserts that only the jobs that asked for a callback are read; that each has its status;
/// that a job whose rows are gone (swept since its record was read), and one whose callback row
/// alone is gone, have none and fail nothing; and that a store that fails fails the read.
/// Catches a status read for every job of a page, a swept job that fails a whole listing, and
/// a failure answered as no status.
#[test]
fn each_asking_job_has_its_status_and_a_gone_one_none() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (store, jobs) = tables_on(clock, Retention::DEFAULT);
    let record = |id: &str, callback: bool| {
        let callback = callback.then(|| JobCallback {
            url: "https://hooks.example/x".to_owned(),
            key: CallbackKey::Key(SigningKey::new(vec![3; 32]).expect("a key")),
            deadline_seconds: None,
        });
        let new = NewJob {
            id: Some(JobId::parse(id).expect("an id")),
            spec: JobSpec { callback, ..spec() },
            priority: 0,
            client: None,
        };
        jobs.insert_queued(new).expect("records").record
    };
    let plain = record("st-plain", false);
    let asked = record("st-asked", true);
    let rowless = record("st-rowless", true);
    let swept = JobId::parse("st-swept").expect("an id");
    let ids = asking([&plain, &asked, &rowless]);
    assert_eq!(ids, vec![asked.id.clone(), rowless.id.clone()]);
    let txn = Txn::new().delete(keys::callback(&rowless.id), Cond::Any);
    store.txn(txn).expect("reaches").expect("deletes");
    let mut all = ids.clone();
    all.push(swept);
    let statuses = read(&jobs, &all).expect("reads");
    let states: Vec<(JobId, CallbackState)> = statuses
        .into_iter()
        .map(|(id, status)| (id, status.state))
        .collect();
    assert_eq!(states, vec![(asked.id.clone(), CallbackState::Armed)]);
    store.fail_read_after(0);
    assert!(read(&jobs, &ids).is_err());
}
