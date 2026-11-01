//! The workload: a job mix as data, expanded in submission order.

use js_measure::{Workload, WorkloadError};

fn workload(text: &str) -> Workload {
    serde_json::from_str(text).unwrap()
}

const MIX: &str = r#"{"jobs":[
  {"name":"a","count":2,"image":"img","args":["x"],"cpu_millis":4000,"memory_bytes":1000,"wall_ms":61000,"profile":"p","submit_at_ms":500,"stagger_ms":1000},
  {"name":"b","count":1,"image":"img2","cpu_millis":1000,"memory_bytes":2000,"wall_ms":1500,"class":"small","submit_at_ms":700}
]}"#;

/// Asserts the expansion: names `<kind>-<index>`, submissions staggered per kind and
/// sorted by time, the lease class defaulting to `whole-node`, the lease's wall time the
/// run's rounded up to seconds plus a minute, and every field carried to the run.
#[test]
fn the_mix_expands_in_submission_order() {
    let jobs = workload(MIX).expand().unwrap();
    let seen: Vec<(&str, u64)> = jobs
        .iter()
        .map(|job| (job.name.as_str(), job.submit_at_ms))
        .collect();
    assert_eq!(seen, vec![("a-0", 500), ("b-0", 700), ("a-1", 1500)]);
    let a = &jobs[0];
    assert_eq!(a.lease.class, "whole-node");
    assert_eq!(a.lease.max_wall_seconds, 61 + 60);
    assert_eq!((a.lease.cpu_millis, a.lease.memory_bytes), (4000, 1000));
    assert_eq!(a.run.image, "img");
    assert_eq!(a.run.args, vec!["x".to_owned()]);
    assert_eq!(a.run.profile.as_deref(), Some("p"));
    assert_eq!(a.run.wall_ms, 61_000);
    let b = &jobs[1];
    assert_eq!(b.lease.class, "small");
    assert_eq!(b.lease.max_wall_seconds, 2 + 60);
    assert_eq!(b.run.profile, None);
}

/// Asserts that a mix with no job is refused, whether it lists none or counts none, and
/// that a key the file format does not know is refused.
#[test]
fn an_empty_mix_is_refused() {
    assert_eq!(
        workload(r#"{"jobs":[]}"#).expand(),
        Err(WorkloadError::Empty)
    );
    let none = r#"{"jobs":[{"name":"a","count":0,"image":"i","cpu_millis":1,"memory_bytes":1,"wall_ms":1}]}"#;
    assert_eq!(workload(none).expand(), Err(WorkloadError::Empty));
    assert!(serde_json::from_str::<Workload>(r#"{"jobs":[],"extra":1}"#).is_err());
}
