//! What the agent reads from the host, through recorded program output.

use js_agent::{
    APPLE_GPU_CLASS, CommandRunner, FileReader, Platform, SystemCommands, SystemFiles, detect,
    host_id, machine_id,
};

use crate::support::{
    AMD_MAC, CPU_CMD, IOREG_CMD, MACHINE_ID, MEMSIZE_CMD, NVIDIA_CMD, NVIDIA_MIXED, PLATFORM_UUID,
    PROFILER_CMD, linux_host, mac_host, platform, run_scripted,
};

/// Asserts that the recorded `nvidia-smi` output of a two-A100 host gives two GPUs, a
/// class derived from the model, one device each with memory in bytes, and the CPU and
/// memory figures of the other probes. It catches a parser that reads MiB as bytes, one
/// that loses a device, and a class that is not a valid class text.
#[test]
fn nvidia_csv_gives_devices_and_a_class() {
    let host = linux_host();
    let found = detect(&host.platform, &host.commands, &host.files).unwrap();
    assert_eq!(found.cpu_millis, 8000);
    assert_eq!(found.memory_bytes, 32 * 1024 * 1024 * 1024);
    assert_eq!(found.gpus.count, 2);
    assert_eq!(found.gpus.class.as_deref(), Some("nvidia-a100-sxm4-80gb"));
    assert_eq!(found.gpus.devices.len(), 2);
    assert_eq!(found.gpus.devices[1].index, 1);
    assert_eq!(found.gpus.devices[1].memory_bytes, 81920 * 1024 * 1024);
    assert!(found.warnings.is_empty());
}

/// Asserts that `nvidia-smi` printing nothing (a driver and no device) offers no GPU and
/// no warning: nothing failed.
#[test]
fn no_nvidia_output_is_no_gpu_without_a_warning() {
    let mut host = linux_host();
    host.commands = host.commands.with(NVIDIA_CMD, Ok("\n"));
    let found = detect(&host.platform, &host.commands, &host.files).unwrap();
    assert_eq!((found.gpus.count, found.gpus.class), (0, None));
    assert!(found.warnings.is_empty());
}

/// Asserts that a GPU probe that fails (the program is absent, exits non-zero, or prints
/// what the parser cannot read) reports `gpus` 0 with no class and no device, and one
/// warning that says the probe failed. It catches a probe failure that aborts the agent,
/// one that keeps a half-read GPU, and one that fails silently.
#[test]
fn gpu_probe_failure_reports_zero_and_warns() {
    let bad_outputs = [
        ("absent program", Err("nvidia-smi: not found")),
        ("wrong field count", Ok("0, GPU-1, NVIDIA A100\n")),
        (
            "index is no number",
            Ok("x, GPU-1, NVIDIA A100-SXM4-80GB, 81920\n"),
        ),
        (
            "memory is no number",
            Ok("0, GPU-1, NVIDIA A100-SXM4-80GB, lots\n"),
        ),
        ("two models", Ok(NVIDIA_MIXED)),
        ("model with no letter", Ok("0, GPU-1, ---, 81920\n")),
    ];
    for (why, output) in bad_outputs {
        let mut host = linux_host();
        host.commands = host.commands.with(NVIDIA_CMD, output);
        let found = detect(&host.platform, &host.commands, &host.files).unwrap();
        assert_eq!(found.gpus, js_agent::Gpus::none(), "{why}");
        assert_eq!(found.warnings.len(), 1, "{why}");
        assert!(found.warnings[0].starts_with("GPU probe failed ("), "{why}");
    }
    let bad_profiles = [
        ("absent program", Err("system_profiler: not found")),
        ("not JSON", Ok("Graphics/Displays:\n")),
        ("no adapter list", Ok("{\"SPHardwareDataType\": []}")),
    ];
    for (why, output) in bad_profiles {
        let mut host = mac_host();
        host.commands = host.commands.with(PROFILER_CMD, output);
        let found = detect(&host.platform, &host.commands, &host.files).unwrap();
        assert_eq!(found.gpus, js_agent::Gpus::none(), "{why}");
        assert_eq!(found.warnings.len(), 1, "{why}");
    }
}

