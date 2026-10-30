//! The Lambda API handles the configuration describes: a reader for the watch path and an
//! aborter for the abort command.
//!
//! The watch path gets only a [`Reader`], which can list and nothing else; the [`Aborter`]
//! goes to the abort command alone.

use std::fmt::Display;

use js_elastic::SchedulerId;
use js_provider_lambdalabs::kind::DEFAULT_API_URL;
use js_provider_lambdalabs::{Aborter, ApiKey, Reader};

use crate::config::LambdaTable;

/// An exit code and what to say.
pub(crate) type Failure = (u8, String);

/// A refusal of the configuration or the command line: exit status 2.
pub(crate) fn refused(why: impl Display) -> Failure {
    (2, why.to_string())
}

/// The key the table names and the API address it uses.
fn credentials(table: &LambdaTable) -> Result<(ApiKey, &str), Failure> {
    let key = ApiKey::from_file(&table.api_key_file).map_err(refused)?;
    Ok((key, table.api_url.as_deref().unwrap_or(DEFAULT_API_URL)))
}

/// The read-only view of `scheduler`'s instances.
pub(crate) fn reader(table: &LambdaTable, scheduler: SchedulerId) -> Result<Reader, Failure> {
    let (key, url) = credentials(table)?;
    Reader::connect(url, &key, scheduler).map_err(refused)
}

/// The aborter for `scheduler`'s instances, for the abort command.
pub(crate) fn aborter(table: &LambdaTable, scheduler: SchedulerId) -> Result<Aborter, Failure> {
    let (key, url) = credentials(table)?;
    Aborter::connect(url, &key, scheduler).map_err(refused)
}
