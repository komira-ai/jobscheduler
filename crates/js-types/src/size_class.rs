//! The class a job is sized by when it leaves its CPU or memory to the scheduler: the value of
//! its label [`SIZE_CLASS_LABEL`] (`docs/design/sizing.md`, section 7.7). A client names the
//! work a job runs there, so that every run of that work reads and teaches one history.
//!
//! A build farm names the build target an action belongs to. A target's name
//! (`root//crates/js-sched:js-sched`) is not a label value, which is 1 to 63 bytes of
//! `[a-z0-9._-]`, so [`SizeClass::of_target`] writes it as one:
//!
//! - letters are lowered, and every run of bytes outside `[a-z0-9_-]` (`//`, `/`, `:`, `.`,
//!   a space, anything not ASCII) is one `.`; the result starts and ends with a letter or a
//!   digit. `root//crates/js-sched:js-sched` is `root.crates.js-sched.js-sched`, and
//!   `cell//:name` is `cell.name`.
//! - a result longer than 63 bytes keeps its last 46 bytes (the package's end and the
//!   target's own name), from the first whole word in them (a word cut in two is dropped,
//!   unless the 46 bytes are one word), and adds `-` and the 16 hex
//!   digits of the 64-bit FNV-1a hash of the whole result, so two long targets that end alike
//!   are still two classes.
//!
//! Two targets whose names differ only in the case of a letter or in which separators stand
//! between the same words are one class, and share a history. A name with no letter or digit
//! has no class.

use crate::labels::MAX_VALUE_LEN;

/// The label whose value names a job's class (`size-class`): the name of the work, such as
/// the build target it runs. A client that runs different work under one image and command
/// names it, so that each work has its own history; without it, every run of an image and
/// command shares one.
pub const SIZE_CLASS_LABEL: &str = "size-class";

/// The label whose value names a job's fallback class (`size-fallback`): a class, written as a
/// [`SIZE_CLASS_LABEL`] value is, whose history sizes the job while its own class has too few
/// runs, such as the kind of build step (`zig-cc`, `cargo-test`) a target's action is. The
/// fallback class is the class of that name: a job whose `size-class` is `zig-cc` and one whose
/// `size-fallback` is `zig-cc` read and teach one history (`docs/design/sizing.md`, 7.8).
pub const SIZE_FALLBACK_LABEL: &str = "size-fallback";

/// The bytes a long name keeps: a label value's 63, less `-` and 16 hex digits.
const KEPT_LEN: usize = MAX_VALUE_LEN - 17;

/// The 64-bit FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// The 64-bit FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A job's size class, a valid value of the label [`SIZE_CLASS_LABEL`] that starts and ends
/// with a letter or a digit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SizeClass(String);

