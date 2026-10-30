//! The Lambda API client the configuration describes, as the handle each command needs.
//!
//! The client can launch and terminate. The watch path gets it only as a [`LambdaRead`],
//! which has no such call; only the abort command takes the whole [`LambdaApi`].

use std::fmt::Display;
use std::sync::Arc;

use js_provider_lambdalabs::kind::{API_TIMEOUT, DEFAULT_API_URL};
use js_provider_lambdalabs::{ApiKey, HttpApi, LambdaApi, LambdaRead};

use crate::config::LambdaTable;

/// An exit code and what to say.
pub(crate) type Failure = (u8, String);

/// A refusal of the configuration or the command line: exit status 2.
pub(crate) fn refused(why: impl Display) -> Failure {
    (2, why.to_string())
}

fn connect(table: &LambdaTable) -> Result<Arc<HttpApi>, Failure> {
    let key = ApiKey::from_file(&table.api_key_file).map_err(refused)?;
    let url = table.api_url.as_deref().unwrap_or(DEFAULT_API_URL);
    let api = HttpApi::connect(url, &key, None, API_TIMEOUT).map_err(refused)?;
    Ok(Arc::new(api))
}

/// The client as a handle that can only read.
pub(crate) fn read_handle(table: &LambdaTable) -> Result<Arc<dyn LambdaRead>, Failure> {
    let api: Arc<dyn LambdaRead> = connect(table)?;
    Ok(api)
}

/// The client whole, for the abort command.
pub(crate) fn full_handle(table: &LambdaTable) -> Result<Arc<dyn LambdaApi>, Failure> {
    let api: Arc<dyn LambdaApi> = connect(table)?;
    Ok(api)
}
