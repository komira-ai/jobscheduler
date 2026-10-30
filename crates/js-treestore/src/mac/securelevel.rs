//! The securelevel start check.

use std::io;

/// The securelevel check failed.
#[derive(Debug, thiserror::Error)]
pub enum SecurelevelError {
    /// The kernel runs above securelevel 0, so clearing `uchg` is refused.
    #[error("kern.securelevel is {0}, above 0: immutable flags cannot be cleared")]
    TooHigh(i64),
    /// The level could not be read.
    #[error("cannot read kern.securelevel: {0}")]
    Read(io::Error),
}

/// Refuses a kernel whose securelevel is above 0.
///
/// # Errors
/// The level is above 0 or `read` failed.
pub fn check_securelevel(read: impl FnOnce() -> io::Result<i64>) -> Result<(), SecurelevelError> {
    let level = read().map_err(SecurelevelError::Read)?;
    if level > 0 {
        return Err(SecurelevelError::TooHigh(level));
    }
    Ok(())
}
