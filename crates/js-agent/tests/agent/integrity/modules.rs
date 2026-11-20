//! The kernel modules the agent's own job runtime loads: a first job on a host that has not
//! loaded them leaves no finding, any other module loaded while a job runs is named alone,
//! and what the runtime loads follows the agent's configuration.

use std::collections::BTreeSet;

use js_agent::integrity::{FakeHost, Integrity, NFT_NAT, OVERLAY, VETH, code, runtime_modules};

use super::{after, said, started};

/// The checks over `host` for an agent that wires jobs when `wired`, as the agent makes them.
fn checks_of(host: &FakeHost, wired: bool) -> Integrity {
    started(host, vec![]).with_runtime_modules(runtime_modules(wired))
}

/// The names as the set the checks take.
fn names(of: &[&str]) -> BTreeSet<String> {
    of.iter().map(|name| (*name).to_owned()).collect()
}

/// Asserts that the runtime's modules are `overlay` for an agent that wires no job's network,
/// and `overlay`, `veth` and `nft_nat` for one that does, and that the constants name them.
/// It catches a set that does not follow the configuration, which would taint a host that
/// does not wire jobs for a `veth` it never loads, or one that does for the first job's
/// network.
#[test]
fn the_runtime_modules_follow_the_configuration() {
    assert_eq!(runtime_modules(false), names(&["overlay"]));
    assert_eq!(
        runtime_modules(true),
        names(&["nft_nat", "overlay", "veth"])
    );
    assert_eq!(
        [OVERLAY, VETH, NFT_NAT],
        ["overlay", "veth", "nft_nat"],
        "the names of the constants"
    );
}

/// Asserts that a first job on a host whose runtime modules were not loaded at the baseline
/// (the file system overlay, and the veth pair and the NAT of a wired job's network) leaves
/// no finding, that the clean check takes them into the baseline, and that a later unload of
/// one is a finding. It catches a host tainted by the first job only because the kernel loads
/// the modules the job runtime needs on their first use, and a baseline that never holds them.
#[test]
fn a_first_job_loading_the_runtime_modules_leaves_no_finding() {
    let host = FakeHost::quiet();
    let mut checks = checks_of(&host, true);
    for module in [OVERLAY, VETH, NFT_NAT] {
        host.load_module(module);
    }
    assert!(after(&mut checks, "r1").is_ok());
    assert!(after(&mut checks, "r2").is_ok(), "nothing changed since");

    host.unload_module(VETH);
    let found = after(&mut checks, "r3").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::MODULE_CHANGED,
            "kernel-module-changed: unloaded veth".to_owned()
        ),
        "the clean check took veth into the baseline"
    );
}

/// Asserts that a module a third party loads during a job (`inet_diag`, which `ss` loads) is a
/// `kernel-module-changed` finding whose detail names that module alone, also when the
/// runtime's modules were loaded in the same job. It catches a list of the runtime's modules
/// that lets other modules pass, and a detail that names the runtime's modules beside it.
#[test]
fn a_module_of_a_third_party_is_named_alone() {
    let host = FakeHost::quiet();
    let mut checks = checks_of(&host, true);
    for module in [OVERLAY, VETH, NFT_NAT, "inet_diag"] {
        host.load_module(module);
    }
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::MODULE_CHANGED,
            "kernel-module-changed: loaded inet_diag".to_owned()
        )
    );
}

/// Asserts that on an agent that wires no job's network `veth` appearing is a finding, while
/// `overlay` appearing is not. It catches a set of the runtime's modules that is the same for
/// every configuration, which would let a job that loads `veth` on a host that never wires
/// jobs go unseen.
#[test]
fn veth_is_a_finding_on_an_agent_that_wires_no_jobs() {
    let host = FakeHost::quiet();
    let mut checks = checks_of(&host, false);
    host.load_module(OVERLAY);
    assert!(after(&mut checks, "r1").is_ok());

    host.load_module(VETH);
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::MODULE_CHANGED,
            "kernel-module-changed: loaded veth".to_owned()
        )
    );
}

/// Asserts that unloading a module of the runtime's list that the baseline holds is a finding,
/// by name. It catches a list that exempts a module from the unload check as well as from the
/// load check.
#[test]
fn unloading_a_runtime_module_is_a_finding() {
    let host = FakeHost::quiet();
    host.load_module(OVERLAY);
    let mut checks = checks_of(&host, false);
    host.unload_module(OVERLAY);
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::MODULE_CHANGED,
            "kernel-module-changed: unloaded overlay".to_owned()
        )
    );
}
