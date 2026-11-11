//! The store the server's state is kept in (`--store`, `server.store`), opened under its
//! writer lock, and the loss of that lock.
//!
//! The server takes the store's writer lock ([`WriterLock::acquire`]) first thing at start:
//! before it reads anything from the store, and before it binds a listener (the readiness
//! listener of `--ready-listen` excepted, which is bound before the lock so that it can say
//! the server waits for it). The lock is tried once: when another process holds it the start
//! is refused with one line naming the lock, and nothing else is done. With `--ready-listen`
//! the server instead tries again at each tick of its retry ticker for as long as the lock
//! is held ([`ServerStore::open_waiting`]); a failure to try the lock at all is refused
//! either way. The server then holds the lock for its life.
//!
//! - **Postgres**: the lock is a session-level advisory lock on a connection of its own.
//!   Every tick asks that session first ([`ServerStore::check`]); when the session is gone,
//!   so is the lock, and the tick does nothing more: the server stops serving, does not tick
//!   again, writes nothing more, and the process exits 1 ([`LockLost`]). Taking the lock also
//!   takes the store's next writer generation, and the store is opened fenced by it
//!   ([`PostgresStore::fenced`]): once another process takes the lock, every commit of this
//!   one is refused, between ticks too.
//! - **SQLite**: the store holds the `flock` beside its file from before its open to its end
//!   ([`SqliteStore::open_locked`]), so a lock this process holds cannot be lost to another.
//!
//! What the store keeps: the host bindings, with their taints and cordons, and the join
//! tokens' digests ([`crate::host_keys`]), and the scaler's records and the hosts the
//! registry refused to remove (`acquires/scaler/records`, `acquires/scaler/unremoved`), read
//! once at start after the lock is taken. The job store and the usage
//! database are where they are without the setting.
//!
//! The first start over a store imports the four files a server without the setting keeps
//! beside the state file (`.agents`, `.joins`, `.scaler`, `.scaler.unremoved`) once, right
//! after the lock is taken and before anything is read from the store ([`ServerStore::import`]).

