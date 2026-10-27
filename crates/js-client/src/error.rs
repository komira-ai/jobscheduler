//! Why a client could not be built or a request got no answer.

/// A failure of the client. The text never holds the token.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A flag, or a file it names, is unusable.
    #[error("{flag}: {why}")]
    Flag {
        /// The flag, with its leading dashes.
        flag: &'static str,
        /// What is wrong with it.
        why: String,
    },
    /// The server could not be reached, or went away before its answer was read.
    #[error("{0}")]
    Server(String),
}

impl ClientError {
    /// A failure naming `flag`.
    pub(crate) fn flag(flag: &'static str, why: impl std::fmt::Display) -> Self {
        Self::Flag {
            flag,
            why: why.to_string(),
        }
    }
}
