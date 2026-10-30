//! `inspect`: stub.

use std::io::Write;

use crate::cli::LayoutArgs;
use crate::error::Error;

pub(crate) async fn inspect(_args: &LayoutArgs, _out: &mut dyn Write) -> Result<(), Error> {
    todo!("inspect")
}
