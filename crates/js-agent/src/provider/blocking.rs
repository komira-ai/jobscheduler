//! The calls of one mount provider, each run to its answer while the caller waits, on a runtime
//! of one thread that is gone when the call returns.
//!
//! For a program that must have one thread when it forks (`js-agent __mac-job`, whose child
//! allocates between `fork` and `exec`): [`super::ProviderClient`] runs two worker threads and
//! does not join them when it is dropped. Here nothing outlives the call: the runtime is a
//! current-thread one, made for the call and dropped at its end.
//!
//! A tree is prepared, then mounted: `Prepare` is asked until the provider answers it (a
//! provider that is unavailable is asked again after [`super::CallBounds::retry`]), then `Mount`
//! until it names a path, both within one wait. A refusal ends the call at once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::MountMode;
use tokio::runtime::{Builder, Runtime};
use tonic::transport::{Channel, Endpoint};

use thiserror::Error;

use super::CallBounds;
use super::call;
use super::socket::UnixConnector;
use crate::lifecycle::{ProviderAnswer, ProviderCall};

/// Why a blocking call did not succeed.
#[derive(Debug, Error)]
pub enum TreeError {
    /// The runtime for the call could not be made.
    #[error("a runtime for the provider's calls: {0}")]
    Runtime(#[from] std::io::Error),
    /// The provider refused the call.
    #[error("refused: {0}")]
    Refused(String),
    /// The provider was unavailable for the whole wait; the last answer.
    #[error("not answered in time: {0}")]
    NotAnswered(String),
}

/// One provider, as a blocking caller names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// The id the calls carry as the agent's (`Source.agent`).
    pub agent: String,
    /// The provider's socket.
    pub socket: PathBuf,
    /// The uid the provider runs as, whose socket it is.
    pub user: u32,
    /// The bound of each call, and the pause before one is asked again.
    pub bounds: CallBounds,
}

impl Provider {
    /// Prepares the tree `reference` read-only as source `index` of run `run_id`, then mounts
    /// it: the path the provider names, as it names it (empty if it named none; the caller
    /// checks it, [`beneath`]). Waits at most `wait` for both.
    ///
    /// # Errors
    /// The runtime could not be made, the provider refused a call, or it was unavailable for
    /// all of `wait`.
    pub fn mount(
        &self,
        run_id: &str,
        index: u32,
        reference: &str,
        wait: Duration,
    ) -> Result<String, TreeError> {
        let (runtime, channel) = self.connect()?;
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + wait;
            let prepare = ProviderCall::Prepare {
                run_id: run_id.to_owned(),
                index,
                reference: reference.to_owned(),
                mode: MountMode::ReadOnly,
            };
            self.until_answered(&channel, prepare, self.bounds.prepare, deadline)
                .await?;
            let mount = ProviderCall::Mount {
                run_id: run_id.to_owned(),
                index,
            };
            self.until_answered(&channel, mount, self.bounds.mount, deadline)
                .await
                .map(Option::unwrap_or_default)
        })
    }

    /// Releases source `index` of run `run_id`, waiting at most `wait`.
    ///
    /// # Errors
    /// As [`Provider::mount`].
    pub fn release(&self, run_id: &str, index: u32, wait: Duration) -> Result<(), TreeError> {
        let (runtime, channel) = self.connect()?;
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + wait;
            let release = ProviderCall::Release {
                run_id: run_id.to_owned(),
                index,
            };
            self.until_answered(&channel, release, self.bounds.release, deadline)
                .await
                .map(drop)
        })
    }

    /// A current-thread runtime, and the provider's channel made on it.
    fn connect(&self) -> Result<(Runtime, Channel), TreeError> {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        let channel = {
            // A channel starts its worker on the runtime it is made in.
            let _entered = runtime.enter();
            Endpoint::from_static("http://provider.invalid")
                .connect_with_connector_lazy(UnixConnector::new(self.socket.clone(), self.user))
        };
        Ok((runtime, channel))
    }

    /// Asks `call` until it is answered, refused, or `deadline` has passed.
    async fn until_answered(
        &self,
        channel: &Channel,
        call: ProviderCall,
        bound: Duration,
        deadline: tokio::time::Instant,
    ) -> Result<Option<String>, TreeError> {
        loop {
            match call::run(channel.clone(), self.agent.clone(), call.clone(), bound).await {
                ProviderAnswer::Done(path) => return Ok(path),
                ProviderAnswer::Refused(why) => return Err(TreeError::Refused(why)),
                other => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(TreeError::NotAnswered(format!("{other:?}")));
                    }
                    tokio::time::sleep(self.bounds.retry).await;
                }
            }
        }
    }
}

/// Whether `path` is absolute and beneath `root` (the provider's mount root, resolved).
#[must_use]
pub fn beneath(path: &Path, root: &Path) -> bool {
    path.is_absolute() && path != root && path.starts_with(root)
}

#[cfg(test)]
#[path = "blocking_tests.rs"]
mod tests;
