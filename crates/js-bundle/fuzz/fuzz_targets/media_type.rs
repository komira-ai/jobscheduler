//! Any text as a layer media type: only the two layer types of the format are accepted.

#![no_main]

use js_bundle::media::{self, LayerEncoding};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    if LayerEncoding::from_media_type(text).is_some() {
        assert!(text == media::LAYER_TAR || text == media::LAYER_TAR_ZSTD);
    }
});
