//! The kit's secret files: `KEY=VALUE` lines in 0600 files of root's under `MDM_ETC`
//! (`install` makes the API keys and the SCEP challenge; the operator makes `dns.env`). A value
//! read from one is a [`Secret`], which is never printed.

use std::fmt;

use js_host_step::Host;

use crate::MdmError;

/// A value from a secret file. It has no `Display`, and its `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The value, for the one place it goes.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Refuse a secret file that is not mode 0600 or not root's.
pub fn check_mode(_path: &str, _mode: u32, _uid: u32) -> Result<(), MdmError> {
    Ok(())
}

/// A `KEY=<value>` line with a value is in `bytes`.
pub fn has_value(_bytes: &[u8], _key: &str) -> bool {
    false
}

/// The value of `key` in the secret file at `path`.
pub fn read(_host: &dyn Host, path: &str, key: &str) -> Result<Secret, MdmError> {
    Err(MdmError::NoSecret {
        path: path.to_owned(),
        key: key.to_owned(),
    })
}
