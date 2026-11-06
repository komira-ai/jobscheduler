//! The whole agent with the VM backend: which flags turn it on, what they require, and what
//! the hello says with it on and off.

use std::fs;
use std::sync::Arc;

use js_vmm::lease_fake::FakeDriver;
use serde_json::Value;

use super::super::native::agent::{native_flags, private};
use super::super::native::fake_helper::FakeHelper;
use crate::jobs::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Host, Jobs, Lowest, Ran, Scratch, Script, ScriptConnector, code, flags,
    linux_host, mac_host, run_on, welcome,
};

/// Runs the whole agent on `host` over `machine` with `extra` flags until the server's
/// first answer, a welcome; returns what it printed and the hello it sent (`null` for none).
fn hello_with(host: &Host, extra: &[String], machine: FakeMachine) -> (Ran, Value) {
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let args = flags(&refs);
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (Arc::new(machine), None);
    let ran = run_on(&args, host, &connector, &mut sleeper, &mut Lowest, jobs);
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// The VM flags over a driver binary and a store with a base image, both in `dir`.
fn vm_flags(dir: &Scratch) -> Vec<String> {
    let driver = dir.write("js-vmm", "#!/bin/sh\n", 0o755);
    let store = private(dir, "vm-store");
    fs::create_dir(dir.path("vm-store").join("base")).unwrap();
    vec![
        "--vm-driver".to_owned(),
        driver.display().to_string(),
        "--vm-store".to_owned(),
        store,
    ]
}

/// Asserts that a Mac agent with the VM flags reports `vm.slots = 2` and, as the most one
/// VM may be given, the CPUs and the GiB of memory its hello offers, beside the native
/// capabilities, and that one without them reports no `vm.*` key. Catches a host that
/// a scheduler never sends a VM lease to (no `vm.slots`), one that says it runs VMs when
/// it runs none, and a count other than two.
#[test]
fn the_hello_reports_two_vm_slots_with_the_flags_and_none_without() {
    let dir = Scratch::new("vm-agent-on");
    let fake = FakeHelper::start(&dir);
    let mut extra = native_flags(&dir, &fake);
    extra.extend(vm_flags(&dir));
    let driver = Arc::new(FakeDriver::new());
    let (ran, hello) = hello_with(&mac_host(), &extra, FakeMachine::new().with_vms(&driver));
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["vm.slots"], "2");
    let cpus = hello["shape"]["cpu_millis"].as_u64().unwrap() / 1000;
    let mem_gib = hello["shape"]["memory_bytes"].as_u64().unwrap() >> 30;
    assert_eq!(caps["vm.max_cpus"], cpus.to_string());
    assert_eq!(caps["vm.max_mem_gib"], mem_gib.to_string());
    assert_eq!(caps["runtime"], "sandbox-exec");
    assert_eq!(driver.calls(), [js_vmm::lease_fake::Call::Leftovers]);

    let off = Scratch::new("vm-agent-off");
    let fake = FakeHelper::start(&off);
    let extra = native_flags(&off, &fake);
    let (ran, hello) = hello_with(&mac_host(), &extra, FakeMachine::new());
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let caps = hello["capabilities"].as_object().unwrap();
    assert_eq!(caps["runtime"], "sandbox-exec");
    for key in ["vm.slots", "vm.max_cpus", "vm.max_mem_gib"] {
        assert!(!caps.contains_key(key), "{key}: {caps:?}");
    }
}

