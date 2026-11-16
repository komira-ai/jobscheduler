//! The VM backend's settings and the capabilities it reports, checked apart from the agent that
//! takes them: the flags (`--vm-driver`, `--vm-store`) exist in the macOS build only, so these
//! run on every build.

use std::fs;

use js_agent::vm::{
    MAX_CPUS_CAPABILITY, MAX_MEM_GIB_CAPABILITY, SLOTS_CAPABILITY, VM_SLOTS, VmSettings, report,
};

use crate::support::{Scratch, hello};

/// Asserts what the VM settings refuse, naming the flag: one of the two paths without the
/// other, a driver that is not an absolute path to a file, and a store that is not an absolute
/// path to a directory holding the base image `base`; that neither path turns the backend off;
/// and that both, right, turn it on. Catches a backend turned on half-configured, which would
/// fail at the first VM start instead of at the agent's.
#[test]
fn the_vm_settings_refuse_a_half_or_wrong_configuration() {
    let dir = Scratch::new("vm-settings");
    let driver = dir.write("js-vmm", "#!/bin/sh\n", 0o755);
    let store = dir.path("vm-store");
    fs::create_dir_all(&store).unwrap();
    let refused = |driver: Option<&std::path::Path>, store: Option<&std::path::Path>| {
        VmSettings::new(driver, store).unwrap_err()
    };
    assert!(VmSettings::new(None, None).unwrap().is_none());
    assert_eq!(refused(None, Some(&store)), "--vm-store: needs --vm-driver");
    assert_eq!(
        refused(Some(&driver), None),
        "--vm-driver: needs --vm-store"
    );
    let driver_text = "--vm-driver: must be an absolute path to the js-vmm binary";
    assert_eq!(
        refused(Some(std::path::Path::new("js-vmm")), Some(&store)),
        driver_text
    );
    assert_eq!(refused(Some(&store), Some(&store)), driver_text);
    let store_text =
        "--vm-store: must be an absolute path to a directory holding the base image `base`";
    assert_eq!(refused(Some(&driver), Some(&store)), store_text);
    assert_eq!(
        refused(Some(&driver), Some(std::path::Path::new("vm-store"))),
        store_text
    );
    fs::create_dir(store.join("base")).unwrap();
    assert!(
        VmSettings::new(Some(&driver), Some(&store))
            .unwrap()
            .is_some()
    );
}

/// Asserts the capabilities the VM backend puts into the hello: `vm.slots` is the number of
/// VMs at once, and `vm.max_cpus` and `vm.max_mem_gib` are the whole CPUs and GiB the host
/// offers its jobs; the hello still validates. Catches a server told the backend can give one
/// VM more than the host offers in all.
#[test]
fn the_vm_capabilities_are_the_slots_and_the_hosts_offer() {
    let mut hello = hello("mac-1");
    hello.shape.cpu_millis = 10_500;
    hello.shape.memory_bytes = (16 << 30) + 1;
    report(&mut hello);
    let got = |key: &str| hello.capabilities.get(key).cloned();
    assert_eq!(got(SLOTS_CAPABILITY), Some(VM_SLOTS.to_string()));
    assert_eq!(got(MAX_CPUS_CAPABILITY).as_deref(), Some("10"));
    assert_eq!(got(MAX_MEM_GIB_CAPABILITY).as_deref(), Some("16"));
    assert_eq!(hello.validate(), Ok(()));
}

/// Asserts that the machine's own VM driver is refused off macOS, naming why, over settings
/// that pass their checks: the VM backend runs on Apple's virtualization alone. Catches a Linux
/// agent that would start the VM helper of a Mac.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_system_machine_runs_no_vms_off_macos() {
    use js_agent::{Machine as _, System};

    let dir = Scratch::new("vm-system");
    let driver = dir.write("js-vmm", "#!/bin/sh\n", 0o755);
    let store = dir.path("vm-store");
    fs::create_dir_all(store.join("base")).unwrap();
    let settings = VmSettings::new(Some(&driver), Some(&store))
        .unwrap()
        .unwrap();
    let refused = System::new().vm_driver(&settings).err();
    assert_eq!(refused.as_deref(), Some("VM leases run on macOS only"));
}
