//! `js job follow`, written by hand: writes one of a job's output streams to stdout from its
//! first byte to its end (`js_client::jobs::follow_logs`), as the job writes it. This file
//! reads the flags and writes the pieces; `job run` writes its two streams the same way.

use std::cell::RefCell;
use std::io::{self, Write};

use clap::{Arg, ArgMatches};
use js_client::jobs::{Piece, follow_logs};
use js_proto::v1::LogStream;

use super::wait::{job_id, job_of};
use super::{Connection, FLAG_ORDER, runtime};
use crate::Services;
use crate::failure::{Failure, exit};
use crate::render::Render;

/// The command line of `job follow`.
pub(crate) fn command() -> clap::Command {
    clap::Command::new("follow")
        .about("Writes a job's output to stdout as the job writes it, until the job has ended and every byte is written.")
        .long_about(
            "Writes a job's output to stdout as the job writes it, until the job has ended and every byte is written. Reads the stream with GetLogs from its first byte, each call asking the server to wait for new bytes and starting where the last one ended, so each byte is written once and exactly as the job wrote it. Bytes the log store does not hold are said on stderr. Exits 0 at the stream's end. After an answer with no new bytes js pauses a second; a follow the server refuses as busy is tried again after five seconds, and one whose answer is lost after two, three times in a row at most; any other failed call exits as any job command does. `js job logs` reads one page; this follows to the end. Written by hand, not generated from the job service.",
        )
        .arg(job_id("The id of the job whose output to follow."))
        .arg(
            Arg::new("stream")
                .long("stream")
                .value_name("NAME")
                .help("The stream to follow: stdout or stderr.")
                .value_parser(["stdout", "stderr"])
                .default_value("stdout")
                .display_order(FLAG_ORDER),
        )
}

/// Runs `job follow`.
pub(crate) fn run(
    globals: &ArgMatches,
    args: &ArgMatches,
    render: Render,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: &Services,
) -> Result<u8, Failure> {
    let job = job_of(args);
    let (stream, name) = match args.get_one::<String>("stream").map(String::as_str) {
        Some("stderr") => (LogStream::Stderr, "stderr"),
        _ => (LogStream::Stdout, "stdout"),
    };
    let connection = Connection::of(globals, &services.dialer)?;
    let waiter = &*services.job_waiter;
    let (out, err) = (RefCell::new(out), RefCell::new(err));
    runtime()?.block_on(async {
        let mut client = connection.client().await?;
        let mut sink = |piece: Piece<'_>| put(piece, name, render, &out, &err);
        follow_logs(&mut client, waiter, &job, stream, &mut sink).await?;
        Ok(exit::OK)
    })
}

/// Writes `piece` of the stream named `name`: its data to `data`, exactly as the job wrote
/// it, or the line that says a gap to `notes`. The two may be one writer: each is borrowed
/// only while it is written to.
pub(crate) fn put(
    piece: Piece<'_>,
    name: &str,
    render: Render,
    data: &RefCell<&mut dyn Write>,
    notes: &RefCell<&mut dyn Write>,
) -> io::Result<()> {
    match piece {
        Piece::Data(bytes) => write(&mut **data.borrow_mut(), bytes),
        Piece::Gap { offset, length } => {
            let line = render.gap(name, offset, length);
            write(&mut **notes.borrow_mut(), line.as_bytes())
        }
    }
}

/// Writes `bytes` to `to` and flushes it, so a reader sees them as they come.
pub(crate) fn write(to: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    to.write_all(bytes)?;
    to.flush()
}
