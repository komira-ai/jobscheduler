//! What `drain` notes in the hold file after the text for a person ([`HOLD_TEXT`]), for
//! `resume` to read back: whether a cordon was in force before the drain, and the host facts
//! when the drain began. The agent reads nothing in the file.
//!
//! ```text
//! js-agent starts no work on this host and keeps it cordoned while this file is here
//! cordoned_before=false
//! facts=os=linux arch=x86_64 ...
//! ```

use crate::lifecycle::hold::HOLD_TEXT;

/// The line that says whether a cordon was in force before the drain.
const CORDONED_BEFORE: &str = "cordoned_before=";

/// The line of the host facts when the drain began.
const FACTS: &str = "facts=";

/// What `drain` notes in the hold file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Note {
    /// A cordon was in force when the drain began: an operator's, or another holder's.
    /// `resume` leaves it in force.
    pub(super) cordoned_before: bool,
    /// The host facts when the drain began, `None` when they could not be read.
    pub(super) facts: Option<String>,
}

impl Note {
    /// The text of the hold file: [`HOLD_TEXT`], then the note, a line each.
    pub(super) fn text(&self) -> String {
        let mut text = format!("{HOLD_TEXT}{CORDONED_BEFORE}{}\n", self.cordoned_before);
        if let Some(facts) = &self.facts {
            text.push_str(&format!("{FACTS}{facts}\n"));
        }
        text
    }

    /// The note of a hold file's text. A line it does not know is ignored, and a text without
    /// the note (a hold file a person wrote) reads as no cordon before and no facts: `resume`
    /// then lifts the cordon and restarts nothing.
    pub(super) fn parse(text: &str) -> Note {
        Note {
            cordoned_before: value(text, CORDONED_BEFORE) == Some("true"),
            facts: value(text, FACTS).map(str::to_owned),
        }
    }
}

/// The rest of the first line of `text` that starts with `key`.
fn value<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    text.lines().find_map(|line| line.strip_prefix(key))
}
