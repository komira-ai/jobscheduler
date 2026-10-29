//! Golden vectors: the media types and the config bytes. A renamed field, a reordered
//! struct or a changed media type changes these texts.

mod common;

use common::{MINIMAL, TWO_TREE, TWO_TREE_REORDERED, edited, golden, minimal, set};
use js_bundle::media::{self, LayerEncoding};
use js_bundle::{BundleConfig, ReadGrantKind};
use serde_json::json;

/// The media types are on the wire; each value is pinned.
#[test]
fn media_types_are_pinned() {
    assert_eq!(media::ARTIFACT_TYPE, "application/vnd.komira.js.bundle.v1");
    assert_eq!(
        media::CONFIG,
        "application/vnd.komira.js.bundle.config.v1+json"
    );
    assert_eq!(
        media::LAYER_TAR_ZSTD,
        "application/vnd.komira.js.bundle.layer.v1.tar+zstd"
    );
    assert_eq!(
        media::LAYER_TAR,
        "application/vnd.komira.js.bundle.layer.v1.tar"
    );
    assert_eq!(
        media::MANIFEST,
        "application/vnd.oci.image.manifest.v1+json"
    );
}

/// A layer type maps to its encoding and back; the generic OCI layer types are not a
/// bundle's, since a generic tool might unpack them as a root file system.
#[test]
fn layer_encodings_round_trip_and_refuse_generic_oci_layers() {
    for encoding in [LayerEncoding::TarZstd, LayerEncoding::Tar] {
        assert_eq!(
            LayerEncoding::from_media_type(encoding.media_type()),
            Some(encoding)
        );
    }
    for other in [
        "application/vnd.oci.image.layer.v1.tar+zstd",
        "application/vnd.oci.image.layer.v1.tar",
        "",
    ] {
        assert_eq!(LayerEncoding::from_media_type(other), None);
    }
}

/// The canonical bytes of the two-tree config are exactly the fixture.
#[test]
fn two_tree_config_writes_the_golden_bytes() {
    assert_eq!(golden().to_canonical_json(), TWO_TREE.as_bytes());
}

/// A config with another key order, whitespace and `14.0.0` for `14.0` reads equal to
/// the golden and writes the golden bytes.
#[test]
fn reordered_spelling_has_the_same_canonical_bytes() {
    let config = BundleConfig::parse(TWO_TREE_REORDERED.as_bytes()).unwrap();
    assert_eq!(config, golden());
    assert_eq!(config.to_canonical_json(), TWO_TREE.as_bytes());
}

/// The config without an Xcode need omits the key; empty lists and maps stay.
#[test]
fn minimal_config_writes_the_golden_bytes() {
    let config = minimal();
    assert!(config.xcode.is_none());
    assert_eq!(config.to_canonical_json(), MINIMAL.as_bytes());
}

/// The read vocabulary is closed and spelled as the format says.
#[test]
fn read_grant_kinds_are_spelled_as_the_format_says() {
    let names: Vec<&str> = ReadGrantKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "system-certs",
            "timezone",
            "resolver-config",
            "sysctl-info",
            "locale"
        ]
    );
    for kind in ReadGrantKind::ALL {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{}\"", kind.as_str()));
        let back: ReadGrantKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }
}

/// Two spellings of the same set give the same bytes: `writable_trees` and
/// `xcode.components` are sets and are written sorted.
#[test]
fn set_lists_are_written_sorted() {
    let spelled = |trees: [&str; 2], components: [&str; 2]| {
        edited(|v| {
            set(v, "/writable_trees", json!(trees));
            set(v, "/xcode/components", json!(components));
        })
        .unwrap()
    };
    let one = spelled(["job", "zig"], ["clang", "swift"]).to_canonical_json();
    let two = spelled(["zig", "job"], ["swift", "clang"]).to_canonical_json();
    assert_eq!(one, two);
    let text = String::from_utf8(one).unwrap();
    assert!(
        text.contains("\"writable_trees\":[\"job\",\"zig\"]"),
        "{text}"
    );
    assert!(
        text.contains("\"components\":[\"clang\",\"swift\"]"),
        "{text}"
    );
}

/// `seatbelt.reads` is an ordered list (the profile builder reads it in order), so its
/// order is part of the bytes and of what materialise hands on.
#[test]
fn read_grant_order_is_significant() {
    let spelled = |reads: [&str; 2]| edited(|v| set(v, "/seatbelt/reads", json!(reads))).unwrap();
    let one = spelled(["timezone", "locale"]);
    let two = spelled(["locale", "timezone"]);
    assert_ne!(one.to_canonical_json(), two.to_canonical_json());
    assert_eq!(
        one.seatbelt.reads,
        [ReadGrantKind::Timezone, ReadGrantKind::Locale]
    );
}