/// The same failure seen from the program: the hello it sends says `gpus` 0 and stderr
/// carries the warning.
#[test]
fn a_failed_probe_shows_in_the_hello_and_on_stderr() {
    let mut host = linux_host();
    host.commands = host.commands.with(NVIDIA_CMD, Err("nvidia-smi: not found"));
    let replies = vec![crate::support::welcome(1)];
    let (ran, sent, _) = run_scripted(&["--host-id", "gpu-less"], &host, replies, 1);
    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    assert!(ran.err.contains("js-agent: warning: GPU probe failed"));
    let hello = &sent.borrow()[0].1;
    assert_eq!(hello["shape"]["gpus"], 0);
    assert_eq!(hello["gpu_devices"], serde_json::json!([]));
    assert!(hello["shape"].get("gpu_class").is_none());
}

/// Asserts that an Apple silicon Mac reports one GPU of class `apple-m`, with no device
/// list (the probe reads no identity), and that nvidia-smi is never started on a Mac. It
/// catches a Mac host offered as GPU-less, and the wrong probe running.
#[test]
fn an_apple_gpu_is_one_gpu_of_class_apple_m() {
    let host = mac_host();
    let found = detect(&host.platform, &host.commands, &host.files).unwrap();
    assert_eq!(found.cpu_millis, 10_000);
    assert_eq!(found.memory_bytes, 16 * 1024 * 1024 * 1024);
    assert_eq!(found.gpus.count, 1);
    assert_eq!(found.gpus.class.as_deref(), Some(APPLE_GPU_CLASS));
    assert!(found.gpus.devices.is_empty());
    assert!(found.warnings.is_empty());
    let ran = host.commands.ran.borrow();
    assert!(!ran.iter().any(|line| line.starts_with("nvidia-smi")));
}

/// Asserts that a Mac whose only GPU is another vendor's offers none, without a warning:
/// the probe worked, the Mac has no Apple GPU.
#[test]
fn a_mac_with_an_amd_gpu_offers_none() {
    let mut host = mac_host();
    host.commands = host.commands.with(PROFILER_CMD, Ok(AMD_MAC));
    let found = detect(&host.platform, &host.commands, &host.files).unwrap();
    assert_eq!((found.gpus.count, found.gpus.class), (0, None));
    assert!(found.warnings.is_empty());
}

/// Asserts that the figures the agent cannot guess fail the start naming what failed:
/// the CPU count, and memory on both platforms.
#[test]
fn missing_cpu_or_memory_facts_are_errors() {
    let cases = [
        (
            linux_host()
                .commands
                .with(CPU_CMD, Err("getconf: not found")),
            linux_host(),
            "cannot count the CPUs: getconf: not found",
        ),
        (
            linux_host().commands.with(CPU_CMD, Ok("many\n")),
            linux_host(),
            "cannot count the CPUs: getconf did not print a number",
        ),
        (
            linux_host().commands,
            {
                let mut host = linux_host();
                host.files = Default::default();
                host
            },
            "cannot size memory: /proc/meminfo: No such file or directory",
        ),
        (
            linux_host().commands,
            {
                let mut host = linux_host();
                host.files = host.files.with("/proc/meminfo", "MemFree: 1 kB\n");
                host
            },
            "cannot size memory: /proc/meminfo has no MemTotal",
        ),
        (
            linux_host().commands,
            {
                let mut host = linux_host();
                host.files = host.files.with("/proc/meminfo", "MemTotal: lots kB\n");
                host
            },
            "cannot size memory: MemTotal did not print a number",
        ),
        (
            mac_host()
                .commands
                .with(MEMSIZE_CMD, Err("sysctl: not found")),
            mac_host(),
            "cannot size memory: sysctl: not found",
        ),
    ];
    for (commands, mut host, expected) in cases {
        host.commands = commands;
        let said = detect(&host.platform, &host.commands, &host.files).unwrap_err();
        assert_eq!(said, expected);
    }
}

