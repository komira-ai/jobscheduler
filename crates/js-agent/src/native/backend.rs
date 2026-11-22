//! Who makes a native run's user, starts it and ends it: the root helper over its socket, or
//! this program itself ([`super::one_program`]). The native runtime asks the one it was opened
//! with, by the helper's verbs.

use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_mac_pool::seatbelt::Profile;
use js_mac_session::client::{Client, ClientError};
use js_mac_session::proto::{Limits, Session};

use super::one_program::{OneProgram, Started};

/// The engine of a native runtime.
pub enum Backend {
    /// The root helper `js-mac-session`.
    Helper(Client),
    /// The agent itself.
    OneProgram(Arc<OneProgram>),
}

impl Backend {
    /// The helper's boot reset; nothing for the agent itself, whose pool swept at its open.
    pub(super) fn boot_reset(&self) -> Result<(), ClientError> {
        match self {
            Self::Helper(helper) => helper.boot_reset(&[]).map(drop),
            Self::OneProgram(_) => Ok(()),
        }
    }

    /// A user for `lease`: its uid.
    pub(super) fn user_create(&self, lease: &str) -> Result<u32, ClientError> {
        match self {
            Self::Helper(helper) => helper.user_create(lease, None),
            Self::OneProgram(engine) => engine.user_create(lease),
        }
    }

    /// The user's name, for an engine that lent it itself; `None` for the helper's (its pool
    /// is asked, `native/pool_user.rs`).
    pub(super) fn lent_name(&self, lease: &str) -> Option<Result<String, ClientError>> {
        match self {
            Self::Helper(_) => None,
            Self::OneProgram(engine) => Some(engine.user_name(lease)),
        }
    }

    /// Starts the run of `lease`. The helper is given the `sandbox-exec` command line of
    /// `profile` and `command`; the agent itself applies `profile` in the start process.
    pub(super) fn run(
        &self,
        lease: &str,
        (profile, command): (&Profile, &[String]),
        env: &[(String, String)],
        fds: [BorrowedFd<'_>; 4],
        (limits, deadline): (Option<Limits>, Option<Duration>),
        (cwd, scratch): (&Path, &Path),
    ) -> Result<Started, ClientError> {
        match self {
            Self::Helper(helper) => {
                let none = String::new();
                let (program, args) = command.split_first().unwrap_or((&none, &[]));
                let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
                let argv: Vec<String> = profile
                    .argv(program, &args)
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect();
                let running = match deadline {
                    Some(deadline) => {
                        helper.run_bounded(lease, (&argv, env), fds, limits, deadline)
                    }
                    None => helper.run_limited(lease, &argv, env, fds, Session::Background, limits),
                }?;
                Ok(Started::Helper(running))
            }
            Self::OneProgram(engine) => {
                engine.run(lease, (profile, command), env, fds, (cwd, scratch))
            }
        }
    }

    /// Stops the run of `lease`, killing it once `grace` has passed.
    pub(super) fn stop(&self, lease: &str, grace: Duration) -> Result<(), ClientError> {
        match self {
            Self::Helper(helper) => helper.stop(lease, grace),
            Self::OneProgram(engine) => engine.stop(lease, grace),
        }
    }

    /// Ends every process of the run of `lease`.
    pub(super) fn kill_uid(&self, lease: &str) -> Result<(), ClientError> {
        match self {
            Self::Helper(helper) => helper.kill_uid(lease),
            Self::OneProgram(engine) => engine.kill_uid(lease),
        }
    }

    /// Ends the user of `lease`: the helper resets its pool user; the agent retires its account.
    pub(super) fn user_delete(&self, lease: &str) -> Result<(), ClientError> {
        match self {
            Self::Helper(helper) => helper.user_delete(lease).map(drop),
            Self::OneProgram(engine) => engine.user_delete(lease),
        }
    }

    /// The helper, for what only it does (adoption's `watch`); `None` for the agent itself.
    pub(super) fn helper(&self) -> Option<&Client> {
        match self {
            Self::Helper(helper) => Some(helper),
            Self::OneProgram(_) => None,
        }
    }
}
