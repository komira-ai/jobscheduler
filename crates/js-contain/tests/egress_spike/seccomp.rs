//! The real seccomp profile of the container runner, as a `linux.seccomp` section.
//!
//! The profile is `src/oci/seccomp.json` (containers/common 0.57.4, pinned by a hash in
//! `hardening.rs`), included as it is. `hardening::seccomp` is crate-private, so the
//! resolution that function does (keep an entry when its `includes`/`excludes` of
//! architectures and capabilities hold for the job, keep names, action, errno and
//! arguments) is repeated here for a job that holds NO capability, which is what the plan's
//! hardening step gives the job. This copy is the test's, not the driver's.

use serde_json::{Value, json};

const PROFILE: &str = include_str!("../../src/oci/seccomp.json");

/// The Go name of this machine's architecture.
fn go_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

fn names(call: &Value, filter: &str, key: &str) -> Vec<String> {
    call[filter][key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// Whether an entry applies to a job of this architecture that holds no capability.
fn applies(call: &Value) -> bool {
    let arch = go_arch();
    let (include, exclude) = (
        names(call, "includes", "arches"),
        names(call, "excludes", "arches"),
    );
    if !include.is_empty() && !include.iter().any(|a| a == arch) {
        return false;
    }
    if exclude.iter().any(|a| a == arch) {
        return false;
    }
    names(call, "includes", "caps").is_empty()
}

/// The `linux.seccomp` section, and the number of syscall entries in it.
pub fn section() -> (Value, usize) {
    let profile: Value = serde_json::from_str(PROFILE).expect("the pinned profile is JSON");
    let native = if go_arch() == "arm64" {
        "SCMP_ARCH_AARCH64"
    } else {
        "SCMP_ARCH_X86_64"
    };
    let entry = profile["archMap"]
        .as_array()
        .expect("archMap")
        .iter()
        .find(|e| e["architecture"] == native)
        .expect("this architecture");
    let mut architectures = vec![entry["architecture"].clone()];
    architectures.extend(
        entry["subArchitectures"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    let syscalls: Vec<Value> = profile["syscalls"]
        .as_array()
        .expect("syscalls")
        .iter()
        .filter(|call| applies(call))
        .map(|call| {
            let mut out = json!({"names": call["names"], "action": call["action"]});
            if let Some(errno) = call.get("errnoRet").filter(|v| !v.is_null()) {
                out["errnoRet"] = errno.clone();
            }
            if let Some(args) = call["args"].as_array().filter(|a| !a.is_empty()) {
                out["args"] = Value::Array(args.clone());
            }
            out
        })
        .collect();
    let count = syscalls.len();
    (
        json!({
            "defaultAction": profile["defaultAction"],
            "defaultErrnoRet": profile["defaultErrnoRet"],
            "architectures": architectures,
            "syscalls": syscalls,
        }),
        count,
    )
}
