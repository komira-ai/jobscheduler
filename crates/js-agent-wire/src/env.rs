//! The one check of an environment: a run's (`run.env`), a native body's (`native.env`) and
//! a job start's (`env`) are held to [`check_env`], and only the field a refusal names
//! differs. The agent's runtimes call it again on the environment they are handed, so a
//! path that skips the wire still meets the reserved names.

use std::collections::BTreeMap;

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::refusal;

/// Checks an environment, refusing it [`refusal::INVALID_ENV`] under `field`: at most
/// [`limits::MAX_ENV`] entries; each name non-empty, at most [`limits::MAX_ENV_KEY_LEN`]
/// bytes of `[A-Za-z_][A-Za-z0-9_]*` and not reserved
/// ([`js_types::env::env_name_reserved`]); each value at most
/// [`limits::MAX_ENV_VALUE_LEN`] bytes without NUL; and at most
/// [`limits::MAX_ENV_TOTAL_BYTES`] bytes of names and values in all. An empty environment
/// passes.
///
/// # Errors
///
/// The first rule the environment breaks, naming `field` and never a name or a value.
pub fn check_env(
    field: &'static str,
    env: &BTreeMap<String, String>,
) -> Result<(), ValidationError> {
    let at = At::new(field, refusal::INVALID_ENV);
    check::count(at, env.len(), limits::MAX_ENV)?;
    let mut total = 0usize;
    for (key, value) in env {
        env_name(at, key)?;
        if value.len() > limits::MAX_ENV_VALUE_LEN {
            return Err(at.fail(Rule::TooLong));
        }
        if value.contains('\0') {
            return Err(at.fail(Rule::BadChars));
        }
        total += key.len() + value.len();
    }
    if total > limits::MAX_ENV_TOTAL_BYTES {
        return Err(at.fail(Rule::TooLong));
    }
    Ok(())
}

fn env_name(at: At, key: &str) -> Result<(), ValidationError> {
    if key.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if key.len() > limits::MAX_ENV_KEY_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    let lead = key.as_bytes()[0];
    let leads = lead.is_ascii_alphabetic() || lead == b'_';
    let inside = key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ![leads, inside].into_iter().all(|ok| ok) {
        return Err(at.fail(Rule::BadChars));
    }
    if js_types::env::env_name_reserved(key) {
        return Err(at.fail(Rule::Reserved));
    }
    Ok(())
}
