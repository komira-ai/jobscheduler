//! The outputs of a CI step come back in its job's stdout: after the step, the job's script
//! writes a line break, a marker, a space, a length in bytes and a line break, then that many
//! bytes of tar archive (`super::script`). [`Demux`] sits between the followed stream and
//! `js`'s stdout: it writes every byte before the frame and after it unchanged, takes the
//! frame out, and keeps the archive.
//!
//! The marker holds the job's id and is the job's argument, so the step's own output does not
//! hold it by chance. The line break before it is part of the frame, so a
//! step whose last line ends in a line break shows it once.

use std::io::{self, Write};

/// The most bytes an archive may hold; a larger length is refused, and nothing of it is kept.
pub(crate) const MAX_ARCHIVE: u64 = 1 << 30;

/// The most bytes of a frame's length line: 20 digits for a 64-bit number.
const MAX_LENGTH_LINE: usize = 20;

/// What a stream held of a frame, once it has ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    /// No frame: the stream ended without the marker.
    Absent,
    /// The archive, whole (empty when the job made none of its outputs).
    Read(Vec<u8>),
    /// A frame that cannot be read, and why.
    Broken(String),
}

/// Where the demultiplexer is in the stream.
enum State {
    /// Before the frame: bytes go out, but for an end that may be the marker's beginning.
    Passing(Vec<u8>),
    /// Reading the length line.
    Length(Vec<u8>),
    /// Reading the archive: the bytes still wanted, and those read.
    Archive(u64, Vec<u8>),
    /// After the frame, or after a frame found broken: every byte goes out.
    After(Frame),
}

/// A writer that passes a job's stdout to `out` and takes the outputs frame out of it.
pub(crate) struct Demux<'a> {
    out: &'a mut dyn Write,
    needle: Vec<u8>,
    state: State,
}

impl<'a> Demux<'a> {
    /// A demultiplexer for the frame of `marker`; with `None`, a writer that passes every
    /// byte.
    pub(crate) fn new(out: &'a mut dyn Write, marker: Option<&str>) -> Self {
        let (needle, state) = match marker {
            Some(marker) => {
                let needle = [b"\n", marker.as_bytes(), b" "].concat();
                (needle, State::Passing(Vec::new()))
            }
            None => (Vec::new(), State::After(Frame::Absent)),
        };
        Self { out, needle, state }
    }

    /// The frame the stream held, after its end: bytes held back as a possible marker go out
    /// first.
    pub(crate) fn finish(mut self) -> io::Result<Frame> {
        let state = std::mem::replace(&mut self.state, State::After(Frame::Absent));
        match state {
            State::Passing(held) => {
                self.out.write_all(&held)?;
                self.out.flush()?;
                Ok(Frame::Absent)
            }
            State::Length(_) | State::Archive(..) => Ok(Frame::Broken(
                "the job's stdout ended inside its outputs".to_owned(),
            )),
            State::After(frame) => Ok(frame),
        }
    }

    /// Takes `bytes` in the state it is in, and gives what is left for the next state.
    fn step<'b>(&mut self, bytes: &'b [u8]) -> io::Result<&'b [u8]> {
        let state = std::mem::replace(&mut self.state, State::After(Frame::Absent));
        let (next, rest) = match state {
            State::Passing(held) => self.passing(held, bytes)?,
            State::Length(line) => length(line, bytes),
            State::Archive(want, got) => archive(want, got, bytes),
            State::After(frame) => {
                self.out.write_all(bytes)?;
                (State::After(frame), &[][..])
            }
        };
        self.state = next;
        Ok(rest)
    }

    /// Before the frame: writes what cannot be the marker, holds what may be its
    /// beginning, and at the marker gives the bytes after it.
    fn passing<'b>(&mut self, held: Vec<u8>, bytes: &'b [u8]) -> io::Result<(State, &'b [u8])> {
        let data = [held.as_slice(), bytes].concat();
        if let Some(at) = find(&data, &self.needle) {
            self.out.write_all(&data[..at])?;
            // The bytes after the marker are the tail of `bytes`: the marker ends in `bytes`
            // or the held bytes and `bytes` hold all of it.
            let after = data.len() - at - self.needle.len();
            let rest = &bytes[bytes.len() - after..];
            return Ok((State::Length(Vec::new()), rest));
        }
        let keep = partial(&data, &self.needle);
        let (write, hold) = data.split_at(data.len() - keep);
        self.out.write_all(write)?;
        Ok((State::Passing(hold.to_vec()), &[][..]))
    }
}

