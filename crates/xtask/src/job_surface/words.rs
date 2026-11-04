//! The spellings the generator turns descriptor names into: words of an UpperCamel name,
//! snake case, kebab case, and the short name of an enum value.

/// The words of an UpperCamel name: `SubmitJob` is `Submit`, `Job`; `GetHTTPLog` is `Get`,
/// `HTTP`, `Log`. A word starts at an upper-case letter that follows a lower-case letter or a
/// digit, or that follows an upper-case letter and is followed by a lower-case one.
pub fn camel_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words: Vec<String> = Vec::new();
    for (at, ch) in chars.iter().enumerate() {
        let before = at.checked_sub(1).and_then(|before| chars.get(before));
        let after = chars.get(at + 1);
        let after_lower = before.is_some_and(|b| b.is_lowercase() | b.is_ascii_digit());
        let acronym_end =
            before.is_some_and(|b| b.is_uppercase()) & after.is_some_and(|a| a.is_lowercase());
        let starts = ch.is_uppercase() & (after_lower | acronym_end);
        match (starts, words.last_mut()) {
            (false, Some(word)) => word.push(*ch),
            _ => words.push(ch.to_string()),
        }
    }
    words
}

/// The snake-case spelling of an UpperCamel name, as tonic names a method: `SubmitJob` is
/// `submit_job`.
pub fn snake(name: &str) -> String {
    lower_words(&camel_words(name)).join("_")
}

/// The words in lower case.
pub fn lower_words(words: &[String]) -> Vec<String> {
    words.iter().map(|word| word.to_lowercase()).collect()
}

/// The kebab-case spelling of a snake-case field name: `cpu_millis` is `cpu-millis`.
pub fn kebab(snake: &str) -> String {
    snake.replace('_', "-")
}

/// A noun without one trailing `s`: `jobs` is `job`; `s` alone is kept.
pub fn singular(noun: &str) -> &str {
    noun.strip_suffix('s')
        .filter(|stem| !stem.is_empty())
        .unwrap_or(noun)
}

/// The short names of an enum's values: each name without the prefix all the names share
/// (cut after its last `_`), in lower case with dashes. `JOB_STATE_QUEUED` among the values
/// of `JobState` is `queued`, `NETWORK_LEVEL_ALLOW_LIST` is `allow-list`.
pub fn short_names(names: &[&str]) -> Vec<String> {
    let prefix = names
        .iter()
        .map(|name| (*name).to_owned())
        .reduce(|shared, name| common_prefix(&shared, &name))
        .unwrap_or_default();
    let cut = prefix.rfind('_').map_or(0, |at| at + 1);
    names
        .iter()
        .map(|name| {
            name.get(cut..)
                .unwrap_or(name)
                .to_lowercase()
                .replace('_', "-")
        })
        .collect()
}

/// The longest prefix `a` and `b` share.
fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the words of names with a plain word, an acronym, a digit and one letter.
    /// It catches an acronym split letter by letter and a word glued to the next.
    #[test]
    fn camel_names_split_into_words() {
        assert_eq!(camel_words("SubmitJob"), ["Submit", "Job"]);
        assert_eq!(camel_words("GetHTTPLog"), ["Get", "HTTP", "Log"]);
        assert_eq!(camel_words("ListV2Jobs"), ["List", "V2", "Jobs"]);
        assert_eq!(camel_words("X"), ["X"]);
        assert_eq!(snake("UncordonHost"), "uncordon_host");
        assert_eq!(kebab("cpu_millis"), "cpu-millis");
    }

    /// Asserts that one trailing `s` is dropped, and that `s` alone and a word without one
    /// are kept.
    #[test]
    fn a_plural_noun_loses_one_s() {
        assert_eq!(singular("jobs"), "job");
        assert_eq!(singular("usage"), "usage");
        assert_eq!(singular("s"), "s");
    }

    /// Asserts the short names of enum values: the shared prefix is cut at its last `_`, so
    /// a value whose own word starts like another's keeps it; and the names of an enum with
    /// one value, or none.
    #[test]
    fn enum_values_lose_their_shared_prefix() {
        let names = [
            "JOB_STATE_UNSPECIFIED",
            "JOB_STATE_QUEUED",
            "JOB_STATE_FAILED_TO_START",
        ];
        assert_eq!(
            short_names(&names),
            ["unspecified", "queued", "failed-to-start"]
        );
        let close = ["SHADE_DARK", "SHADE_DARKER"];
        assert_eq!(short_names(&close), ["dark", "darker"]);
        assert_eq!(short_names(&["ONLY_ONE"]), ["one"]);
        assert!(short_names(&[]).is_empty());
    }
}
