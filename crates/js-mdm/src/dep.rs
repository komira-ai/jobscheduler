//! `js-mdm dep`: give NanoDEP the Automated Device Enrollment profile, and assign it to
//! devices Apple Business Manager already lists. (Stubs: the tests come first.)

use std::io::Write;

use js_host_step::Mode;
use js_mdm_core::config::MdmConfig;

use crate::{Env, MdmError};

/// A device serial number as Apple Business Manager lists it: 8 to 14 upper-case letters and
/// digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serial(String);

impl Serial {
    /// `text` as a serial number.
    ///
    /// # Errors
    /// What a serial number is, naming `text`.
    pub fn new(text: &str) -> Result<Serial, String> {
        let ok = (8..=14).contains(&text.len())
            && text
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        match ok {
            true => Ok(Serial(text.to_owned())),
            false => Err(format!(
                "{text:?} is not a serial number (8 to 14 upper-case letters and digits)"
            )),
        }
    }

    /// The serial number.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Define the ADE profile (stub: does nothing).
///
/// # Errors
/// None yet.
pub fn define_profile(
    env: &Env<'_>,
    c: &MdmConfig,
    replace: bool,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let _ = (env, c, replace, mode, out);
    Ok(())
}

/// Assign the profile (stub: does nothing).
///
/// # Errors
/// None yet.
pub fn assign(
    env: &Env<'_>,
    c: &MdmConfig,
    serials: &[Serial],
    mode: Mode,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    let _ = (env, c, serials, mode, out);
    Ok(true)
}
