//! A process that dies after any step of making a tree leaves something the next start
//! reconciles.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{Rig, sized, wanted};
use js_treestore::{Config, Error, Step};

const ALL: [Step; 7] = [
    Step::Pinned,
    Step::Staged,
    Step::Unpacked,
    Step::Sealed,
    Step::Renamed,
    Step::Recorded,
    Step::Registered,
];

fn dying_after(step: Step) -> Config {
    Config {
        crash: Some(Arc::new(move |at| at == step)),
        ..Config::default()
    }
}

#[test]
fn every_step_dies_without_cleaning_up_and_the_next_start_reconciles() {
    for step in ALL {
        let rig = Rig::new();
        let l = sized("f", 10, 1);
        rig.have(&l);
        let store = rig.open(dying_after(step));
        let error = store.ensure(&wanted(&l)).unwrap_err();
        assert!(
            matches!(error, Error::Crashed(at) if at == step),
            "{step:?}"
        );
        // The dead process released nothing.
        assert_eq!(rig.fake.holders_pinning().len(), 1, "{step:?}");
        let staged = (Step::Staged..=Step::Sealed).contains(&step);
        assert_eq!(rig.names("stage").len(), usize::from(staged), "{step:?}");
        assert_eq!(store.stats().in_flight, 0);
        drop(store);

        // Restart: nothing is live, so the blob pin is reaped; accounted bytes are re-made.
        rig.fake.restart(&BTreeSet::new());
        let store = rig.open(Config::default());
        assert!(rig.fake.holders_pinning().is_empty(), "{step:?}");
        assert!(rig.names("stage").is_empty(), "{step:?}");
        assert!(rig.names("trash").is_empty(), "{step:?}");
        let complete = step >= Step::Recorded;
        assert_eq!(
            rig.fake.accounted(),
            if complete { 10 } else { 0 },
            "{step:?}"
        );
        let sealed_before = rig.sealer.seal_count();
        store.ensure(&wanted(&l)).unwrap();
        let again = usize::from(!complete);
        assert_eq!(rig.sealer.seal_count(), sealed_before + again, "{step:?}");
        assert_eq!(rig.fake.accounted(), 10, "{step:?}");
        assert_eq!(rig.fake.entries(), 1, "{step:?}");
    }
}
