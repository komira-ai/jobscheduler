//! The names a mint is made of, each checked once: the bucket, the log prefix and the role.
//!
//! The bucket and the prefix go into the session policy's resource pattern, so neither may
//! hold a character that would widen it (`*`, `?`, `$`, `{`) or break out of the JSON string
//! (`"`, `\`, a control character): the bucket is `[a-z0-9.-]` and the prefix is held to the
//! object layout's rules (`js_logstore::layout`), which allow only `[A-Za-z0-9._-]` and `/`.

use js_logstore::{JobPrefix, LayoutError};

/// The longest role ARN, in bytes.
pub const MAX_ROLE_ARN_LEN: usize = 2048;

/// Why a name was refused. It never repeats the name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The bucket is not 3 to 63 bytes of `[a-z0-9.-]`.
    #[error("the bucket must be 3 to 63 bytes of [a-z0-9.-]")]
    Bucket,
    /// The log prefix breaks the object layout's rules.
    #[error("{0}")]
    Prefix(LayoutError),
    /// The role ARN does not begin with `arn:`, is longer than [`MAX_ROLE_ARN_LEN`] bytes, or
    /// holds a byte that is not printable ASCII.
    #[error("the role must be an arn: value of printable ASCII, at most {MAX_ROLE_ARN_LEN} bytes")]
    RoleArn,
}

/// The bucket of the log store: 3 to 63 bytes of `[a-z0-9.-]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket(String);

impl Bucket {
    /// The bucket `name`.
    ///
    /// # Errors
    ///
    /// [`NameError::Bucket`] when `name` is not 3 to 63 bytes of `[a-z0-9.-]`.
    pub fn new(name: &str) -> Result<Self, NameError> {
        let valid = (3..=63).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        match valid {
            true => Ok(Self(name.to_owned())),
            false => Err(NameError::Bucket),
        }
    }

    /// The bucket's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The configured log prefix `L`, with its trailing `/` (empty when there is none): every
/// job's log objects have keys that begin with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPrefix(String);

impl LogPrefix {
    /// The log prefix configured as `prefix` (the path of the `url`, without a leading or
    /// trailing `/`; empty for none), held to the object layout's rules.
    ///
    /// # Errors
    ///
    /// [`NameError::Prefix`] with the layout's reason when `prefix` breaks its rules.
    pub fn new(prefix: &str) -> Result<Self, NameError> {
        // The layout checks a prefix when it builds a job's prefix under it; asking it for one
        // with a valid job id applies exactly its rules to `prefix`, so the two never differ.
        JobPrefix::new(prefix, "job").map_err(NameError::Prefix)?;
        match prefix.is_empty() {
            true => Ok(Self(String::new())),
            false => Ok(Self(format!("{prefix}/"))),
        }
    }

    /// `L`: the prefix with its trailing `/`, or empty.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The role `AssumeRole` assumes: an `arn:` value of printable ASCII, at most
/// [`MAX_ROLE_ARN_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleArn(String);

impl RoleArn {
    /// The role `arn`.
    ///
    /// # Errors
    ///
    /// [`NameError::RoleArn`] when `arn` breaks the rule above.
    pub fn new(arn: &str) -> Result<Self, NameError> {
        let valid = arn.len() <= MAX_ROLE_ARN_LEN
            && arn.starts_with("arn:")
            && arn.bytes().all(|b| b.is_ascii_graphic());
        match valid {
            true => Ok(Self(arn.to_owned())),
            false => Err(NameError::RoleArn),
        }
    }

    /// The ARN.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
