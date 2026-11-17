//! What a post-job check reports when the host is not known to be clean ([`CheckFinding`]).

/// The most bytes of detail a [`CheckFinding`] keeps.
pub const CHECK_DETAIL_MAX: usize = 256;

/// What a post-job check reports when the host is not known to be clean: a short code the
/// check owns, and a detail cut to [`CHECK_DETAIL_MAX`] bytes so that a check cannot fill
/// the agent's memory or its log with the output of a program. The code becomes the code
/// of the run's [`super::Finding`], so it is one finding for each code of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFinding {
    /// The run left something behind on the host.
    Residue {
        /// What kind, as a short code of the check.
        code: &'static str,
        /// What the check saw.
        detail: String,
    },
    /// The check could not run, so the host is not known to be clean.
    Unchecked {
        /// Which check, as a short code.
        code: &'static str,
        /// Why it could not run.
        detail: String,
    },
}

impl CheckFinding {
    /// A [`CheckFinding::Residue`] of `code`; `detail` is cut to [`CHECK_DETAIL_MAX`]
    /// bytes, at a character boundary.
    pub fn residue(code: &'static str, detail: &str) -> Self {
        Self::Residue {
            code,
            detail: bounded(detail).to_owned(),
        }
    }

    /// A [`CheckFinding::Unchecked`] of `code`; `detail` is cut to [`CHECK_DETAIL_MAX`]
    /// bytes, at a character boundary.
    pub fn unchecked(code: &'static str, detail: &str) -> Self {
        Self::Unchecked {
            code,
            detail: bounded(detail).to_owned(),
        }
    }

    /// The short code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Residue { code, .. } | Self::Unchecked { code, .. } => code,
        }
    }

    /// The detail, at most [`CHECK_DETAIL_MAX`] bytes: cut here too, so a finding built
    /// as a struct literal, which skips the constructors, is bounded where it is read.
    pub fn detail(&self) -> &str {
        match self {
            Self::Residue { detail, .. } | Self::Unchecked { detail, .. } => bounded(detail),
        }
    }
}

/// `text` cut to at most [`CHECK_DETAIL_MAX`] bytes, at a character boundary.
fn bounded(text: &str) -> &str {
    let end = (0..=CHECK_DETAIL_MAX.min(text.len()))
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(0);
    &text[..end]
}
