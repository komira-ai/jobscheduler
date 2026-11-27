//! What a post-job check reports when the host is not known to be clean ([`CheckFinding`]),
//! what it reports of a change that does not taint the host ([`CheckNote`]), and what a
//! check asked as a step of the worker answers ([`Checked`]).

use std::time::Duration;

use super::{Event, Finding};

/// What [`CheckHook::poll`](super::CheckHook::poll) answers: the verdict, or how long the
/// checks wait before they look at the host again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// The verdict, as [`CheckHook::run`](super::CheckHook::run) gives it.
    Done(Result<(), CheckFinding>),
    /// The checks look again once this long has passed: the lifecycle asks again for the
    /// same run no sooner (on its clock), and the job waits in its cleanup with its slot
    /// booked meanwhile.
    Wait(Duration),
}

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

/// What a post-job check saw change on the host that does not taint it: the host's own
/// doing, which no contained job can cause. Logged as one line, never sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckNote {
    /// The check's code (one of `crate::integrity::code`).
    pub code: &'static str,
    /// What changed, cut to [`CHECK_DETAIL_MAX`] bytes at a character boundary.
    pub detail: String,
}

impl CheckNote {
    /// A note of `code`; `detail` is cut to [`CHECK_DETAIL_MAX`] bytes, at a character
    /// boundary.
    pub fn new(code: &'static str, detail: &str) -> Self {
        Self {
            code,
            detail: bounded(detail).to_owned(),
        }
    }

    /// The finding event that logs a note of a check that belongs to no run (a taint's
    /// re-check, the check of a whole-node lease): the note's code and detail. It taints
    /// nothing. A note after a job's checks is the job's finding instead (`Job::note`).
    pub(super) fn event(self) -> Event {
        Event::Finding(Finding {
            run_id: None,
            code: self.code,
            message: self.detail,
        })
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
