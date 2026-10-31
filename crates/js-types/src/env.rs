//! The environment names a job or a bundle may not set: one list for every crate that
//! checks a name, so an addition is made once.
//!
//! This holds the reserved list only. The grammar of a name (`[A-Za-z_][A-Za-z0-9_]*`)
//! and what to do with a refusal stay with each caller, which has its own error type.

/// Name prefixes that are reserved: the dynamic loader's (`DYLD_`, `LD_`), the sandbox's
/// (`SANDBOX_`), the agent's own (`JS_`), launchd's (`__XPC_`), the allocator's debug
/// switches (`MALLOC` covers `MALLOC_*`, `Malloc` covers `MallocStackLogging` and
/// `MallocLogFile`, which make the allocator write files) and the Objective-C runtime's
/// (`OBJC_`), and a shell's imported functions (`BASH_FUNC_`, as in `BASH_FUNC_ulimit%%`,
/// which would replace the `ulimit` of the shell that sets a run's limits).
pub const RESERVED_ENV_PREFIXES: [&str; 9] = [
    "DYLD_",
    "LD_",
    "SANDBOX_",
    "JS_",
    "__XPC_",
    "MALLOC",
    "Malloc",
    "OBJC_",
    "BASH_FUNC_",
];

/// Names a shell reads before its first command: its options (`SHELLOPTS`, `BASHOPTS`),
/// the file it sources (`BASH_ENV`, `ENV`), a prompt it evaluates (`PS4` under `xtrace`,
/// `PROMPT_COMMAND`) and what steers its word splitting and `cd` (`IFS`, `CDPATH`). The
/// agent starts a run through a shell that sets its limits, so a job may not set one.
pub const SHELL_ENV_NAMES: [&str; 8] = [
    "SHELLOPTS",
    "BASHOPTS",
    "BASH_ENV",
    "ENV",
    "PS4",
    "PROMPT_COMMAND",
    "IFS",
    "CDPATH",
];

/// Names the agent owns: it sets each for every run, so no job or bundle may.
pub const OWNED_ENV_NAMES: [&str; 6] = [
    "HOME",
    "TMPDIR",
    "CFFIXED_USER_HOME",
    "XDG_CACHE_HOME",
    "CLANG_MODULE_CACHE_PATH",
    "DEVELOPER_DIR",
];

/// The longest environment variable name, in bytes.
pub const MAX_ENV_NAME_LEN: usize = 128;

/// Whether `name` starts with a [`RESERVED_ENV_PREFIXES`] entry or is one of the
/// [`OWNED_ENV_NAMES`] or [`SHELL_ENV_NAMES`]. The comparison is exact and case-sensitive.
pub fn env_name_reserved(name: &str) -> bool {
    RESERVED_ENV_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || OWNED_ENV_NAMES.contains(&name)
        || SHELL_ENV_NAMES.contains(&name)
}
