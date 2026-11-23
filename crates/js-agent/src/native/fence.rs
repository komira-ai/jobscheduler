//! The places on a Mac that only the privacy controls (TCC) guard from a native job.
//!
//! A native job keeps the agent's responsibility (it is started without the disclaim), so
//! the privacy controls count it as the agent, which holds Full Disk Access. Unix modes guard
//! every user's home and root's home from the job's account; what they do not guard, and only
//! the privacy controls did, is listed here, measured on the proof Mac with a profile whose
//! reads are open (`docs/design/spawn-disclaim.md`, "The fence, measured"). Each is a read
//! deny of every run's profile, under both spellings, since `/Library` is a firmlink to the
//! data volume and which spelling Seatbelt matches was not measured.
//!
//! The list is fixed for one macOS release: the measurement is run again after every macOS
//! upgrade of the pool, and a new place is added here.

/// The privacy-only places, each spelled under `/Library` and under
/// `/System/Volumes/Data/Library`: the privacy database's directory, the database itself,
/// and Time Machine's preferences. Read-denied to every native run
/// ([`super::NativeSettings::from_flags`]).
pub const PRIVACY_ONLY_PLACES: [&str; 6] = [
    "/Library/Application Support/com.apple.TCC",
    "/Library/Application Support/com.apple.TCC/TCC.db",
    "/Library/Preferences/com.apple.TimeMachine.plist",
    "/System/Volumes/Data/Library/Application Support/com.apple.TCC",
    "/System/Volumes/Data/Library/Application Support/com.apple.TCC/TCC.db",
    "/System/Volumes/Data/Library/Preferences/com.apple.TimeMachine.plist",
];

/// Whether `path` (a canonical absolute path) is one of [`PRIVACY_ONLY_PLACES`] or lies
/// below one: a mount grant there would follow the read denies in the profile and reopen
/// the place, so the start is refused.
#[must_use]
pub fn is_fenced(path: &str) -> bool {
    PRIVACY_ONLY_PLACES.iter().any(|place| {
        path == *place
            || path
                .strip_prefix(place)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::{PRIVACY_ONLY_PLACES, is_fenced};

    /// Asserts the six privacy-only places, spelled out, and that each is fenced while a
    /// path that only begins like one, and its parent directory, are not.
    ///
    /// Catches: a place dropped, added or misspelled in the list every run's profile
    /// read-denies, and a fence test by text prefix.
    #[test]
    fn the_privacy_only_places_are_these_six_paths() {
        assert_eq!(
            PRIVACY_ONLY_PLACES,
            [
                "/Library/Application Support/com.apple.TCC",
                "/Library/Application Support/com.apple.TCC/TCC.db",
                "/Library/Preferences/com.apple.TimeMachine.plist",
                "/System/Volumes/Data/Library/Application Support/com.apple.TCC",
                "/System/Volumes/Data/Library/Application Support/com.apple.TCC/TCC.db",
                "/System/Volumes/Data/Library/Preferences/com.apple.TimeMachine.plist",
            ]
        );
        assert!(is_fenced(
            "/Library/Preferences/com.apple.TimeMachine.plist"
        ));
        assert!(is_fenced(
            "/System/Volumes/Data/Library/Application Support/com.apple.TCC/TCC.db"
        ));
        assert!(!is_fenced(
            "/Library/Preferences/com.apple.TimeMachine.plist2"
        ));
        assert!(!is_fenced("/Library/Preferences"));
    }
}