impl Write for Demux<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while !rest.is_empty() {
            rest = self.step(rest)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Reads the length line: digits to a line break.
fn length(mut line: Vec<u8>, bytes: &[u8]) -> (State, &[u8]) {
    let Some(at) = bytes.iter().position(|byte| *byte == b'\n') else {
        line.extend_from_slice(bytes);
        let state = match line.len() > MAX_LENGTH_LINE {
            true => broken("its length line is too long"),
            false => State::Length(line),
        };
        return (state, &[]);
    };
    line.extend_from_slice(&bytes[..at]);
    let rest = &bytes[at + 1..];
    let digits = std::str::from_utf8(&line).ok();
    let digits = digits.filter(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()));
    let state = match digits.map(str::parse::<u64>) {
        Some(Ok(0)) => State::After(Frame::Read(Vec::new())),
        Some(Ok(size)) if size <= MAX_ARCHIVE => State::Archive(size, Vec::new()),
        Some(Ok(size)) => broken(&format!("{size} bytes, more than {MAX_ARCHIVE}")),
        _ => broken("its length is not a number"),
    };
    (state, rest)
}

/// Reads the archive's bytes, up to its length.
fn archive(want: u64, mut got: Vec<u8>, bytes: &[u8]) -> (State, &[u8]) {
    let missing = want - got.len() as u64;
    let take = usize::try_from(missing).map_or(bytes.len(), |missing| missing.min(bytes.len()));
    got.extend_from_slice(&bytes[..take]);
    let state = match got.len() as u64 == want {
        true => State::After(Frame::Read(got)),
        false => State::Archive(want, got),
    };
    (state, &bytes[take..])
}

/// The state after a frame found broken for `why`.
fn broken(why: &str) -> State {
    State::After(Frame::Broken(format!("the outputs frame is broken: {why}")))
}

/// Where `needle` first stands in `data`.
fn find(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .position(|window| window == needle)
}

