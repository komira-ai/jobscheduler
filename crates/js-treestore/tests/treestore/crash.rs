//! A process that dies after any step of making a tree leaves something the next start
//! reconciles.

use std::sync::Arc;

use crate::common::{Rig, sized, wanted};
use js_treestore::{Config, Error, Step};

const ALL: [Step; 6] = [
    Step::Fetched,
    Step::Staged,
    Step::Unpacked,
    Step::Sealed,
    Step::Renamed,
    Step::Recorded,
];

#[test]
fn every_step_dies_without_cleaning_up_and_the_next_start_reconciles() {
    for step in ALL {
        let rig = Rig::new();
        let l = sized("f", 10, 1);
        rig.have(&l);
        let cfg = Config {
            crash: Some(Arc::new(move |at| at == step)),
            ..rig.cfg()
        };
        let store = rig.open(cfg);
        let error = store.ensure(&wanted(&l)).unwrap_err();
        assert!(
            matches!(error, Error::Crashed(at) if at == step),
            "{step:?}"
        );
        // The dead process cleaned nothing up.
        assert_eq!(rig.names("tmp").len(), 1, "{step:?}");
        let staged = (Step::Staged..=Step::Sealed).contains(&step);
        assert_eq!(rig.names("stage").len(), usize::from(staged), "{step:?}");
        assert_eq!(store.stats().in_flight, 0);
        assert_eq!(store.stats().trees, 0, "{step:?}");
        drop(store);

        // The next start finds a tree only if the record was written.
        let store = rig.open(rig.cfg());
        assert!(rig.names("tmp").is_empty(), "{step:?}");
        assert!(rig.names("stage").is_empty(), "{step:?}");
        assert!(rig.names("trash").is_empty(), "{step:?}");
        let complete = step >= Step::Recorded;
        assert_eq!(store.stats().trees, usize::from(complete), "{step:?}");
        assert_eq!(rig.present(&l.diff_id), complete, "{step:?}");
        let fetched = rig.source.fetches();
        store.ensure(&wanted(&l)).unwrap();
        assert_eq!(
            rig.source.fetches(),
            fetched + usize::from(!complete),
            "{step:?}"
        );
        assert_eq!(store.stats().used, 10, "{step:?}");
    }
}
