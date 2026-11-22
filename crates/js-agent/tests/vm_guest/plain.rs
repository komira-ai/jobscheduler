//! A plain run: `crun run` of a bundle written from the prepared image root and the
//! request's arguments, in fresh pid, ipc, uts, mount and network namespaces (loopback only),
//! the root read-only, no new privileges. Nothing else of the agent's: no cgroup of ours, no
//! scratch, no firewall. Its standard output and error are piped back in the reply.
//!
//! The wall-time limit is the guest's: a timer thread sends `crun kill <id> KILL` when it is
//! reached. A `kill` request is `crun kill <id> <signal>`.

use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use js_agent::linux_vm::wire::{OUTPUT_TAIL, Reply, tail};
use js_mac_session::disclaim::Program;

use crate::serve::Shared;

/// The bundle's `config.json` for `args` on `rootfs`.
fn config(rootfs: &Path, args: &[String]) -> serde_json::Value {
    serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": args,
            "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "cwd": "/",
            "noNewPrivileges": true,
            "capabilities": {
                "bounding": ["CAP_KILL", "CAP_NET_BIND_SERVICE"],
                "effective": ["CAP_KILL", "CAP_NET_BIND_SERVICE"],
                "permitted": ["CAP_KILL", "CAP_NET_BIND_SERVICE"]
            }
        },
        "root": {"path": rootfs, "readonly": true},
        "hostname": "job",
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc"},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
             "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]},
            {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs",
             "options": ["nosuid", "nodev", "mode=1777"]}
        ],
        "linux": {
            "namespaces": [
                {"type": "pid"}, {"type": "ipc"}, {"type": "uts"},
                {"type": "mount"}, {"type": "network"}
            ],
            "maskedPaths": ["/proc/kcore", "/proc/keys", "/sys/firmware"],
            "readonlyPaths": ["/proc/sys", "/proc/sysrq-trigger"]
        }
    })
}

/// Whether `id` is a name `crun` and the bundle directory take: letters, digits, `-`, `_`.
fn plain_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A `run` request.
pub fn run(shared: &Shared, id: &str, args: &[String], wall_ms: u64) -> Reply {
    if !plain_name(id) {
        return Reply::refused(format!("{id:?} is not a container name"));
    }
    if args.is_empty() {
        return Reply::refused("a run takes its process's arguments");
    }
    {
        let Ok(mut book) = shared.book.lock() else {
            return Reply::refused("the book is poisoned");
        };
        if book.running.contains(id) || book.exits.contains_key(id) {
            return Reply::refused(format!("{id} is already known"));
        }
        book.running.insert(id.to_owned());
    }
    let reply = run_bundle(shared, id, args, wall_ms);
    if let Ok(mut book) = shared.book.lock() {
        book.running.remove(id);
        book.exits.insert(id.to_owned(), reply.clone());
    }
    reply
}

/// Writes the bundle, runs it and removes the bundle.
fn run_bundle(shared: &Shared, id: &str, args: &[String], wall_ms: u64) -> Reply {
    let flags = &shared.flags;
    let bundle = flags.runs.join(id);
    if let Err(why) = fs::create_dir(&bundle).and_then(|()| {
        fs::write(
            bundle.join("config.json"),
            config(&flags.rootfs, args).to_string(),
        )
    }) {
        return Reply::refused(format!("write the bundle {}: {why}", bundle.display()));
    }
    let words: [&OsStr; 4] = [
        OsStr::new("run"),
        OsStr::new("--bundle"),
        bundle.as_os_str(),
        OsStr::new(id),
    ];
    let spawned = Program::new(&flags.crun, words)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(why) => {
            let _ = fs::remove_dir_all(&bundle);
            return Reply::refused(format!("start crun: {why}"));
        }
    };
    let done = Arc::new(AtomicBool::new(false));
    let hit = Arc::new(AtomicBool::new(false));
    if wall_ms > 0 {
        let (done, hit, crun, id) = (
            Arc::clone(&done),
            Arc::clone(&hit),
            flags.crun.clone(),
            id.to_owned(),
        );
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(wall_ms));
            if !done.load(Ordering::SeqCst) {
                hit.store(true, Ordering::SeqCst);
                let _ = Program::new(&crun, ["kill", id.as_str(), "KILL"]).output();
            }
        });
    }
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(err) = stderr.as_mut() {
            let _ = err.read_to_end(&mut bytes);
        }
        bytes
    });
    let mut out = Vec::new();
    if let Some(o) = stdout.as_mut() {
        let _ = o.read_to_end(&mut out);
    }
    let status = child.wait();
    done.store(true, Ordering::SeqCst);
    let err = err_reader.join().unwrap_or_default();
    let _ = fs::remove_dir_all(&bundle);
    match status {
        Err(why) => Reply::refused(format!("wait for crun: {why}")),
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt as _;
            Reply {
                ok: true,
                id: id.to_owned(),
                exit: status.code(),
                signal: status.signal(),
                stdout: tail(&out, OUTPUT_TAIL),
                stderr: tail(&err, OUTPUT_TAIL),
                wall_limit_hit: hit.load(Ordering::SeqCst),
                ..Reply::default()
            }
        }
    }
}

/// A `kill` request.
pub fn kill(shared: &Shared, id: &str, signal: &str) -> Reply {
    if !plain_name(id) || !plain_name(signal) {
        return Reply::refused("a kill takes a container name and a signal name");
    }
    match Program::new(&shared.flags.crun, ["kill", id, signal]).output() {
        Ok(out) if out.status.success() => Reply {
            ok: true,
            id: id.to_owned(),
            ..Reply::default()
        },
        Ok(out) => Reply::refused(format!(
            "crun kill: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(why) => Reply::refused(format!("start crun: {why}")),
    }
}

/// A `wait` request: the kept exit, waiting while the run is still running.
pub fn wait(shared: &Shared, id: &str) -> Reply {
    loop {
        {
            let Ok(book) = shared.book.lock() else {
                return Reply::refused("the book is poisoned");
            };
            if let Some(exit) = book.exits.get(id) {
                return exit.clone();
            }
            if !book.running.contains(id) {
                return Reply::refused(format!("no run {id}"));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
