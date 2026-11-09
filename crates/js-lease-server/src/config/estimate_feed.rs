//! `--estimate-feed` and `server.estimate_feed`, kept for one release so that a command
//! line or file that turns the feed on still starts: no lease is costed by an estimate any
//! more, so the flag does nothing and leaves a warning. Its refusals (beside `--usage-off`,
//! and without an agent pool) are made where the flag and the key are read.

/// The warning for the flag or key `place` (`--estimate-feed`, or the key
/// `server.estimate_feed`) when it is turned `on`; none when it is off. It names the flag
/// or the key to drop.
pub(crate) fn deprecated(on: bool, place: &str) -> Option<String> {
    let drop = if place.starts_with("--") {
        "flag"
    } else {
        "key"
    };
    on.then(|| {
        format!(
            "{place} is deprecated and does nothing: no lease is costed by an estimate any more (drop the {drop})"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag's warning is given in full, the key's names the key to drop, and an
    /// estimate feed that is off leaves none.
    #[test]
    fn the_warning_names_the_flag_or_the_key_to_drop_in_full() {
        assert_eq!(
            deprecated(true, "--estimate-feed").as_deref(),
            Some(
                "--estimate-feed is deprecated and does nothing: no lease is costed by an estimate any more (drop the flag)"
            )
        );
        assert_eq!(
            deprecated(true, "server.estimate_feed").as_deref(),
            Some(
                "server.estimate_feed is deprecated and does nothing: no lease is costed by an estimate any more (drop the key)"
            )
        );
        assert_eq!(deprecated(false, "--estimate-feed"), None);
    }
}
