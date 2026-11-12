//! Test support: a row's fields read, set and rearranged as a later or an earlier build might
//! write them.

use super::{Carried, Fields, Writer, rootfs_fields};

/// A format byte no build of this one writes, for any kind of row: what a later build's row
/// holds.
pub(crate) const LATER_FORMAT: u8 = rootfs_fields::FORMAT_NO_IMAGE + 1;

/// The value of field `tag` in `row`, if the row parses and holds it. Test support.
pub(crate) fn field_of(row: &[u8], tag: u16) -> Option<Vec<u8>> {
    Fields::of_job(row).ok()?.0.remove(&tag)
}

/// `row` with its field `tag` set to `value` (added, or put in place of the one it holds),
/// as a later build might write it; `None` when `row` does not parse. Test support.
pub(crate) fn set_field(row: &[u8], tag: u16, value: &[u8]) -> Option<Vec<u8>> {
    let mut fields = Fields::of_job(row).ok()?;
    fields.0.insert(tag, value.to_vec());
    Writer(fields.0)
        .finish_as(fields.1, &Carried::default())
        .ok()
}

/// `row` with its fields in descending tag order and the fields `without` left out: the same
/// record, if what is left out is a default, in bytes no build writes. Test support.
pub(crate) fn rearranged(row: &[u8], without: &[u16]) -> Option<Vec<u8>> {
    let fields = Fields::of_job(row).ok()?;
    let mut out = vec![fields.1];
    for (tag, value) in fields.0.iter().rev() {
        if !without.contains(tag) {
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&(value.len() as u32).to_be_bytes());
            out.extend_from_slice(value);
        }
    }
    Some(out)
}
