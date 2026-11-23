//! The load the heartbeat carries on a host of the crun runtime: the usage is sampled, through
//! the machine's own file reader, from the actions cgroup the runtime delegated when it opened.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent::crun::{CgroupSettings, CgroupSource, Seams};
use js_agent::flags_file::FileFacts;
use js_agent::usage::{Collector, Outbox, UsageClock};
use js_agent::{Backend, FileReader, Machine as _, Platform, RuntimeBuild, System, SystemFiles};
use js_testkit::dir::TestDir;

use super::crun_e2e::{NODE_IN_USE, crun_settings, seams};

/// The machine's own file reader, except that the host's `/proc/stat` is a fixed text (300
/// busy ticks), so the test reads the real cgroup files and not the host's CPU.
struct FixedProcStat;

impl FileReader for FixedProcStat {
    fn read(&self, path: &str) -> Result<String, String> {
        match path {
            "/proc/stat" => Ok("cpu  100 0 200 900 0 0 0 0 0 0\n".to_owned()),
            _ => SystemFiles.read(path),
        }
    }

    fn link(&self, path: &str) -> Result<String, String> {
        SystemFiles.link(path)
    }

    fn device(&self, path: &Path) -> Result<u64, String> {
        SystemFiles.device(path)
    }

    fn is_tmpfs(&self, path: &Path) -> Result<bool, String> {
        SystemFiles.is_tmpfs(path)
    }

    fn facts(&self, path: &Path) -> io::Result<FileFacts> {
        SystemFiles.facts(path)
    }

    fn entries(&self, dir: &Path) -> Result<Vec<String>, io::Error> {
        SystemFiles.entries(dir)
    }
}

/// A clock that always reads `0`: one run is all the test takes.
struct Zero;

impl UsageClock for Zero {
    fn now_usec(&self) -> u64 {
        0
    }
}

/// A cgroup mount with the agent's `root` and `actions` cgroups, the files the runtime opens
/// there, and the node files the usage reads in `actions`: `usage_usec` of CPU and 4096 bytes
/// of memory, 1024 of them inactive file pages, and no CPU limit.
fn mount_with_actions(mount: &Path, usage_usec: u64) {
    for part in ["root", "actions"] {
        fs::create_dir_all(mount.join(part)).expect("cgroup");
    }
    let files = [
        ("root/cgroup.procs", String::new()),
        ("actions/cgroup.procs", String::new()),
        ("actions/cgroup.threads", String::new()),
        ("actions/cgroup.subtree_control", String::new()),
        ("actions/cpu.stat", format!("usage_usec {usage_usec}\n")),
        ("actions/memory.current", "4096\n".to_owned()),
        (
            "actions/memory.stat",
            "inactive_file 1024\nactive_file 0\n".to_owned(),
        ),
        ("actions/cpu.max", "max 100000\n".to_owned()),
    ];
    for (file, text) in files {
        fs::write(mount.join(file), text).expect("a cgroup file");
    }
}

/// Asserts that once the crun runtime has opened, the usage read through the machine's file
/// reader from the actions cgroup it recorded is the node's: one run of the collector
/// publishes a load with the `usage_usec` and the working set of the actions cgroup's files,
/// beside the host's CPU busy time (a fixed `/proc/stat`, 300 ticks of 10 ms).
/// The runtime names its cgroups relative to the mount (`/actions`); the usage reads files, so
/// it must be given the directory below the mount. It catches the delegated cgroup's name
/// read as a path from `/`, where no `cpu.stat` is: every heartbeat of a crun host then
/// carries no load, and the job service charges every host by its bookings alone.
#[test]
fn a_crun_host_samples_its_load_from_the_delegated_actions_cgroup() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "crun-usage");
    let mount = dir.path().join("cgroup");
    mount_with_actions(&mount, 123_456);
    let settings = crun_settings();
    let platform = Platform {
        os: "linux".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    };
    let backend = Backend::Container(settings.clone());
    let build = RuntimeBuild {
        backend: &backend,
        host_id: "h1",
        platform: &platform,
        stop_grace: Duration::from_secs(1),
    };
    let cgroups = CgroupSource::Fixed(CgroupSettings {
        mount: mount.clone(),
        root: "/root".to_owned(),
        actions: "/actions".to_owned(),
    });
    let machine = System::new().with_crun(Seams { cgroups, ..seams() });
    machine.runtime(&build).expect("it opens");

    let actions = settings.actions_cgroup().expect("the runtime recorded it");
    let outbox = Arc::new(Outbox::new());
    outbox.set_window(Duration::from_secs(5));
    let mut collector = Collector::new(
        Box::new(FixedProcStat),
        actions,
        Box::new(Zero),
        outbox.clone(),
    );
    assert_eq!(collector.run(&[]), Vec::<String>::new());
    let load = outbox.take().expect("the heartbeat carries a load");
    assert_eq!(
        (
            load.seq,
            load.cpu_usage_usec,
            load.working_set_bytes,
            load.actions_cpu_limited,
            load.host_cpu_usec
        ),
        (1, 123_456, 3072, false, Some(3_000_000))
    );
}
