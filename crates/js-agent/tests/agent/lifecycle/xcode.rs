//! A job start that names an Xcode build: the lifecycle asks the runtime whether it holds it
//! before anything is created.

use js_agent_wire::{Outcome, refusal};

use super::per_job::{job_start, listing, start};
use super::{Harness, names};

/// Asserts that a job start naming an Xcode build on a runtime that holds none (the default,
/// as every runtime but the Mac's native one) is answered `failed_to_start` with
/// `xcode-not-held`, creates nothing (no container, no scratch directory, no booking), and
/// that a start naming none beside it runs. It catches a job run without the Xcode it names.
#[test]
fn a_job_start_naming_an_xcode_the_runtime_lacks_is_refused_and_creates_nothing() {
    let mut h = Harness::new("job-xcode-refused");
    let before = h.life.free();
    let mut spec = start("j1");
    spec.xcode = Some("16C5032a".to_owned());
    let events = h.apply(vec![job_start(spec), job_start(start("j2"))]);
    assert_eq!(
        names(&events),
        [
            format!("refused j1 {}", refusal::XCODE_NOT_HELD),
            "started j2".to_owned()
        ]
    );
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(h.fake.created()[0].run_id, "j2");
    assert_eq!(h.fake.created()[0].xcode, None);
    assert!(h.scratch_of("j1").metadata().is_err());
    assert_eq!(h.life.free().cpu_millis, before.cpu_millis - 1000);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::XCODE_NOT_HELD));
}
