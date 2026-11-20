//! A kernel module appearing or going is the host's own doing: the check passes and the log
//! names it in one note, for the modules the job runtime loads on its first use, for one a
//! host tool loads, and for an unload; the baseline takes the change so it is noted once.

use js_agent::integrity::{FakeHost, code};
use js_agent::lifecycle::CheckNote;

use super::{noted_after, quiet, started};

/// Asserts that a first job on a host whose runtime modules were not loaded at the baseline
/// (the file system overlay, and the veth pair and the NAT of a wired job's network) passes
/// with one `kernel-module-changed` note naming all three, that the clean check takes them
/// into the baseline so the next check notes nothing, and that a later unload of one is one
/// note too. It catches a taint for the first job only because the kernel loads the modules
/// the job runtime needs on their first use, a note per module, and a baseline that never
/// holds them.
#[test]
fn a_first_job_loading_the_runtime_modules_is_one_note() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    for module in ["overlay", "veth", "nft_nat"] {
        host.load_module(module);
    }
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::MODULE_CHANGED,
            "loaded nft_nat, loaded overlay, loaded veth"
        )]
    );
    assert!(quiet(&mut checks, "r2"), "nothing changed since");

    host.unload_module("veth");
    assert_eq!(
        noted_after(&mut checks, "r3"),
        [CheckNote::new(code::MODULE_CHANGED, "unloaded veth")],
        "the check of r1 took veth into the baseline"
    );
}

/// Asserts that a module a host tool loads during a job (`inet_diag`, which `ss` loads) is
/// named in the note, beside the runtime's, and that the check passes. It catches a note that
/// leaves a module out, and a taint for a module a job did not load.
#[test]
fn a_module_a_tool_loads_is_named_in_the_note() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    for module in ["overlay", "inet_diag"] {
        host.load_module(module);
    }
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::MODULE_CHANGED,
            "loaded inet_diag, loaded overlay"
        )]
    );
}

/// Asserts that a module loaded and one unloaded during one job are one note naming both,
/// the loaded first (`loaded evil, unloaded ext4`), that the check passes, and that the
/// baseline takes both, so the next check notes nothing. It catches a comparison that sees
/// only what was loaded, a note that names one of the two, and a change noted again after
/// every later check.
#[test]
fn a_module_loaded_and_one_unloaded_are_one_note_naming_both() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.unload_module("ext4");
    host.load_module("evil");
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::MODULE_CHANGED,
            "loaded evil, unloaded ext4"
        )]
    );
    assert!(quiet(&mut checks, "r2"), "the baseline took both");
}

/// Asserts that unloading a module the baseline holds (the runtime's `overlay`, loaded
/// before the agent started) is a note naming it as unloaded, and that the check passes. It
/// catches a taint for an unload, which no contained job can do and an operator does.
#[test]
fn unloading_a_module_the_baseline_holds_is_a_note() {
    let host = FakeHost::quiet();
    host.load_module("overlay");
    let mut checks = started(&host, vec![]);
    host.unload_module("overlay");
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(code::MODULE_CHANGED, "unloaded overlay")]
    );
}