use std::fs::{OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use js_state::postgres::PostgresStore;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{LockRefused, Store, StoreError, SystemClock, WriterLock, WriterTarget};
use tokio::sync::{Mutex, oneshot, watch};
use tracing::{error, info};

use crate::config::{StoreNames, StoreSetting};
use crate::error::StartupError;
use crate::ticker::{Tick, Ticker};
use crate::util::{blocking, or_pending};

mod import;

/// The store, opened under the writer lock the server holds for its life.
pub(crate) struct ServerStore {
    /// The store, for the parts of the server that keep their state in it.
    store: Arc<dyn Store>,
    /// The writer lock on Postgres, asked at every tick. `None` on SQLite, whose store holds
    /// the lock itself.
    lock: Option<Mutex<WriterLock>>,
    /// `--store` or `server.store`, for the refusals.
    names: StoreNames,
}

impl ServerStore {
    /// Takes the writer lock of the store `setting` names, then opens the store under it. A
    /// SQLite file is made owner-only first, as the server's other files are.
    ///
    /// # Errors
    ///
    /// A refusal naming the setting: another process holds the lock, the lock could not be
    /// tried, or the store could not be opened. None repeats the connection URL.
    pub(crate) fn open(setting: StoreSetting) -> Result<Self, StartupError> {
        let lock = WriterLock::acquire(&setting.target)
            .map_err(|refused| setting.names.refuse(refused))?;
        Self::open_locked(setting, lock)
    }

    /// [`ServerStore::open`] for a server that waits: while another process holds the
    /// writer lock, logs once that the server waits and tries again at each tick of
    /// `retry`, finishing the tick when the attempt it asked for is done. The tries and the
    /// open run off the runtime's threads.
    ///
    /// # Errors
    ///
    /// As [`ServerStore::open`], except that a lock another process holds is waited for, not
    /// refused.
    pub(crate) async fn open_waiting<R: Ticker>(
        setting: StoreSetting,
        mut retry: R,
    ) -> Result<Self, StartupError> {
        let mut asked: Option<Tick> = None;
        let mut waiting = false;
        let lock = loop {
            let target = setting.target.clone();
            let tried = blocking(move || WriterLock::acquire(&target)).await;
            asked.take().into_iter().for_each(Tick::finish);
            match tried {
                Ok(lock) => break lock,
                Err(LockRefused::Held) => {
                    if !waiting {
                        info!("waiting for the store's writer lock");
                        waiting = true;
                    }
                    asked = Some(retry.next().await);
                }
                Err(refused) => return Err(setting.names.refuse(refused)),
            }
        };
        blocking(move || Self::open_locked(setting, lock)).await
    }

    /// Opens the store of `setting` under `lock`, which was taken on its target.
    fn open_locked(setting: StoreSetting, lock: WriterLock) -> Result<Self, StartupError> {
        let StoreSetting { target, names } = setting;
        let refused = |err: String| names.refuse(format!("the store could not be opened: {err}"));
        let (store, lock): (Arc<dyn Store>, _) = match target {
            WriterTarget::Postgres(config) => {
                let store = PostgresStore::open(&config).map_err(|err| refused(err.to_string()))?;
                // Every commit checks the store still holds the generation this lock took
                // (always one on Postgres), so none succeeds once another process took it.
                let store = lock
                    .generation()
                    .into_iter()
                    .fold(store, PostgresStore::fenced);
                (Arc::new(store), Some(Mutex::new(lock)))
            }
            WriterTarget::Sqlite(file) => {
                let store = open_sqlite(&file, lock).map_err(refused)?;
                (Arc::new(store), None)
            }
        };
        info!("the store is open and its writer lock is held");
        Ok(Self { store, lock, names })
    }

    /// The first start's import of the files beside the state file at `state_file`, once,
    /// and a no-op at every later start (see the `import` module). `records_needed` is true
    /// for a live elastic pool that is not at its first start, which does not start without
    /// its records.
    ///
    /// # Errors
    ///
    /// A file's refusal, naming the file, or the store could not be read or written, naming
    /// the store's setting: nothing was imported.
    pub(crate) fn import(
        &self,
        state_file: &Path,
        records_needed: bool,
    ) -> Result<(), StartupError> {
        let refuse = |reason: String| self.refuse(reason);
        import::run(&*self.store, state_file, records_needed, &refuse)
    }

    /// A refusal of the start for `reason`, naming the store's flag or key: what the store
    /// holds cannot be read as written.
    pub(crate) fn refuse(&self, reason: impl std::fmt::Display) -> StartupError {
        self.names.refuse(reason)
    }

    /// Whether the writer lock is still held: on Postgres a trivial query on the lock's own
    /// session, on SQLite always.
    ///
    /// # Errors
    ///
    /// The lock is lost: its session is gone, or the query failed.
    pub(crate) async fn check(&self) -> Result<(), LockLost> {
        let Some(lock) = &self.lock else {
            return Ok(());
        };
        lock.lock().await.check().map_err(LockLost::from)
    }

    /// The store, shared.
    pub(crate) fn store(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }
}

/// Opens the SQLite store at `file` under `lock`, made owner-only first.
fn open_sqlite(file: &Path, lock: WriterLock) -> Result<SqliteStore, String> {
    let made = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(file)
        .and_then(|opened| opened.set_permissions(Permissions::from_mode(0o600)));
    made.map_err(|err| format!("{}: {err}", file.display()))?;
    let clock = Arc::new(SystemClock::default());
    SqliteStore::open_locked(file, lock, clock, Options::default()).map_err(|err| err.to_string())
}

/// The store's writer lock was lost while the server ran, so the server stopped: it serves
/// nothing more, ticks no more and writes nothing more, and the process exits 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the store's writer lock was lost, so the server stopped: {reason}")]
pub struct LockLost {
    reason: String,
}

impl From<StoreError> for LockLost {
    fn from(err: StoreError) -> Self {
        Self {
            reason: err.to_string(),
        }
    }
}

/// Where the loss of the writer lock is announced: the tick that finds it says so here, and
/// the listeners, [`crate::Running::lock_lost`] and the shutdown read it.
#[derive(Clone)]
pub(crate) struct LockWatch {
    lost: watch::Sender<Option<LockLost>>,
}

impl LockWatch {
    pub(crate) fn new() -> Self {
        Self {
            lost: watch::Sender::new(None),
        }
    }

    /// Announces the loss, logged once at error level.
    pub(crate) fn lose(&self, lost: LockLost) {
        error!(%lost, "stopping: the server serves, ticks and writes nothing more");
        self.lost.send_replace(Some(lost));
    }

    /// The loss, once announced.
    pub(crate) fn lost(&self) -> Option<LockLost> {
        self.lost.borrow().clone()
    }

    /// Completes when the loss is announced, with the loss. (The watch holds the sender, so
    /// the wait ends only with a loss.)
    pub(crate) async fn wait(&self) -> LockLost {
        let mut lost = self.lost.subscribe();
        let seen = lost.wait_for(Option::is_some).await.ok();
        or_pending(seen.and_then(|seen| seen.clone())).await
    }

    /// A stop signal for a listener that fires at `stopped` (or when its sender is dropped)
    /// or at the loss, whichever comes first: after a loss the server serves nothing more.
    pub(crate) fn or_stopped(&self, stopped: oneshot::Receiver<()>) -> oneshot::Receiver<()> {
        let (stop, either) = oneshot::channel();
        let watch = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stopped => {}
                _ = watch.wait() => {}
            }
            let _ = stop.send(());
        });
        either
    }
}
