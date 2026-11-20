//! What a change of the loaded kernel modules says: the text of the `kernel-module-changed`
//! note.
//!
//! A kernel module appearing or going is not a taint (see [`super`]): a contained job cannot
//! load or unload one (`init_module`, `finit_module` and `delete_module` fail for it), the
//! kernel loads one on a first use for any process (the job runtime's `overlay`, `veth` and
//! `nft_nat`, a host tool's `inet_diag`), and the host's operator unloads them. So the
//! difference is logged once, as a note naming each module, and the baseline takes it.

use std::collections::BTreeSet;

/// What the modules `now` differ in from the modules `before`, as the detail of the
/// `kernel-module-changed` note: each module loaded (`loaded <name>`), then each one unloaded
/// (`unloaded <name>`), each group in the order of the names.
pub(super) fn changed_modules(before: &BTreeSet<String>, now: &BTreeSet<String>) -> Vec<String> {
    now.difference(before)
        .map(|name| format!("loaded {name}"))
        .chain(
            before
                .difference(now)
                .map(|name| format!("unloaded {name}")),
        )
        .collect()
}
