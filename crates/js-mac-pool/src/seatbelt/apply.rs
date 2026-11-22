//! What applying a profile inside the run's own process takes.

use std::ffi::{CStr, CString};

/// Why a profile cannot be handed to the in-process apply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InitError {
    /// The profile text holds a NUL byte.
    #[error("the profile text holds a NUL byte")]
    TextNul,
    /// A parameter's name or value holds a NUL byte; the argument is the name, a NUL in it
    /// spelled `\0`.
    #[error("the profile parameter {0} holds a NUL byte")]
    ParamNul(String),
}

/// A profile's text and parameters as the C strings that `sandbox_init_with_parameters`
/// takes: the text, and the parameters as one list of name, value, name, value. The
/// caller adds the null pointer that ends the list.
///
/// Every string is checked when this is made, so a caller makes it before it forks and
/// the step between fork and exec has nothing left to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInit {
    text: CString,
    params: Vec<CString>,
}

impl SandboxInit {
    /// The C strings of `text` and `params` (name and value, in order).
    ///
    /// # Errors
    /// [`InitError::TextNul`] for a NUL in the text, [`InitError::ParamNul`] for one in a
    /// parameter's name or value.
    pub fn new(text: &str, params: &[(String, String)]) -> Result<Self, InitError> {
        let text = CString::new(text).map_err(|_| InitError::TextNul)?;
        let mut flat = Vec::with_capacity(params.len() * 2);
        for (name, value) in params {
            let refused = || InitError::ParamNul(name.replace('\0', "\\0"));
            flat.push(CString::new(name.as_str()).map_err(|_| refused())?);
            flat.push(CString::new(value.as_str()).map_err(|_| refused())?);
        }
        Ok(Self { text, params: flat })
    }

    /// The profile text.
    #[must_use]
    pub fn text(&self) -> &CStr {
        &self.text
    }

    /// The parameters: name, value, name, value, in the profile's order.
    #[must_use]
    pub fn params(&self) -> &[CString] {
        &self.params
    }
}
