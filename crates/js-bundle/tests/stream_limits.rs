//! The compressed side of a layer: the zstd limits of the bundle format, and a layer's
//! digest and ceiling checks.

mod layer;

use std::io::Read;
use std::os::fd::AsFd as _;

use js_bundle::media::LayerEncoding;
use js_bundle::{
    Decoder, DiffId, Limits, MAX_WINDOW_BYTES, StreamError, UnpackError, compress, unpack_layer,
};
use layer::{Scratch, Tar, diff_id_of, limits};

fn extract(
    blob: &[u8],
    diff_id: &DiffId,
    limits: &Limits,
) -> Result<js_bundle::Unpacked, UnpackError> {
    let scratch = Scratch::new();
    let fd = scratch.root_fd();
    unpack_layer(blob, LayerEncoding::TarZstd, fd.as_fd(), diff_id, limits)
}

fn small_layer() -> (Vec<u8>, Vec<u8>, DiffId) {
    let tar = Tar::new().dir("d").file("d/f", b"hello hello hello").end();
    let blob = compress(&tar);
    let id = diff_id_of(&tar);
    (tar, blob, id)
}

fn stream_error(result: Result<js_bundle::Unpacked, UnpackError>) -> StreamError {
    match result {
        Err(UnpackError::Stream(error)) => error,
        other => panic!("not a stream error: {other:?}"),
    }
}

/// A frame header: magic, descriptor, and the bytes that follow it.
fn head(descriptor: u8, rest: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, descriptor];
    out.extend_from_slice(rest);
    out
}

fn decoder_error(bytes: &[u8]) -> StreamError {
    let mut source = bytes;
    match Decoder::new(&mut source) {
        Err(error) => error,
        Ok(_) => panic!("accepted {bytes:?}"),
    }
}

fn decoder_accepts(bytes: &[u8]) {
    let mut source = bytes;
    assert!(Decoder::new(&mut source).is_ok(), "refused {bytes:?}");
}

/// A compressed layer extracts and its raw stream hashes to the diff id.
#[test]
fn a_zstd_layer_extracts_and_its_diff_id_is_checked() {
    let (tar, blob, id) = small_layer();
    let done = extract(&blob, &id, &limits()).unwrap();
    assert_eq!(done.entries, 2);
    assert_eq!(done.stream_bytes, tar.len() as u64);
    assert_eq!(done.diff_id, id);
}

/// A stream that hashes to another diff id than the config's is refused.
#[test]
fn a_stream_that_hashes_to_another_diff_id_is_refused() {
    let (_, blob, id) = small_layer();
    let other = Tar::new().dir("d").end();
    match extract(&blob, &diff_id_of(&other), &limits()) {
        Err(UnpackError::DiffId { expected, got }) => {
            assert_eq!(expected, diff_id_of(&other).as_str());
            assert_eq!(got, id.as_str());
        }
        other => panic!("{other:?}"),
    }
}

/// A decompression bomb: a small blob that expands past the raw ceiling is stopped at the
/// ceiling, not extracted.
#[test]
fn a_decompression_bomb_is_stopped_at_the_ceiling() {
    let mut tar = Tar::new().dir("d").body();
    tar.resize(tar.len() + (16 << 20), 0);
    let blob = compress(&tar);
    assert!(
        blob.len() * 100 < tar.len(),
        "the bomb should compress at least 100 to 1"
    );
    let tight = Limits {
        entries: 10,
        bytes: 64 << 10,
        model_bytes: 1 << 28,
    };
    let error = stream_error(extract(&blob, &diff_id_of(&tar), &tight));
    assert_eq!(
        error,
        StreamError::Ceiling {
            max: (64 << 10) + 1024
        }
    );
}

/// The same bytes stored uncompressed are held to the same ceiling.
#[test]
fn an_uncompressed_layer_has_the_same_ceiling() {
    let mut tar = Tar::new().dir("d").body();
    tar.resize(tar.len() + 100_000, 0);
    let scratch = Scratch::new();
    let fd = scratch.root_fd();
    let tight = Limits {
        entries: 10,
        bytes: 64 << 10,
        model_bytes: 1 << 28,
    };
    let result = unpack_layer(
        &tar[..],
        LayerEncoding::Tar,
        fd.as_fd(),
        &diff_id_of(&tar),
        &tight,
    );
    assert!(matches!(stream_error(result), StreamError::Ceiling { .. }));
}

/// A layer is one frame: a second frame, a skippable frame and a stray byte after it are refused.
#[test]
fn bytes_after_the_one_frame_are_refused() {
    let (_, blob, id) = small_layer();
    let mut two = blob.clone();
    two.extend_from_slice(&blob);
    let mut skippable = blob.clone();
    skippable.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]);
    let mut stray = blob;
    stray.push(0);
    for bytes in [two, skippable, stray] {
        assert_eq!(
            stream_error(extract(&bytes, &id, &limits())),
            StreamError::TrailingBytes
        );
    }
}

