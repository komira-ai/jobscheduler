//! What the agent reads from the host, through recorded program output.

use std::fs;
use std::path::Path;
use std::time::Duration;

use js_agent::{
    APPLE_GPU_CLASS, CommandRunner, FileReader, Platform, SystemCommands, SystemFiles, detect,
    host_id, machine_id,
};
use rustix::process::{Pid, test_kill_process};

use crate::support::{
    AMD_MAC, CPU_CMD, IOREG_CMD, MACHINE_ID, MEMSIZE_CMD, NVIDIA_CMD, NVIDIA_MIXED, PLATFORM_UUID,
    PROFILER_CMD, Scratch, linux_host, mac_host, platform, run_scripted,
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
    assert!(!ran.iter().any(|line| line.contains("nvidia-smi")));
}

/// Asserts that every helper program the agent starts is named by an absolute path in a
/// system directory, on both platforms, for the facts and for the machine id: none is
/// looked up on the `PATH` the agent inherited, where a directory the operator does not
/// control could put a program of the same name first. It catches a bare program name.
#[test]
fn helpers_are_started_by_absolute_path() {
    for host in [linux_host(), mac_host()] {
        detect(&host.platform, &host.commands, &host.files).unwrap();
        machine_id(&host.platform, &host.commands, &host.files).ok();
        let ran = host.commands.ran.borrow();
        assert!(!ran.is_empty());
        for line in ran.iter() {
            let program = line.split(' ').next().unwrap();
            let system = program.starts_with("/usr/bin/") || program.starts_with("/usr/sbin/");
            assert!(system, "{line}");
        }
    }
}

/// Asserts that a probe the runner gave up on (it never answered, or printed more than
/// the cap) is a failed GPU probe like any other: no GPU, one warning that says so, and
/// the start goes on. The runner's own bounds are tested against real processes below.
#[test]
fn a_probe_the_runner_gave_up_on_offers_no_gpu() {
    for why in [
        "/usr/bin/nvidia-smi: timed out after 10s",
        "/usr/bin/nvidia-smi: printed more than 16384 bytes",
    ] {
        let mut host = linux_host();
        host.commands = host.commands.with(NVIDIA_CMD, Err(why));
        let found = detect(&host.platform, &host.commands, &host.files).unwrap();
        assert_eq!(found.gpus, js_agent::Gpus::none(), "{why}");
        assert_eq!(found.warnings.len(), 1, "{why}");
        assert!(found.warnings[0].contains(why), "{why}");
    }
}

/// Asserts that a stop request that comes while a probe is hung is not lost: the probe
/// gives up, the agent warns and says hello, and the first wait ends with the stop, so the
/// agent announces the drain and exits 0. It catches a start that ignores a stop request
/// until the server has been reached for good.
#[test]
fn a_stop_after_a_probe_that_gave_up_still_stops_the_agent() {
    let mut host = linux_host();
    host.commands = host
        .commands
        .with(NVIDIA_CMD, Err("/usr/bin/nvidia-smi: timed out after 10s"));
    let replies = vec![crate::support::welcome(1), crate::support::beat_ok()];
    let (ran, sent, waits) = run_scripted(&["--host-id", "wedged"], &host, replies, 1);
    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    assert!(ran.err.contains("warning: GPU probe failed"), "{}", ran.err);
    assert!(ran.err.contains("draining: the server was told"));
    assert_eq!(waits.len(), 1);
    assert_eq!(sent.borrow().len(), 2);
}

/// Asserts that GPU text the wire would refuse (a model or uuid over the cap, a control
/// character) drops the GPUs with one warning instead of ending the agent with status 2:
/// the hello offers no GPU, no device and no class, keeps the CPU and memory, and stderr
/// says the GPU facts were not acceptable. It catches a host that cannot register because
/// its driver printed a long or odd name.
#[test]
fn gpu_text_the_wire_refuses_drops_the_gpus_with_a_warning() {
    let long = "x".repeat(129);
    let cases = [
        ("long model", format!("0, GPU-1, NVIDIA {long}, 81920\n")),
        (
            "long uuid",
            format!("0, GPU-{long}, NVIDIA A100-SXM4-80GB, 81920\n"),
        ),
        (
            "control character",
            "0, GPU-\u{1b}[2J, NVIDIA A100-SXM4-80GB, 81920\n".to_owned(),
        ),
    ];
    for (why, output) in cases {
        let mut host = linux_host();
        host.commands = host.commands.with(NVIDIA_CMD, Ok(&output));
        let replies = vec![crate::support::welcome(1)];
        let (ran, sent, _) = run_scripted(&["--host-id", "odd-gpu"], &host, replies, 1);
        assert_eq!(ran.status(), crate::support::code(0), "{why}: {}", ran.err);
        let warning = "js-agent: warning: GPU probe failed (the wire refuses the GPU facts: ";
        assert!(ran.err.contains(warning), "{why}: {}", ran.err);
        assert!(ran.err.contains("offering no GPU"), "{why}: {}", ran.err);
        assert!(!ran.err.contains('\u{1b}') && !ran.err.contains(&long));
        let hello = &sent.borrow()[0].1;
        assert_eq!(hello["shape"]["gpus"], 0, "{why}");
        assert_eq!(hello["gpu_devices"], serde_json::json!([]), "{why}");
        assert!(hello["shape"].get("gpu_class").is_none(), "{why}");
        assert_eq!(hello["shape"]["cpu_millis"], 8000, "{why}");
    }
}

