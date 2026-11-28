//! Whether a raw profile is cut short: a file that holds less than the profile its own header
//! describes, as a process killed while it wrote its profile at exit leaves it. LLVM's
//! profile runtime writes a raw profile in one pass, the header first, and `llvm-profdata`
//! refuses a file that ends before the header's sections do ("file header is corrupt"),
//! which fails the merge of every other profile with it.
//!
//! The header this reads is that of LLVM 23's raw profiles (`INSTR_PROF_RAW_VERSION` 11, the
//! 64-bit magic, no variant flags): nineteen little-endian `u64`s (`INSTR_PROF_RAW_HEADER`,
//! in order: magic, version, binary ids size, data records, padding before the counters,
//! counters, padding after them, bitmap bytes, padding after them, uniform counters,
//! padding after them, uniform counters delta, names size, counters delta, bitmap delta,
//! names delta, virtual tables, virtual table names size, last value kind), then the binary
//! ids, the data records (72 bytes each), the counters (8 bytes each), the bitmap, the
//! names (padded to 8 bytes), each section followed by the padding its header gives, and
//! value data, which a coverage build does not write. The sections and
//! paddings it adds up are those `llvm-profdata` (`RawInstrProfReader::readHeader`) adds up.
//! A file whose header is of another version, another magic, or has sections this does not
//! size (uniform counters, virtual tables) is not judged here: it is left to
//! `llvm-profdata`, which names it if it refuses it.

/// The first eight bytes of a 64-bit raw profile, as a little-endian `u64`.
const MAGIC: u64 = 0xff6c_7072_6f66_7281;

/// The raw version read here, with no variant flag set.
const VERSION: u64 = 11;

/// The bytes of the header: nineteen `u64`s.
pub(super) const HEADER: usize = 152;

/// The bytes of one data record (`__llvm_profile_data`) of version 11.
const RECORD: u64 = 72;

/// The bytes of one counter.
const COUNTER: u64 = 8;

/// Why a raw profile of `size` bytes whose first bytes are `head` (its first [`HEADER`]
/// bytes, or all of it when it is shorter) is cut short, or `None` when it is not, or is not
/// a profile this can size.
pub(super) fn cut_short(head: &[u8], size: u64) -> Option<String> {
    if size == 0 {
        return Some("empty".to_owned());
    }
    if size < HEADER as u64 {
        // A file that begins as a profile but ends inside its header; one that does not
        // begin as one is not a profile cut short.
        let begun = head.len().min(8);
        let magic = MAGIC.to_le_bytes();
        let ours = head.get(..begun) == magic.get(..begun);
        return ours.then(|| format!("{size} bytes, shorter than the {HEADER} of a header"));
    }
    let words: Vec<u64> = head
        .chunks_exact(8)
        .filter_map(|word| word.first_chunk::<8>())
        .map(|word| u64::from_le_bytes(*word))
        .collect();
    let [
        magic,
        version,
        ids,
        records,
        before_counters,
        counters,
        after_counters,
        bitmap,
        after_bitmap,
        uniform,
        after_uniform,
        _,
        names,
        _,
        _,
        _,
        vtables,
        vnames,
        _,
    ] = words[..]
    else {
        return None;
    };
    if (magic, version, uniform, vtables, vnames) != (MAGIC, VERSION, 0, 0, 0) {
        return None;
    }
    let sections = [
        ids,
        records.checked_mul(RECORD)?,
        before_counters,
        counters.checked_mul(COUNTER)?,
        after_counters,
        bitmap,
        after_bitmap,
        after_uniform,
        names.checked_next_multiple_of(8)?,
    ];
    let needs = sections
        .iter()
        .try_fold(HEADER as u64, |sum, bytes| sum.checked_add(*bytes))?;
    (size < needs).then(|| format!("{size} of the {needs} bytes its header gives"))
}

#[cfg(test)]
mod tests {
    use super::{HEADER, MAGIC, VERSION, cut_short};

