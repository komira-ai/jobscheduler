//! The fields that name a job's work: its labels (tag 20, `job_tag::LABELS`, a map) and the
//! name of its size class ([`job_class_tag::SIZE_CLASS`], text), with the same additive rules
//! (B8) as every other field.
//!
//! The size class is written only for a job that names one, so the row of a job that names
//! none has the bytes a build before the field wrote, and a row without the tag reads as none.
//! A build from before the tag skips it as it skips any tag it does not know and carries it
//! into its next write of the row ([`super::Carried`]); it sizes the job by its labels alone. A
//! value that is not a [`SizeClassName`] (empty, over 512 bytes, or a byte outside printable
//! ASCII) is [`DecodeError::BadValue`], as a refused label is.

use js_types::{Labels, SizeClassName};

use super::{DecodeError, Fields, Writer, job_tag, map, text_of};

/// The tag the size class adds to a job row, after the ones of `job_tag` and the other
/// `*_fields` modules.
pub(crate) mod job_class_tag {
    /// The name of the job's size class (text, 1 to 512 bytes of printable ASCII).
    pub const SIZE_CLASS: u16 = 66;
}

/// Puts `labels` and `size_class` in `row`: the labels always (an empty map when there are
/// none), the size class only when the job names one.
pub(super) fn put(row: &mut Writer, labels: &Labels, size_class: Option<&SizeClassName>) {
    row.put(
        job_tag::LABELS,
        map(labels.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes()))),
    );
    row.put_opt(
        job_class_tag::SIZE_CLASS,
        size_class.map(|class| class.as_str().as_bytes().to_vec()),
    );
}

/// The size class of a row: none when the row lacks the tag.
pub(super) fn take_size_class(fields: &mut Fields) -> Result<Option<SizeClassName>, DecodeError> {
    fields.opt(job_class_tag::SIZE_CLASS, size_class_of)
}

/// The size class a value holds.
fn size_class_of(tag: u16, value: Vec<u8>) -> Result<SizeClassName, DecodeError> {
    SizeClassName::parse(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

#[cfg(test)]
#[path = "class_fields_tests.rs"]
mod tests;
