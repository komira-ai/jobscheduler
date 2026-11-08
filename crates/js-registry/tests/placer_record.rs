//! What [`js_registry::Registry::request_record`] answers while a placer that does not keep
//! the wait list is set: a request told apart by its id reads the record the placer
//! gives, and another id reads none.

mod support;

use std::sync::Arc;

use js_provider::{Candidate, StartSpec};
use js_registry::{MarkedTails, Placer, RequestKey, RequestRecord};
use js_types::RequestId;
use support::world::*;

/// A placer that books the first host it is shown and keeps a record for one request id.
struct Records(RequestId);

impl Placer for Records {
    fn pick(&self, _: &StartSpec, open: &[Candidate], _: &MarkedTails) -> Option<usize> {
        (!open.is_empty()).then_some(0)
    }

    fn whole_record(&self, key: &RequestKey) -> Option<RequestRecord> {
        match key {
            RequestKey::Id(id) if *id == self.0 => Some(RequestRecord {
                prepare_failures: 3,
                ..RequestRecord::default()
            }),
            _ => None,
        }
    }
}

/// Asserts that the record of a request id the placer keeps is the placer's, with its
/// prepare failures, and that another id the placer keeps nothing for reads none.
#[test]
fn a_request_id_reads_the_record_of_a_placer_that_keeps_it() {
    let w = World::new(&["a"], |settings| settings);
    let kept = RequestId::new("kept").unwrap();
    let other = RequestId::new("other").unwrap();
    w.fx.registry.set_placer(Arc::new(Records(kept.clone())));
    let record = w.fx.registry.request_record(&kept).unwrap();
    assert_eq!(record.prepare_failures, 3);
    assert_eq!(w.fx.registry.request_record(&other), None);
}
