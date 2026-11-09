//! What the registry keeps of a failed prepare of a whole-host job.
//!
//! - [`reasons`]: why a prepare failed and what each reason does to the host.
//! - [`requests`]: the exclusion a failure earns its host, and the taint of `host-unsafe`.

pub(crate) mod reasons;
pub(crate) mod requests;
