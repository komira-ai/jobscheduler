//! `--placement-curves` and `server.placement_curves`: whether the placement engine learns
//! a CPU and a memory profile for each job class from the series its runs recorded. `off`
//! learns nothing; `shadow` learns and logs one line per run it checks. Placement is the
//! same in both. The value is read by [`js_sched_placer::sizes::Curves::parse`]; this is
//! where its refusal is worded, for the flag and the key alike.

use js_sched_placer::sizes::Curves;

/// `text` as the mode it names, or the refusal naming the two values that are accepted.
pub(crate) fn parse(text: &str) -> Result<Curves, String> {
    Curves::parse(text)
        .ok_or_else(|| format!("placement_curves must be off or shadow; got {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `off` and `shadow` are read as the mode they name, and that `on`, an
    /// upper-case spelling, a padded value and an empty one are refused with the whole
    /// message, which names both accepted values and quotes the value given. Catches a
    /// parse that takes `on`, ignores case or trims, and a refusal that does not name the
    /// accepted values.
    #[test]
    fn the_two_values_are_read_and_any_other_is_refused_in_full() {
        assert_eq!(parse("off"), Ok(Curves::Off));
        assert_eq!(parse("shadow"), Ok(Curves::Shadow));
        for bad in ["on", "Shadow", " off", ""] {
            assert_eq!(
                parse(bad),
                Err(format!(
                    "placement_curves must be off or shadow; got {bad:?}"
                )),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse("on"),
            Err("placement_curves must be off or shadow; got \"on\"".to_owned())
        );
    }
}