/// Asserts that each wrong use of the VM flags exits 2 before anything is sent, with the
/// exact reason: one without the other, a Linux host, no native jobs, a relative driver, a
/// store without a base image, and a `--capability` that names a VM key. Catches a backend
/// turned on half-configured, and an operator who forges the VM capabilities.
#[test]
fn a_wrong_vm_flag_exits_2_naming_it() {
    let dir = Scratch::new("vm-agent-flags");
    let fake = FakeHelper::start(&dir);
    let native = native_flags(&dir, &fake);
    let vm = vm_flags(&dir);
    let (driver, store) = (vm[1].clone(), vm[3].clone());
    let with = |more: &[&str]| -> Vec<String> {
        let mut all = native.clone();
        all.extend(more.iter().map(|flag| (*flag).to_owned()));
        all
    };
    let no_base = private(&dir, "empty-store");
    let cases: Vec<(Host, Vec<String>, String)> = vec![
        (
            mac_host(),
            with(&["--vm-store", &store]),
            "--vm-store: needs --vm-driver".to_owned(),
        ),
        (
            mac_host(),
            with(&["--vm-driver", &driver]),
            "--vm-driver: needs --vm-store".to_owned(),
        ),
        (
            mac_host(),
            vec![
                "--vm-driver".to_owned(),
                driver.clone(),
                "--vm-store".to_owned(),
                store.clone(),
            ],
            "--vm-driver: needs --native-helper-socket (the VMs run beside the native jobs)"
                .to_owned(),
        ),
        (
            mac_host(),
            with(&["--vm-driver", "js-vmm", "--vm-store", &store]),
            "--vm-driver: must be an absolute path to the js-vmm binary".to_owned(),
        ),
        (
            mac_host(),
            with(&["--vm-driver", "/nonexistent/js-vmm", "--vm-store", &store]),
            "--vm-driver: must be an absolute path to the js-vmm binary".to_owned(),
        ),
        (
            mac_host(),
            with(&["--vm-driver", &driver, "--vm-store", "vm-store"]),
            "--vm-store: must be an absolute path to a directory holding the base image `base`"
                .to_owned(),
        ),
        (
            mac_host(),
            with(&["--vm-driver", &driver, "--vm-store", &no_base]),
            "--vm-store: must be an absolute path to a directory holding the base image `base`"
                .to_owned(),
        ),
        (
            mac_host(),
            with(&["--capability", "vm.slots=9"]),
            "--capability: vm.slots is detected and cannot be set".to_owned(),
        ),
    ];
    for (host, extra, said) in cases {
        let (ran, hello) = hello_with(&host, &extra, FakeMachine::new());
        assert_eq!(ran.status(), code(2), "{said}: {}", ran.err);
        assert!(ran.err.contains(&said), "{said}: {}", ran.err);
        assert_eq!(hello, Value::Null, "{said}: nothing was sent");
    }
    // A Linux agent is refused for the platform before the native flags are judged.
    let linux = vec![
        "--vm-driver".to_owned(),
        driver,
        "--vm-store".to_owned(),
        store,
    ];
    let (ran, _) = hello_with(&linux_host(), &linux, FakeMachine::new());
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("--vm-driver: VM leases run on macOS only"),
        "{}",
        ran.err
    );
}

/// Asserts that an agent whose machine runs no VMs (the default driver, off macOS) does not
/// start with the backend on, saying so, and sends no hello. Catches an agent that reports
/// `vm.slots` while it cannot make one VM. (On a Mac the default driver is the real one.)
#[cfg(not(target_os = "macos"))]
#[test]
fn a_machine_that_runs_no_vms_keeps_the_agent_from_starting() {
    let dir = Scratch::new("vm-agent-nodriver");
    let fake = FakeHelper::start(&dir);
    let mut extra = native_flags(&dir, &fake);
    extra.extend(vm_flags(&dir));
    let (ran, hello) = hello_with(&mac_host(), &extra, FakeMachine::new());
    assert_ne!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err.contains("VM leases run on macOS only"),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);
}

/// Asserts that the machine itself (not a test's) runs no VM off macOS: its VM driver is
/// refused with the reason. Catches a Linux agent that would report `vm.slots` over a driver
/// that cannot boot anything.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_system_machine_runs_no_vms_off_macos() {
    use clap::Parser as _;
    use js_agent::vm::VmSettings;
    use js_agent::{Cli, Machine, Platform, System};

    let dir = Scratch::new("vm-agent-system");
    let vm = vm_flags(&dir);
    let mut args = vec!["js-agent".to_owned()];
    args.extend(flags(&[]).iter().map(|flag| (*flag).to_owned()));
    args.extend(vm);
    let cli = Cli::try_parse_from(&args).unwrap();
    let mac = Platform {
        os: "macos".to_owned(),
        arch: "arm64".to_owned(),
    };
    let settings = VmSettings::from_cli(&cli, &mac, true).unwrap().unwrap();
    let refused = System::new().vm_driver(&settings).map(|_| ());
    assert_eq!(refused, Err("VM leases run on macOS only".to_owned()));
}