/// A layer cut anywhere is truncated, never half extracted as a success.
#[test]
fn a_truncated_zstd_layer_is_refused_wherever_it_is_cut() {
    let (_, blob, id) = small_layer();
    for cut in 0..blob.len() {
        let error = stream_error(extract(&blob[..cut], &id, &limits()));
        assert_eq!(
            error,
            StreamError::Truncated,
            "cut at {cut} of {}",
            blob.len()
        );
    }
}

/// A blob that does not start with the zstd magic is refused.
#[test]
fn a_blob_with_a_damaged_magic_is_refused() {
    let (_, mut blob, id) = small_layer();
    blob[0] ^= 0x55;
    assert_eq!(
        stream_error(extract(&blob, &id, &limits())),
        StreamError::NotZstd
    );
}

/// A block header of the reserved type is a malformed frame, not a truncated one.
#[test]
fn a_malformed_frame_is_corrupt() {
    let mut bytes = head(0x00, &[0x68]);
    bytes.extend_from_slice(&[0x07, 0, 0, 0, 0, 0, 0, 0]);
    let mut source = &bytes[..];
    let mut decoder = Decoder::new(&mut source).unwrap();
    let error = decoder.read(&mut [0u8; 16]).unwrap_err();
    let inner = error
        .get_ref()
        .unwrap()
        .downcast_ref::<StreamError>()
        .unwrap();
    assert!(matches!(inner, StreamError::Corrupt(_)), "{inner:?}");
}

/// A read of no bytes is not the end of the frame.
#[test]
fn a_read_into_an_empty_buffer_reads_nothing() {
    let (_, blob, _) = small_layer();
    let mut source = &blob[..];
    let mut decoder = Decoder::new(&mut source).unwrap();
    assert_eq!(decoder.read(&mut []).unwrap(), 0);
    let mut all = Vec::new();
    decoder.read_to_end(&mut all).unwrap();
    assert!(!all.is_empty());
}

/// The window limit is 8 MiB: at it passes, over it is refused, whether the frame says it in
/// a window descriptor or, being single-segment, in its content size.
#[test]
fn the_zstd_window_is_capped_at_8_mib() {
    assert_eq!(MAX_WINDOW_BYTES, 8 << 20);
    // Window descriptor: exponent 13 is 8 MiB; mantissa 1 adds an eighth.
    decoder_accepts(&head(0x00, &[13 << 3]));
    assert_eq!(
        decoder_error(&head(0x00, &[(13 << 3) | 1])),
        StreamError::Window {
            size: (8 << 20) + (1 << 20),
            max: 8 << 20
        }
    );
    // Single segment: the content size is the window. One byte (flag 0), two bytes
    // (flag 1, plus 256), four (flag 2) and eight (flag 3).
    decoder_accepts(&head(0x20, &[200]));
    decoder_accepts(&head(0x60, &[0, 0]));
    decoder_accepts(&head(0xA0, &(1u32 << 20).to_le_bytes()));
    assert_eq!(
        decoder_error(&head(0xA0, &(9u32 << 20).to_le_bytes())),
        StreamError::Window {
            size: 9 << 20,
            max: 8 << 20
        }
    );
    assert_eq!(
        decoder_error(&head(0xE0, &(1u64 << 40).to_le_bytes())),
        StreamError::Window {
            size: 1 << 40,
            max: 8 << 20
        }
    );
}

/// A dictionary id of any width, a reserved bit, a skippable frame, another format and
/// input too short for a header are each refused.
#[test]
fn frames_with_a_dictionary_or_another_shape_are_refused() {
    for flag in 1..=3u8 {
        assert_eq!(
            decoder_error(&head(flag, &[0x68, 1, 1, 1, 1])),
            StreamError::Dictionary
        );
    }
    assert!(matches!(
        decoder_error(&head(0x08, &[0x68])),
        StreamError::Corrupt(_)
    ));
    for magic in [0x184D_2A50u32, 0x184D_2A5F] {
        let mut bytes = magic.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(decoder_error(&bytes), StreamError::SkippableFrame);
    }
    assert_eq!(decoder_error(b"PK\x03\x04\0"), StreamError::NotZstd);
    assert_eq!(decoder_error(b"\x1f\x8b\x08\0\0"), StreamError::NotZstd);
    assert_eq!(decoder_error(b""), StreamError::Truncated);
    assert_eq!(decoder_error(&[0x28, 0xB5, 0x2F]), StreamError::Truncated);
    assert_eq!(decoder_error(&head(0x00, &[])), StreamError::Truncated);
}

/// A source that fails is reported as a read failure.
#[test]
fn a_source_that_fails_is_a_read_error() {
    struct Failing;
    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no disk"))
        }
    }
    let mut source = Failing;
    match Decoder::new(&mut source) {
        Err(StreamError::Read(why)) => assert!(why.contains("no disk"), "{why}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("accepted"),
    }
}

/// Compression is a function of the bytes alone.
#[test]
fn compression_is_deterministic() {
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(compress(&data), compress(&data));
    assert_ne!(compress(&data), compress(&data[1..]));
}
