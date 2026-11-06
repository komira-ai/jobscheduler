//! The client of the port over a scripted transport: the shape of each request (action,
//! parameters, signing), each answer read into the records, pages, every shape of error
//! answer, the timeout, and the credentials, with no socket and no clock but the injected
//! one.

mod catalog;
mod credential;
mod launch;
mod pricing;
mod script;
mod secrets;
mod wire;
