//! The supervisor's system calls, which only macOS has: the audit session ([`audit`]), and the
//! drop to the slot user, the limits and the Seatbelt profile (`sys`), and the start time of a
//! process, by which a later agent knows a supervisor ([`start_of`]). All of the supervisor's
//! `unsafe` code is here.

pub mod audit;
mod process;
mod sys;

pub use process::start_of;

pub(crate) use audit::new_audit_session as audit_session;
pub(crate) use sys::{apply_profile, drop_to, limits};