/// Asserts that dropping the GPUs is only for GPU facts the driver printed: a hello that
/// is refused for another reason (the host id, or the class the operator named with
/// `--gpu-class`) still ends the agent with status 2 and that reason, and says nothing
/// about dropping GPUs; GPU facts the wire accepts are kept. It catches a check that
/// swallows a mistake of the operator's as a warning.
#[test]
fn gpus_are_dropped_only_when_they_are_the_fault() {
    let (ran, sent, _) = run_scripted(&["--host-id", "Not Valid"], &linux_host(), vec![], 1);
    assert_eq!(ran.status(), crate::support::code(2), "{}", ran.err);
    assert!(ran.err.contains("host_id"), "{}", ran.err);
    assert!(!ran.err.contains("GPU"), "{}", ran.err);
    assert!(sent.borrow().is_empty());

    let flags = ["--host-id", "fine", "--gpu-class", "Not A Class"];
    let (ran, sent, _) = run_scripted(&flags, &linux_host(), vec![], 1);
    assert_eq!(ran.status(), crate::support::code(2), "{}", ran.err);
    assert!(ran.err.contains("shape.gpu_class"), "{}", ran.err);
    assert!(!ran.err.contains("GPU"), "{}", ran.err);
    assert!(sent.borrow().is_empty());

    let replies = vec![crate::support::welcome(1)];
    let (ran, sent, _) = run_scripted(&["--host-id", "fine"], &linux_host(), replies, 1);
    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    assert!(!ran.err.contains("GPU"), "{}", ran.err);
    assert_eq!(sent.borrow()[0].1["shape"]["gpus"], 2);
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

/// Asserts that a host without `getconf` is still counted, by the standard library, with
/// a warning that names the failure and the stand-in, on both platforms and in the text
/// the program prints (a `getconf` that prints no number stays an error, see
/// `missing_cpu_or_memory_facts_are_errors`). It catches a start that fails on a minimal
/// image, and a fallback that is silent.
#[test]
fn a_missing_getconf_falls_back_to_the_standard_count_with_a_warning() {
    let counted = std::thread::available_parallelism().unwrap().get() as u64;
    let note = "getconf: not found; counted the CPUs with the standard library instead";
    for mut host in [linux_host(), mac_host()] {
        host.commands = host.commands.with(CPU_CMD, Err("getconf: not found"));
        let found = detect(&host.platform, &host.commands, &host.files).unwrap();
        assert_eq!(found.cpu_millis, counted * 1000);
        assert_eq!(found.warnings, [note]);
    }
    let mut host = linux_host();
    host.commands = host.commands.with(CPU_CMD, Err("getconf: not found"));
    let (ran, sent, _) = run_scripted(&[], &host, vec![crate::support::welcome(1)], 1);
    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    assert!(
        ran.err.contains(&format!("js-agent: warning: {note}")),
        "{}",
        ran.err
    );
    assert_eq!(sent.borrow()[0].1["shape"]["cpu_millis"], counted * 1000);
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
    let runner = SystemCommands::default();
    let printed = runner.run("echo", &["recorded"]).unwrap();
    assert_eq!(printed, "recorded\n");
    let failed = runner.run("false", &[]).unwrap_err();
    assert!(failed.starts_with("false: "), "{failed}");
    let absent = runner.run("js-agent-no-such-program", &[]).unwrap_err();
    assert!(absent.starts_with("js-agent-no-such-program: "), "{absent}");

    let own = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    assert!(SystemFiles.read(own).unwrap().contains("js-agent"));
    let said = SystemFiles.read("/nonexistent/js-agent").unwrap_err();
    assert!(said.starts_with("/nonexistent/js-agent: "), "{said}");
}

/// The pid a probe script wrote to `file` before it blocked.
fn pid_in(file: &Path) -> Pid {
    let text = fs::read_to_string(file).unwrap();
    Pid::from_raw(text.trim().parse().unwrap()).unwrap()
}

/// Runs `script` under `sh` with `file` as `$0`, so the script can say its pid.
fn run_script(runner: &SystemCommands, script: &str, file: &Path) -> Result<String, String> {
    runner.run("sh", &["-c", script, file.to_str().unwrap()])
}

/// Asserts that a probe that never exits is given up on: the runner reports that it timed
/// out (the agent then offers no GPU), and the process is gone when it returns, killed by
/// its pid and collected, so no wedged helper is left behind. The probe is a shell that
/// says its pid and then sleeps for much longer than the test runs. It catches a runner
/// that waits for ever, and one that stops waiting but leaves the process running.
#[test]
fn a_probe_that_never_answers_is_killed_and_collected() {
    let dir = Scratch::new("probe_timeout");
    let file = dir.path("pid");
    let runner = SystemCommands::bounded(Duration::from_secs(1), 4096);
    let said = run_script(&runner, "echo $$ > \"$0\"; exec sleep 30", &file).unwrap_err();
    assert_eq!(said, "sh: timed out after 1s");
    assert!(test_kill_process(pid_in(&file)).is_err(), "still running");
}

/// The same for a probe that closes its output and then hangs: the output is complete
/// but the exit never comes.
#[test]
fn a_probe_that_prints_and_then_hangs_is_killed_and_collected() {
    let dir = Scratch::new("probe_hang");
    let file = dir.path("pid");
    let runner = SystemCommands::bounded(Duration::from_secs(1), 4096);
    let script = "echo $$ > \"$0\"; echo partial; exec >&-; exec sleep 30";
    let said = run_script(&runner, script, &file).unwrap_err();
    assert_eq!(said, "sh: timed out after 1s");
    assert!(test_kill_process(pid_in(&file)).is_err(), "still running");
}

/// Asserts that output over the cap ends the probe at once and is a failure: the error
/// says how much was allowed, the process (which would otherwise block on a full pipe or
/// run on) is killed and collected, and not more than the cap plus one byte is ever read
/// into memory. It also pins the edge: exactly the cap is accepted. The limit is an hour,
/// so the outcome does not depend on time.
#[test]
fn a_probe_that_prints_too_much_is_killed_and_collected() {
    let dir = Scratch::new("probe_oversize");
    let file = dir.path("pid");
    let runner = SystemCommands::bounded(Duration::from_secs(3600), 1024);
    let script = "echo $$ > \"$0\"; head -c 100000 /dev/zero; exec sleep 30";
    let said = run_script(&runner, script, &file).unwrap_err();
    assert_eq!(said, "sh: printed more than 1024 bytes");
    assert!(test_kill_process(pid_in(&file)).is_err(), "still running");

    let edge = SystemCommands::bounded(Duration::from_secs(3600), 5);
    assert_eq!(edge.run("printf", &["abcde"]).unwrap(), "abcde");
    let said = edge.run("printf", &["abcdef"]).unwrap_err();
    assert_eq!(said, "printf: printed more than 5 bytes");
}

/// Asserts that the cap is enforced while the output is read: a program that prints
/// without end (`/usr/bin/yes`, which never exits and never closes its output) is refused
/// as too chatty, not as timed out, and its process is gone when the runner returns. The
/// limit is far above the time the refusal takes, so a runner that reads without a bound
/// (and so never sees the cap) ends by the limit and fails the message check. It catches
/// an unbounded read.
#[test]
fn a_probe_that_prints_without_end_is_refused_at_the_cap() {
    let dir = Scratch::new("probe_endless");
    let file = dir.path("pid");
    let runner = SystemCommands::bounded(Duration::from_secs(5), 1024);
    let said = run_script(&runner, "echo $$ > \"$0\"; exec /usr/bin/yes", &file).unwrap_err();
    assert_eq!(said, "sh: printed more than 1024 bytes");
    assert!(test_kill_process(pid_in(&file)).is_err(), "still running");
}

/// The runner the program uses is bounded by default: a program that prints a megabyte
/// is refused instead of read into memory.
#[test]
fn the_default_runner_caps_what_a_probe_may_print() {
    let said = SystemCommands::default()
        .run("head", &["-c", "1000000", "/dev/zero"])
        .unwrap_err();
    assert_eq!(said, "head: printed more than 16384 bytes");
}

/// The platform of this build names a non-empty OS and architecture.
#[test]
fn the_current_platform_is_named() {
    let here = Platform::current();
    assert!(!here.os.is_empty() && !here.arch.is_empty());
    assert_eq!(platform("linux", "x86_64").os, "linux");
}