impl SizeClass {
    /// The class of the build target named `target` (see the module docs); `None` when the
    /// name holds no ASCII letter or digit.
    #[must_use]
    pub fn of_target(target: &str) -> Option<Self> {
        let mut readable = String::with_capacity(target.len());
        for c in target.chars().map(|c| c.to_ascii_lowercase()) {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-') {
                readable.push(c);
            } else if !readable.ends_with('.') {
                readable.push('.');
            }
        }
        let readable = readable.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if readable.is_empty() {
            return None;
        }
        if readable.len() <= MAX_VALUE_LEN {
            return Some(Self(readable.to_owned()));
        }
        let (dropped, mut kept) = readable.split_at(readable.len() - KEPT_LEN);
        if !dropped.ends_with('.')
            && let Some((_, rest)) = kept.split_once('.')
        {
            kept = rest;
        }
        let kept = kept.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        Some(Self(format!("{kept}-{:016x}", fnv1a(readable.as_bytes()))))
    }

    /// The class as the label's value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The 64-bit FNV-1a hash of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::Labels;

    /// The hash a long target's class ends with: of its dotted, lowered form (for the names
    /// these tests use, which hold no byte but letters, digits, `_`, `-`, `/` and `:`).
    fn hash(target: &str) -> u64 {
        let mut dotted = target.to_ascii_lowercase().replace(['/', ':'], ".");
        while dotted.contains("..") {
            dotted = dotted.replace("..", ".");
        }
        fnv1a(dotted.trim_matches('.').as_bytes())
    }

    fn class(target: &str) -> Option<String> {
        SizeClass::of_target(target).map(|class| class.as_str().to_owned())
    }

    /// Asserts that a target's name is written as its words joined by `.`, lowered, with `_`
    /// and `-` kept and the root package's empty segment gone. Catches a separator that is
    /// kept, doubled or dropped, or a letter left upper.
    #[test]
    fn a_target_is_its_words_joined_by_dots() {
        assert_eq!(
            class("root//crates/js-sched:js-sched").as_deref(),
            Some("root.crates.js-sched.js-sched")
        );
        assert_eq!(class("cell//:name").as_deref(), Some("cell.name"));
        assert_eq!(
            class("root//Foo/Bar:My_Test").as_deref(),
            Some("root.foo.bar.my_test")
        );
        assert_eq!(
            class("root//a:b (prelude//platforms:default#0123)").as_deref(),
            Some("root.a.b.prelude.platforms.default.0123")
        );
        assert_eq!(class("//pkg:été").as_deref(), Some("pkg.t"));
        assert_eq!(class("x").as_deref(), Some("x"));
    }

    /// Asserts that the class starts and ends with a letter or a digit whatever surrounds the
    /// name, and that a name with no letter or digit has none. Catches a class that starts with
    /// a separator (no history profile name may) or an empty label value.
    #[test]
    fn the_class_starts_and_ends_with_a_letter_or_digit_and_a_bare_name_has_none() {
        assert_eq!(class("-_//:a_-").as_deref(), Some("a"));
        assert_eq!(class("_a-").as_deref(), Some("a"));
        for none in ["", "//:", "-_-", "...", "日本"] {
            assert_eq!(class(none), None, "{none:?}");
        }
    }

    /// Asserts that a name of 63 bytes is kept whole and one of 64 keeps its last 46 bytes,
    /// `-` and the hash of the whole dotted name: 63 bytes in all; that two long names that
    /// differ only in case or separators are one class; that a cut inside a word starts the
    /// kept part at the next word; and that two long names that end alike are two classes
    /// while one name is always one. Catches a class longer than a label value, a cut at the
    /// wrong end or inside a word, or a hash of the kept part only.
    #[test]
    fn a_long_name_keeps_its_end_and_the_hash_of_the_whole() {
        let fits = format!("a{}", "b".repeat(62));
        assert_eq!(class(&fits), Some(fits.clone()));
        let long = format!("a{}", "b".repeat(63));
        let hashed = class(&long).unwrap();
        assert_eq!(hashed.len(), 63);
        assert_eq!(hashed, format!("{}-{:016x}", "b".repeat(46), hash(&long)));
        let other = format!("c{}", "b".repeat(63));
        assert_ne!(class(&other), Some(hashed.clone()));
        assert_eq!(class(&long), Some(hashed));
        let deep = format!("root//{}:test", "pkg/".repeat(20));
        let upper = format!("ROOT//{}/TEST", "PKG:".repeat(20));
        assert_eq!(class(&upper), class(&deep));
        let kept = class(&deep).unwrap();
        let tail = format!(".test-{:016x}", hash(&deep));
        assert_eq!(
            kept,
            format!("{}{tail}", "pkg.".repeat(10).trim_end_matches('.'))
        );
    }

    /// Asserts that a cut that lands on a separator starts at the next word, one that lands just
    /// after a separator keeps every word after it whole, and a kept part that starts with `-`
    /// starts after it. Catches a long class that starts with `.` or `-`, or a whole word
    /// dropped as if it were cut.
    #[test]
    fn a_cut_at_a_separator_keeps_the_next_word_whole() {
        let on = format!("{}:{}", "a".repeat(30), "b".repeat(45));
        assert_eq!(
            class(&on),
            Some(format!("{}-{:016x}", "b".repeat(45), hash(&on)))
        );
        let after = format!("{}:{}", "a".repeat(29), "b".repeat(46));
        assert_eq!(
            class(&after),
            Some(format!("{}-{:016x}", "b".repeat(46), hash(&after)))
        );
        let whole = format!("{}:{}:{}", "a".repeat(29), "b".repeat(20), "c".repeat(25));
        assert_eq!(
            class(&whole),
            Some(format!(
                "{}.{}-{:016x}",
                "b".repeat(20),
                "c".repeat(25),
                hash(&whole)
            ))
        );
        let dash = format!("{}-{}", "a".repeat(40), "b".repeat(45));
        assert_eq!(
            class(&dash),
            Some(format!("{}-{:016x}", "b".repeat(45), hash(&dash)))
        );
    }

    /// Asserts the hash's value for known input, so that a class name never changes under a
    /// history already kept by it. Catches another hash, offset or prime.
    #[test]
    fn the_hash_is_fnv1a_64() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// Asserts that every class is a value the label accepts. Catches a class a submit refuses.
    #[test]
    fn every_class_is_a_label_value() {
        let targets = [
            "root//crates/js-sched:js-sched".to_owned(),
            format!("root//{}:t", "Deep/".repeat(40)),
            format!("{}é{}", "a".repeat(40), "b".repeat(40)),
            "cell//:name".to_owned(),
        ];
        for target in targets {
            let class = SizeClass::of_target(&target).unwrap();
            let labels = BTreeMap::from([(SIZE_CLASS_LABEL.to_owned(), class.as_str().to_owned())]);
            let labels = Labels::from_caller(&labels).unwrap();
            assert_eq!(labels.get(SIZE_CLASS_LABEL), Some(class.as_str()));
        }
    }
}