/// Asserts the machine id comes from `/etc/machine-id` on Linux and from the platform
/// UUID that `ioreg` prints on a Mac, and that an empty file, a missing file and output
/// without the UUID are errors.
#[test]
fn the_machine_id_is_read_per_platform() {
    let linux = linux_host();
    let id = machine_id(&linux.platform, &linux.commands, &linux.files).unwrap();
    assert_eq!(id, MACHINE_ID);
    let mac = mac_host();
    let id = machine_id(&mac.platform, &mac.commands, &mac.files).unwrap();
    assert_eq!(id, PLATFORM_UUID);

    let mut empty = linux_host();
    empty.files = empty.files.with("/etc/machine-id", "\n");
    let said = machine_id(&empty.platform, &empty.commands, &empty.files).unwrap_err();
    assert_eq!(said, "/etc/machine-id is empty");
    let mut none = linux_host();
    none.files = Default::default();
    assert!(machine_id(&none.platform, &none.commands, &none.files).is_err());

    let mut silent = mac_host();
    silent.commands = silent.commands.with(IOREG_CMD, Ok("+-o Root\n"));
    let said = machine_id(&silent.platform, &silent.commands, &silent.files).unwrap_err();
    assert_eq!(said, "ioreg printed no IOPlatformUUID");
    let mut down = mac_host();
    down.commands = down.commands.with(IOREG_CMD, Err("ioreg: not found"));
    assert!(machine_id(&down.platform, &down.commands, &down.files).is_err());
}

/// Asserts that the default host id is a salted hash of the machine id and never the
/// machine id: the id is `h-` and 32 lower case hex digits, the same machine gives the
/// same id, another machine another, and the raw id appears nowhere in it or in the hello
/// the program sends. It catches an id that leaks the machine id, one that is not a valid
/// host id, and one that changes from run to run.
#[test]
fn host_id_is_hashed_not_raw() {
    let id = host_id(MACHINE_ID);
    assert_eq!(id.len(), 34);
    assert!(id.starts_with("h-"));
    assert!(
        id[2..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert!(!id.contains(MACHINE_ID));
    assert_eq!(id, host_id(MACHINE_ID));
    assert_ne!(id, host_id("another-machine"));

    let (ran, sent, _) = run_scripted(&[], &linux_host(), vec![crate::support::welcome(1)], 1);
    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    let hello = sent.borrow()[0].1.clone();
    assert_eq!(hello["host_id"], id);
    assert!(!hello.to_string().contains(MACHINE_ID));
    assert!(!ran.err.contains(MACHINE_ID));
}

/// The real runner and reader: a program that succeeds, one that exits non-zero, one that
/// is absent, a file that exists and one that does not. The error of a failed program
/// names it and prints none of its output.
#[test]
fn the_system_runner_and_reader_report_success_and_failure() {
    let printed = SystemCommands.run("echo", &["recorded"]).unwrap();
    assert_eq!(printed, "recorded\n");
    let failed = SystemCommands.run("false", &[]).unwrap_err();
    assert!(failed.starts_with("false: "), "{failed}");
    let absent = SystemCommands
        .run("js-agent-no-such-program", &[])
        .unwrap_err();
    assert!(absent.starts_with("js-agent-no-such-program: "), "{absent}");

    let own = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    assert!(SystemFiles.read(own).unwrap().contains("js-agent"));
    let said = SystemFiles.read("/nonexistent/js-agent").unwrap_err();
    assert!(said.starts_with("/nonexistent/js-agent: "), "{said}");
}

/// The platform of this build names a non-empty OS and architecture.
#[test]
fn the_current_platform_is_named() {
    let here = Platform::current();
    assert!(!here.os.is_empty() && !here.arch.is_empty());
    assert_eq!(platform("linux", "x86_64").os, "linux");
}
