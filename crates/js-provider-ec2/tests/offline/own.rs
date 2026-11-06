//! The words of a label that cannot become a tag.

use js_provider_ec2::LabelError;

/// Asserts that each refusal of a label names the key, or the count, and says what the
/// rule is, so an operator can fix the label.
#[test]
fn a_label_refusal_says_what_the_rule_is() {
    assert_eq!(
        LabelError::Key("A".to_owned()).to_string(),
        "label key \"A\" must be lower case letters, digits and '-', starting with a letter, and at most 128 characters as a tag"
    );
    assert_eq!(
        LabelError::Twice("a".to_owned()).to_string(),
        "label key \"a\" is given twice"
    );
    assert_eq!(
        LabelError::Value("a".to_owned()).to_string(),
        "the value of label \"a\" must be at most 256 printable ASCII characters"
    );
    assert_eq!(
        LabelError::TooMany(51).to_string(),
        "51 tags are more than the 50 allowed"
    );
}
