//! Temporary: prints the pinned values of the fixture layer in one failure.

mod layer;

use js_bundle::pack_layer;
use layer::{Scratch, fixture};

#[test]
fn the_fixture_layer_has_these_digests() {
    let scratch = Scratch::new();
    fixture(&scratch.root(), false);
    let packed = pack_layer(&scratch.root()).unwrap();
    let got = (
        packed.diff_id.as_str().to_string(),
        packed.blob_digest.clone(),
        packed.uncompressed_bytes,
        packed.entries,
    );
    let zero = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    assert_eq!(got, (zero.to_string(), zero.to_string(), 0, 0));
}