/// How many bytes at the end of `data` are the beginning of `needle`, at most one fewer than
/// its length.
fn partial(data: &[u8], needle: &[u8]) -> usize {
    let most = data.len().min(needle.len() - 1);
    (1..=most)
        .rev()
        .find(|len| data.ends_with(&needle[..*len]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "js-ci-outputs-ab12";

    /// Writes each piece of `pieces` to a demultiplexer of [`MARKER`] and gives what went
    /// out and the frame.
    fn through(pieces: &[&[u8]]) -> (Vec<u8>, Frame) {
        let mut out = Vec::new();
        let mut demux = Demux::new(&mut out, Some(MARKER));
        for piece in pieces {
            demux.write_all(piece).unwrap();
        }
        let frame = demux.finish().unwrap();
        (out, frame)
    }

    /// The bytes of a stream: `before`, the frame of `archive`, then `after`.
    fn stream(before: &[u8], archive: &[u8], after: &[u8]) -> Vec<u8> {
        let head = format!("\n{MARKER} {}\n", archive.len());
        [before, head.as_bytes(), archive, after].concat()
    }

    /// Asserts that a stream cut into pieces at every byte, at every pair of places and in one
    /// piece gives the same output and the same archive: the step's lines before and after the
    /// frame go out, the frame does not, the archive is kept whole. It catches a marker or a
    /// length line split between two pieces and missed, and bytes held back and lost.
    #[test]
    fn a_frame_is_taken_out_wherever_the_pieces_are_cut() {
        let archive = b"tar\nbytes \x00\xff with a line break";
        let whole = stream(b"build ok\n", archive, b"late\n");
        let want = (b"build ok\nlate\n".to_vec(), Frame::Read(archive.to_vec()));
        assert_eq!(through(&[&whole]), want);
        let bytes: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(through(&bytes), want);
        for first in 0..whole.len() {
            for second in first..whole.len() {
                let pieces = [&whole[..first], &whole[first..second], &whole[second..]];
                assert_eq!(through(&pieces), want, "cut at {first} and {second}");
            }
        }
    }

    /// Asserts that output that only begins like the marker goes out whole, at the end of the
    /// stream too, and that a stream with no frame gives none. It catches bytes held back as
    /// a possible marker and never written.
    #[test]
    fn output_like_the_marker_goes_out_and_no_frame_is_absent() {
        let near = format!("a\n{} b\n\n{MARKER}", &MARKER[..5]);
        let (out, frame) = through(&[near.as_bytes(), b"x\n"]);
        assert_eq!(
            (out, frame),
            ([near.as_bytes(), b"x\n"].concat(), Frame::Absent)
        );
        let tail = format!("end\n{}", &MARKER[..9]);
        let (out, frame) = through(&[tail.as_bytes()]);
        assert_eq!((out, frame), (tail.into_bytes(), Frame::Absent));
        assert_eq!(through(&[]), (Vec::new(), Frame::Absent));
    }

    /// Asserts that a length of 0 is an empty archive and the bytes after it go out. It
    /// catches a frame with no archive waited for until the stream ends.
    #[test]
    fn a_length_of_zero_is_an_empty_archive() {
        let whole = stream(b"", b"", b"after");
        assert_eq!(
            through(&[&whole]),
            (b"after".to_vec(), Frame::Read(Vec::new()))
        );
    }

    /// Asserts each broken frame, by the whole message: a length that is not a number, an
    /// empty one, one above [`MAX_ARCHIVE`], a length line too long, and a stream that ends
    /// inside the length line or inside the archive. The bytes after a broken length line go
    /// out. It catches a broken frame read as an archive.
    #[test]
    fn each_broken_frame_says_why() {
        let bad = |line: &str| format!("x\n{MARKER} {line}\nrest");
        let not_number = "the outputs frame is broken: its length is not a number";
        assert_eq!(
            through(&[bad("12a").as_bytes()]),
            (b"xrest".to_vec(), Frame::Broken(not_number.to_owned()))
        );
        assert_eq!(
            through(&[bad("").as_bytes()]).1,
            Frame::Broken(not_number.to_owned())
        );
        let above = MAX_ARCHIVE + 1;
        assert_eq!(
            through(&[bad(&above.to_string()).as_bytes()]).1,
            Frame::Broken(format!(
                "the outputs frame is broken: {above} bytes, more than 1073741824"
            ))
        );
        let long = format!("\n{MARKER} {}", "9".repeat(21));
        assert_eq!(
            through(&[long.as_bytes(), b"tail"]),
            (
                b"tail".to_vec(),
                Frame::Broken(
                    "the outputs frame is broken: its length line is too long".to_owned()
                )
            )
        );
        let cut = "the job's stdout ended inside its outputs".to_owned();
        let in_line = format!("\n{MARKER} 12");
        assert_eq!(through(&[in_line.as_bytes()]).1, Frame::Broken(cut.clone()));
        let in_archive = format!("\n{MARKER} 12\nshort");
        assert_eq!(through(&[in_archive.as_bytes()]).1, Frame::Broken(cut));
    }

    /// Asserts that with no marker every byte goes out, the marker of another run too, and
    /// there is no frame. It catches a frame taken out of a run that declared no outputs.
    #[test]
    fn without_a_marker_every_byte_goes_out() {
        let mut out = Vec::new();
        let mut demux = Demux::new(&mut out, None);
        let whole = stream(b"a", b"zz", b"b");
        demux.write_all(&whole).unwrap();
        demux.flush().unwrap();
        assert_eq!(demux.finish().unwrap(), Frame::Absent);
        assert_eq!(out, whole);
    }

    /// A writer that fails every write.
    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("closed"))
        }
    }

    /// Asserts that a write to a closed stdout fails, before the frame, after it, and at the
    /// end for bytes held back, and that a flush fails with it. It catches a failed write
    /// taken for written.
    #[test]
    fn a_failed_write_is_an_error() {
        let mut closed = Closed;
        let mut demux = Demux::new(&mut closed, Some(MARKER));
        assert!(demux.write_all(b"abc").is_err());
        let mut closed = Closed;
        let mut demux = Demux::new(&mut closed, Some(MARKER));
        demux.write_all(b"\n").unwrap();
        assert!(demux.finish().is_err());
        let mut closed = Closed;
        let mut demux = Demux::new(&mut closed, None);
        assert!(demux.write_all(b"abc").is_err());
        assert!(demux.flush().is_err());
    }
}
