//! Which environment names a bundle or a job may set (format section 1.3). The reserved
//! list is [`js_types::env`]'s, the same one the agent's wire check reads.

use js_types::env::MAX_ENV_NAME_LEN;

use crate::error::EnvNameError;

/// Checks an environment name a bundle rule or a job sets: `[A-Za-z_][A-Za-z0-9_]*`, at
/// most [`MAX_ENV_NAME_LEN`] bytes, and not reserved
/// (not checked here).
pub fn check_env_name(name: &str) -> Result<(), EnvNameError> {
    let mut bytes = name.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_'));
    let rest_ok = bytes.all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'));
    if !(first_ok && rest_ok) {
        return Err(EnvNameError::Malformed);
    }
    if name.len() > MAX_ENV_NAME_LEN {
        return Err(EnvNameError::TooLong {
            max: MAX_ENV_NAME_LEN,
        });
    }
    Ok(())
}