    /// A header of version 11 with `ids` bytes of binary ids, `records` data records,
    /// `counters` counters and `names` bytes of names; `set` changes word `i` to `v` first.
    fn header(ids: u64, records: u64, counters: u64, names: u64, set: &[(usize, u64)]) -> Vec<u8> {
        let mut words = [0u64; 19];
        words[0] = MAGIC;
        words[1] = VERSION;
        words[2] = ids;
        words[3] = records;
        words[5] = counters;
        words[12] = names;
        words[18] = 2;
        for (i, v) in set {
            words[*i] = *v;
        }
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// The bytes of the profile [`header`] describes with no change: 152 + 32 + 2 * 72 +
    /// 3 * 8 + 13 padded to 16.
    const WHOLE: u64 = 152 + 32 + 144 + 24 + 16;

    /// Asserts that a profile holding every byte its header gives, or more (value data), is
    /// not cut short, and that one a byte short of it is, with both sizes named.
    #[test]
    fn a_profile_shorter_than_its_header_gives_is_cut_short() {
        let head = header(32, 2, 3, 13, &[]);
        assert_eq!(cut_short(&head, WHOLE), None);
        assert_eq!(cut_short(&head, WHOLE + 40), None);
        assert_eq!(
            cut_short(&head, WHOLE - 1),
            Some(format!(
                "{} of the {WHOLE} bytes its header gives",
                WHOLE - 1
            ))
        );
        assert_eq!(
            cut_short(&head, HEADER as u64),
            Some(format!("152 of the {WHOLE} bytes its header gives"))
        );
    }

    /// The header of a raw profile a coverage build of this workspace's `js-lease-server`
    /// tests wrote with LLVM 23 (8820 data records, 17697 counters, 137541 bytes of names):
    /// the file was 914344 bytes.
    const REAL: [u64; 19] = [
        0xff6c_7072_6f66_7281,
        0xb,
        0x20,
        0x2274,
        0,
        0x4521,
        0,
        0,
        0,
        0,
        0,
        0,
        0x2_1945,
        0xffff_ffff_fffd_d6f8,
        0xffff_9fb9_8437_ecf0,
        0x0000_6046_7954_4f5e,
        0,
        0,
        2,
    ];

    /// Asserts that the header of a real profile gives the size the whole file had, so the
    /// words are read as the runtime writes them: whole at 914344 bytes, cut short a byte
    /// before.
    #[test]
    fn a_real_profile_is_whole_at_its_size_and_cut_short_a_byte_before() {
        let head: Vec<u8> = REAL.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(cut_short(&head, 914_344), None);
        assert_eq!(
            cut_short(&head, 914_343),
            Some("914343 of the 914344 bytes its header gives".to_owned())
        );
    }

    /// Asserts that the paddings and the bitmap the header names count toward its size.
    #[test]
    fn the_paddings_and_the_bitmap_count() {
        let head = header(32, 2, 3, 13, &[(4, 8), (6, 8), (7, 5), (8, 3), (10, 8)]);
        let needs = WHOLE + 8 + 8 + 5 + 3 + 8;
        assert_eq!(cut_short(&head, needs), None);
        assert_eq!(
            cut_short(&head, needs - 1),
            Some(format!(
                "{} of the {needs} bytes its header gives",
                needs - 1
            ))
        );
    }

    /// Asserts that an empty file, and a file that ends inside a header it began, are cut
    /// short, whatever of the magic they hold.
    #[test]
    fn an_empty_file_or_one_ending_in_its_header_is_cut_short() {
        assert_eq!(cut_short(&[], 0), Some("empty".to_owned()));
        let head = header(32, 2, 3, 13, &[]);
        for size in [1, 7, 8, 9, 151] {
            assert_eq!(
                cut_short(&head[..size], size as u64),
                Some(format!("{size} bytes, shorter than the 152 of a header")),
                "{size}"
            );
        }
    }

    /// Asserts that a short file that does not begin with the magic is not judged here, nor
    /// is a header of another magic, of another version (a variant flag set counts), with
    /// sections this does not size, or whose sizes overflow.
    #[test]
    fn what_this_cannot_size_is_left_to_llvm_profdata() {
        assert_eq!(cut_short(b"raw", 3), None);
        assert_eq!(cut_short(&[0x81, 0x72, 0x00], 3), None);
        let short = WHOLE - 1;
        let changes: [&[(usize, u64)]; 9] = [
            &[(0, MAGIC ^ 1)],
            &[(1, 10)],
            &[(1, VERSION | (1 << 60))],
            &[(9, 1)],
            &[(16, 1)],
            &[(17, 8)],
            &[(3, u64::MAX)],
            &[(5, u64::MAX)],
            &[(12, u64::MAX)],
        ];
        for set in changes {
            assert_eq!(
                cut_short(&header(32, 2, 3, 13, set), short),
                None,
                "{set:?}"
            );
        }
        assert_eq!(cut_short(&header(u64::MAX, 2, 3, 13, &[]), short), None);
    }

    /// Asserts that a head shorter than a header, for a file that is not, is not judged.
    #[test]
    fn a_head_that_is_not_a_whole_header_is_not_judged() {
        let head = header(32, 2, 3, 13, &[]);
        assert_eq!(cut_short(&head[..HEADER - 8], WHOLE - 1), None);
    }
}
