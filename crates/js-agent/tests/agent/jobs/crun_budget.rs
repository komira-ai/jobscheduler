//! `--image-budget-gib` reaches the crun runtime the agent opens.

use super::crun_e2e::{NODE_IN_USE, Rig};
use crate::support::code;

/// Asserts that `--image-budget-gib` reaches the crun runtime: an agent started with
/// `--image-budget-gib 3` opens its runtime from crun settings whose image budget is
/// 3 GiB in bytes, and one started without the flag from settings with no budget (nothing is
/// evicted). It catches a flag that is checked and never handed to the runtime, and a budget
/// handed over in GiB rather than bytes.
#[test]
fn the_image_budget_reaches_the_crun_runtime() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    for (extra, want) in [
        (&["--image-budget-gib", "3"][..], Some(3 << 30)),
        (&[][..], None),
    ] {
        let rig = Rig::new("jobs_crun_image_budget", extra);
        let budgets = rig.machine.image_budgets.lock().expect("budgets").clone();
        assert_eq!(budgets, [want], "{extra:?}");
        let ran = rig.agent.stop();
        assert_eq!(ran.status(), code(0), "{}", ran.err);
    }
}
