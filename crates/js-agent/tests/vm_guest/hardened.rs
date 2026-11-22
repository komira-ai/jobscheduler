//! A hardened run: the agent's own crun runtime (`CrunRuntime`, opened as the agent opens
//! it by tests/runtime_host) with its egress firewall started in allow-list mode for the
//! request's endpoints. So the container gets what a Linux node gives a job: the job user in
//! a user namespace, `no_new_privs` and the seccomp filter, its cgroup under the runtime's,
//! its scratch directory, and its own network namespace wired through the host's veth with
//! the nftables rules that let it reach the allowed endpoints and nothing else.
//!
//! Each request opens the firewall and the runtime and drops both after the run, so the
//! allow-list is the request's. One hardened run at a time.

use std::fs;
use std::time::{Duration, Instant};

use js_agent::crun::start_egress;
use js_agent::egress::{Config, Egress, Flags as EgressFlags};
use js_agent::lifecycle::{Runtime as _, Status};
use js_agent::linux_vm::wire::{OUTPUT_TAIL, Reply};

use crate::runtime_host::{RuntimeHost, Setup};
use crate::serve::Shared;

/// How long a hardened run may take before it is killed and reported.
const LIMIT: Duration = Duration::from_secs(300);

/// A `hardened` request.
pub fn run(shared: &Shared, id: &str, script: &str, allow: &[String]) -> Reply {
    let Ok(_one) = shared.hardened.lock() else {
        return Reply::refused("the hardened lock is poisoned");
    };
    match run_inner(shared, id, script, allow) {
        Ok(reply) => reply,
        Err(why) => Reply::refused(why),
    }
}

fn run_inner(shared: &Shared, id: &str, script: &str, allow: &[String]) -> Result<Reply, String> {
    let flags = &shared.flags;
    let busybox = flags
        .busybox
        .as_deref()
        .ok_or("the guest program was started without --busybox")?;
    let work = flags
        .hardened_work
        .as_deref()
        .ok_or("the guest program was started without --hardened-work")?
        .join(id);
    fs::create_dir_all(&work).map_err(|why| format!("make {}: {why}", work.display()))?;
    let daemon = std::env::current_exe().map_err(|why| format!("this program's path: {why}"))?;
    let setup = Setup {
        crun: &flags.crun,
        busybox,
        job_user: flags.job_user,
        daemon: &daemon,
        leases_beside: true,
    };
    let config = Config::from_flags(&EgressFlags {
        mode: Some("allow-list"),
        allow,
        ..EgressFlags::default()
    })
    .map_err(|why| format!("the allow-list: {why}"))?;
    let egress = start_egress(&config, |config| Egress::start(config, None))
        .map_err(|why| format!("start the egress: {why}"))?;
    let mut host = RuntimeHost::open(
        "vmguest",
        &format!("vm-{id}"),
        &work,
        &setup,
        egress.as_ref(),
    )?;
    let spec = host.spec(id, script)?;
    for dir in [host.runtime.lease_dir(id), host.runtime.tmp_dir(id)]
        .into_iter()
        .flatten()
    {
        fs::create_dir(&dir).map_err(|why| format!("make {}: {why}", dir.display()))?;
    }
    host.runtime
        .check_egress()
        .map_err(|why| format!("check the egress: {why}"))?;
    let handle = host
        .runtime
        .stage(&spec)
        .map_err(|why| format!("stage: {why}"))?;
    host.runtime
        .start_staged(&handle)
        .map_err(|why| format!("start: {why}"))?;
    let began = Instant::now();
    let exit = loop {
        match host.runtime.poll(&handle) {
            Ok(Status::Exited(exit)) => break Some(exit),
            Ok(Status::Gone) => break None,
            Ok(Status::Running) | Err(_) => {}
        }
        if began.elapsed() > LIMIT {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let tail = host
        .runtime
        .log_tail(&handle)
        .ok()
        .flatten()
        .unwrap_or_default();
    let _ = host.runtime.remove(&handle);
    drop(host);
    drop(egress);
    let cut = |text: String| js_agent::linux_vm::wire::tail(text.as_bytes(), OUTPUT_TAIL);
    Ok(Reply {
        ok: exit.is_some(),
        id: id.to_owned(),
        exit: exit.map(|e| e.code),
        stdout: cut(tail.stdout),
        stderr: cut(tail.stderr),
        error: if exit.is_some() {
            String::new()
        } else {
            "the run was gone or over its limit".to_owned()
        },
        ..Reply::default()
    })
}
