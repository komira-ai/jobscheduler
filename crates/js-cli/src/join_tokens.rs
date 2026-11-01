//! `lease join-token mint`: asks the server for a one-time join token and puts it where the
//! caller said, never on a terminal.

use std::io::Write;

use js_client::Client;

use crate::cli::MintArgs;
use crate::failure::Failure;
use crate::render::Render;

/// `lease join-token mint`.
pub(crate) fn mint(
    client: &Client,
    render: Render,
    args: &MintArgs,
    stdout_is_terminal: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let _ = (client, render, args.body(), stdout_is_terminal, out, err);
    Err(Failure::Usage(
        "join-token mint is not implemented".to_owned(),
    ))
}
