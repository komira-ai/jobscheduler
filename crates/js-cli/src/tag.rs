//! The label with which `lease run` marks its start, so that a lease whose answer was
//! lost can be found again in the list.

use crate::api::Entry;
use crate::failure::Failure;

/// The label key. It is not under the prefix the server reserves for itself.
pub(crate) const KEY: &str = "js-run";

/// The random bytes of a tag: 128 bits, as 32 characters of hexadecimal.
const BYTES: usize = 16;

/// A value no other start carries: 128 random bits in lower-case hexadecimal (a label
/// value the server accepts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunTag(String);

impl RunTag {
    /// A new tag from the system's random source.
    pub(crate) fn fresh() -> Result<Self, Failure> {
        Self::from_draw(draw())
    }

    /// The tag of these random bytes, or the failure of the draw.
    fn from_draw(drawn: Result<[u8; BYTES], getrandom::Error>) -> Result<Self, Failure> {
        drawn
            .map(|bytes| Self(bytes.iter().map(|byte| format!("{byte:02x}")).collect()))
            .map_err(|why| Failure::Server(format!("cannot draw a random run label: {why}")))
    }

    /// The label value.
    pub(crate) fn value(&self) -> &str {
        &self.0
    }

    /// Whether the listed lease carries this tag.
    pub(crate) fn marks(&self, entry: &Entry) -> bool {
        entry.labels.get(KEY).is_some_and(|value| *value == self.0)
    }
}

fn draw() -> Result<[u8; BYTES], getrandom::Error> {
    let mut bytes = [0u8; BYTES];
    getrandom::getrandom(&mut bytes).map(|()| bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn entry(labels: &[(&str, &str)]) -> Entry {
        Entry {
            id: "1-1".to_owned(),
            state: "held".to_owned(),
            host: None,
            expires_at_ms: 1,
            labels: labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// Asserts that a tag is 32 lower-case hexadecimal characters, that two tags differ,
    /// and that a tag marks exactly the leases that carry its value under its key.
    #[test]
    fn a_tag_is_random_hex_and_marks_its_own_lease() {
        let (first, second) = (RunTag::fresh().unwrap(), RunTag::fresh().unwrap());
        let value = first.value();
        assert_eq!(value.len(), 32);
        assert!(
            value
                .bytes()
                .all(|b| b.is_ascii_digit() | (b'a'..=b'f').contains(&b))
        );
        assert_ne!(first, second);
        assert!(first.marks(&entry(&[(KEY, value)])));
        assert!(!first.marks(&entry(&[(KEY, second.value())])));
        assert!(!first.marks(&entry(&[("other", value)])));
        assert!(!first.marks(&entry(&[])));
    }

    /// Asserts that many tags drawn from the system source are all different and all in
    /// the label format (32 lower-case hexadecimal characters), so no tag is reused
    /// between starts.
    #[test]
    fn many_tags_are_distinct() {
        let tags: std::collections::BTreeSet<String> = (0..64)
            .map(|_| RunTag::fresh().unwrap().value().to_owned())
            .collect();
        assert_eq!(tags.len(), 64);
        for tag in &tags {
            assert_eq!(tag.len(), 32);
            assert!(
                tag.bytes()
                    .all(|b| b.is_ascii_digit() | (b'a'..=b'f').contains(&b))
            );
        }
    }

    /// Asserts that a random source that fails is a failure naming it, not a tag of
    /// zeros.
    #[test]
    fn a_failed_draw_is_a_failure() {
        let failed = RunTag::from_draw(Err(getrandom::Error::UNEXPECTED));
        let message = failed.unwrap_err().to_string();
        assert!(
            message.starts_with("cannot draw a random run label: "),
            "{message}"
        );
    }

    /// Asserts that the bytes of a draw are written as two hexadecimal digits each.
    #[test]
    fn the_bytes_are_written_in_hex() {
        let tag = RunTag::from_draw(Ok([0, 1, 15, 16, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 171]));
        assert_eq!(tag.unwrap().value(), "00010f10ff00000000000000000000ab");
    }
}
