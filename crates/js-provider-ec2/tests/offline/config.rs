//! The settings' public parts.

use js_provider_ec2::{ConfigError, ImageId, Images};

fn image(text: &str) -> ImageId {
    ImageId::new(text).unwrap()
}

/// Asserts that images must name at least one architecture, and that an image is found by
/// the architecture an offer spells (`arm64` or `x86_64`) and by no other text; an
/// architecture with no image has none.
#[test]
fn images_are_found_by_the_architecture_an_offer_spells() {
    assert_eq!(Images::new(None, None), Err(ConfigError::NoImage));
    assert_eq!(
        ConfigError::NoImage.to_string(),
        "images: must name an image for arm64 or x86_64"
    );
    let both = Images::new(Some(image("ami-01234567")), Some(image("ami-89abcdef"))).unwrap();
    assert_eq!(both.get("arm64"), Some(&image("ami-01234567")));
    assert_eq!(both.get("x86_64"), Some(&image("ami-89abcdef")));
    assert_eq!(both.get("aarch64"), None);
    let arm_only = Images::new(Some(image("ami-01234567")), None).unwrap();
    assert_eq!(arm_only.get("x86_64"), None);
    let x86_only = Images::new(None, Some(image("ami-89abcdef"))).unwrap();
    assert_eq!(x86_only.get("arm64"), None);
}
