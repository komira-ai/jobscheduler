//! Which environment names a bundle or a job may set (format section 1.3).

use crate::error::EnvNameError;

/// The prefix of the names the agent reserves for itself.
pub const RESERVED_ENV_PREFIX: &str = "JS_";

const PREFIXES: [&str; 4] = ["DYLD_", "LD_", "SANDBOX_", RESERVED_ENV_PREFIX];

/// Names the agent owns (it sets them for every run).
const OWNED: [&str; 6] = [
    "HOME",
    "TMPDIR",
    "CFFIXED_USER_HOME",
    "XDG_CACHE_HOME",
    "CLANG_MODULE_CACHE_PATH",
    "DEVELOPER_DIR",
];

/// Checks an environment name a bundle rule or a job sets: `[A-Za-z_][A-Za-z0-9_]*`,
/// not starting with `DYLD_`, `LD_`, `SANDBOX_` or [`RESERVED_ENV_PREFIX`], and not one
/// of `HOME`, `TMPDIR`, `CFFIXED_USER_HOME`, `XDG_CACHE_HOME`,
/// `CLANG_MODULE_CACHE_PATH` and `DEVELOPER_DIR`.
pub fn check_env_name(name: &str) -> Result<(), EnvNameError> {
    let mut bytes = name.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_'));
    let rest_ok = bytes.all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'));
    if !(first_ok && rest_ok) {
        return Err(EnvNameError::Malformed);
    }
    if PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || OWNED.contains(&name) {
        return Err(EnvNameError::Forbidden);
    }
    Ok(())
}
