//! The Postgres backend (feature `postgres`): section 4.3 of `docs/design/architecture-v2.md`,
//! clause by clause.
//!
//! - **C1, C2.** A batch is one transaction, run read committed. Its body locks every key
//!   the batch names, in key order in one statement: a key's row is locked, and a key with
//!   no row is reserved by an uncommitted placeholder row that blocks every other writer of
//!   the key until this transaction ends. Each transaction in the batch judges its
//!   preconditions against what is locked, then writes, so what it judged holds at the
//!   commit. A deadlock between two bodies (keys a claim's decision reads one by one can be
//!   locked in any order) runs the batch again.
//! - **C3.** A counter is a row incremented in the writing transaction.
//! - **C4.** Expiry is `expires_us` against the database's `clock_timestamp()`, read when a
//!   transaction opens and again after each statement that waited for a key's lock, and by
//!   every read; a reaper (`reap_expired`) writes the `Expire` rows. A test injects a clock
//!   ([`PostgresStore::with_clock`]) and the same statements take its reading instead.
//! - **C5.** Revisions come from one head row, which a transaction takes only at its tail
//!   (`tx`): it locks the head, moves it past its writes, writes the rows and feed rows at
//!   those revisions, and commits, so revisions commit in the order they were taken. Bodies
//!   run at once; tails queue on a table lock (`SHARE UPDATE EXCLUSIVE`, the one every
//!   release takes) and hold it for a few statements and the commit's flush.
//!   [`Store::changes`] reads the committed head, then the rows up to it, and returns the
//!   head as its cursor: nothing at or below it can still commit. A bare sequence would not
//!   do (a later number can commit first and a poller steps over the earlier one);
//!   [`Flaw::SequenceRevisions`] is that mistake, kept so the suite can show it fails.
//!   Polling is the baseline; this module sends no `NOTIFY`.
//! - **C6.** `synchronous_commit` must be on: a connection where it is not is refused,
//!   so an acknowledged write has been flushed.
//! - **C7.** A batch is one transaction and one commit, however many transactions it
//!   holds.
//!
//! [`ExtensionPool`] is for a crate that keeps tables of its own in the schema:
//! connections of their own, outside the commit head, taking none of the locks above.
//! Such a crate registers its migration with [`schema::extend`].
//!
//! There is no writer lock and no writer generation: any number of processes open the same
//! schema and write at once, and each commit is correct on its own (the head orders the
//! commits, every row a transaction changes carries a version it must still find).
//! [`serving::probe`] says whether a process of a given application name is connected, for
//! the update hook; it takes no lock.
//!
//! [`PostgresStore::claim`] is the claim path: rows locked `FOR UPDATE SKIP LOCKED`, each
//! claimed through the same version compare-and-set.
//!
//! A call waits at most [`Timeouts::checkout`] for a connection of the pool, and fails with
//! [`StoreError::Unavailable`] past it; a statement the server ends at its
//! `statement_timeout` or `lock_timeout` is [`StoreError::Unavailable`] too.
//!
//! **Connections.** Every connection is made with the configuration's TLS mode
//! ([`TlsMode`]: `sslmode`, with the root certificate `sslrootcert` names) and its limits
//! ([`Timeouts`]: connect, keepalive, `TCP_USER_TIMEOUT`, and the server's
//! `statement_timeout` and `idle_in_transaction_session_timeout`). A call whose connection is
//! gone (closed, broken under it, or ended by the server) is run once more on a new
//! connection, and fails as [`StoreError::Disconnected`] when that one is gone too. A
//! transaction is run again only when its connection was gone before it began: one under way
//! is rolled back by the server and reported, not run again, since its commit may have
//! happened.
//!
//! The feed table grows until [`Store::trim_feed`] trims it (the server's job pass calls
//! it); a trim deletes only at or below the committed head, so it never removes a row that
//! can still commit behind it. A read below the feed's floor is
//! refused with [`StoreError::StaleCursor`]; the floor is derived, not stored (one below
//! the oldest feed row, or the head when the feed is empty), which holds because every
//! revision takes exactly one feed row. [`Flaw::SequenceRevisions`] revisions are not
//! contiguous, so it neither trims nor checks.
//!
//! Limits: calls block, so an async caller uses a blocking task.

#[cfg(any(test, feature = "test-support"))]
mod backend;
pub mod boot;
mod claim;
mod conn;
#[cfg(feature = "test-database")]
mod database;
mod extension;
mod not_up;
mod reach;
pub mod schema;
pub mod serving;
mod session;
mod timeouts;
mod tls;
mod tx;
mod url;

use std::fmt;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use postgres::error::SqlState;
use postgres::types::ToSql;

#[cfg(any(test, feature = "test-support"))]
pub use backend::PostgresBackend;
pub use conn::Conn;
#[cfg(feature = "test-database")]
pub use database::{TestDatabase, test_database, test_database_with};
pub use extension::{ExtensionPool, Loan, store_error};
pub use not_up::OpenError;
pub use reach::{Unreached, reach};
use session::{Edge, Pool, Pooled, RangeShape, Session};
pub use timeouts::Timeouts;
use tls::Tls;
pub use tls::{TlsError, TlsMode};
use tx::{Row, Tx, keys_of, micros};

use conn::{Dial, DialError, connection_lost};

use crate::clock::{Clock, StoreTime};
use crate::store::{Claim, Span, Store, TxnRead, page_of};
use crate::{
    Change, Changes, Commit, CommitStats, Conflict, Direction, Event, Revision, ScanPage,
    ScanRange, StoreError, Txn, Version, Versioned,
};

/// The most times a batch is run again after a serialization failure.
const MAX_ATTEMPTS: usize = 200;

/// A deliberate defect, to show the suite can fail on a Postgres store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flaw {
    /// A correct store.
    #[default]
    None,
    /// C5: revisions come from a sequence, outside the transaction, and the feed
    /// cursor is the last row seen, so a later revision that commits first makes the
    /// poller step over the earlier one.
    SequenceRevisions,
    /// The claim waits for a row another transaction holds instead of skipping it.
    ClaimBlocks,
}

/// Why a connection string is not usable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No URL was given.
    #[error("the postgres state store needs a URL (postgres://...)")]
    Missing,
    /// The URL is not a `postgres://` or `postgresql://` URL.
    #[error("the state store URL must start with postgres:// or postgresql://")]
    NotPostgres,
    /// The URL names no host.
    #[error("the state store URL names no host")]
    NoHost,
    /// A TLS mode that checks the server's certificate (`verify-ca`, `verify-full`) was asked
    /// for with no root certificate to check it against.
    #[error(
        "the state store's sslmode verify-ca and verify-full need a root certificate (sslrootcert)"
    )]
    NoRootCert,
    /// The root certificate file cannot be read or holds no certificate. The text names the
    /// file, which is a path of the configuration's own and never the URL.
    #[error("the state store's root certificate is not usable: {0}")]
    RootCert(String),
    /// The URL does not parse.
    #[error("the state store URL is not valid: {0}")]
    Invalid(String),
    /// The schema name is not lower-case letters, digits and underscores.
    #[error(
        "the state store schema {0:?} must be 1 to 63 characters of a-z, 0-9 and _, not starting with a digit"
    )]
    BadSchema(String),
}

/// Where the store is and how it connects.
#[derive(Clone)]
pub struct PostgresConfig {
    pg: postgres::Config,
    schema: String,
    max_connections: usize,
    supported: schema::Supported,
    tls: Tls,
    timeouts: Timeouts,
}

impl fmt::Debug for PostgresConfig {
    // The connection string carries a password; leave it out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("schema", &self.schema)
            .field("max_connections", &self.max_connections)
            .field("tls", &self.tls.mode())
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl PostgresConfig {
    /// The schema a store lives in unless told otherwise.
    pub const DEFAULT_SCHEMA: &'static str = "kbf_state";

    /// The most connections a store opens unless told otherwise. Transactions run their
    /// bodies at once and queue only for the commit head at their tails.
    pub const DEFAULT_MAX_CONNECTIONS: usize = 8;

    /// Reads a `postgres://` URL. Its `sslmode` takes every value libpq does but `allow`
    /// ([`TlsMode`]; none is `disable`), and `sslrootcert` names the PEM file of the root
    /// certificates a `verify` mode trusts. Its `connect_timeout`, `tcp_user_timeout` and
    /// `keepalives_*` replace those of [`Timeouts::default`].
    ///
    /// # Errors
    /// [`ConfigError`] when the URL is missing, is not a Postgres URL, names no host, asks
    /// for a `verify` mode with no root certificate, or names one that cannot be read.
    pub fn parse(url: &str) -> Result<PostgresConfig, ConfigError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(ConfigError::Missing);
        }
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(ConfigError::NotPostgres);
        }
        let query = url::split(url)?;
        let mut pg: postgres::Config = query
            .url
            .parse()
            .map_err(|err: postgres::Error| ConfigError::Invalid(err.to_string()))?;
        if pg.get_hosts().is_empty() {
            return Err(ConfigError::NoHost);
        }
        if pg.get_application_name().is_none() {
            pg.application_name("kbf-state");
        }
        pg.ssl_mode(query.mode.ssl_mode());
        Ok(PostgresConfig {
            schema: Self::DEFAULT_SCHEMA.to_owned(),
            max_connections: Self::DEFAULT_MAX_CONNECTIONS,
            supported: schema::Supported::this_build(),
            tls: Tls::new(query.mode, query.root.as_deref())?,
            timeouts: Timeouts::default().with_url(&pg, query.idle_set),
            pg,
        })
    }

    /// Connects with TLS as `mode` says, trusting the certificates of the PEM file `root` when
    /// one is given (a `verify` mode needs one; the others do not read it). It replaces what
    /// the URL said.
    ///
    /// # Errors
    /// [`ConfigError::NoRootCert`] for a `verify` mode without `root`, [`ConfigError::RootCert`]
    /// when the file cannot be read or holds no certificate.
    pub fn with_tls(
        mut self,
        mode: TlsMode,
        root: Option<&Path>,
    ) -> Result<PostgresConfig, ConfigError> {
        self.tls = Tls::new(mode, root)?;
        self.pg.ssl_mode(mode.ssl_mode());
        Ok(self)
    }

    /// The TLS mode connections are made with.
    #[must_use]
    pub fn tls_mode(&self) -> TlsMode {
        self.tls.mode()
    }

    /// Replaces every limit with those of `timeouts`. A limit the URL set stays set where
    /// `timeouts` has `None` for it: the client cannot unset one.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> PostgresConfig {
        self.timeouts = timeouts;
        self
    }

    /// The limits connections are made with.
    #[must_use]
    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// How the store's own sessions, the writer lock's and the schema checks connect: TLS,
    /// the network limits, and the server limits set on each new session.
    fn dial(&self) -> Dial {
        Dial {
            session: self.timeouts.session_sql(),
            ..self.dial_ddl()
        }
    }

    /// How the migration runner connects: TLS and the network limits, and no server limits,
    /// so a migration's DDL is not cut off.
    fn dial_ddl(&self) -> Dial {
        Dial {
            pg: self.timeouts.network(self.pg.clone()),
            tls: self.tls.clone(),
            session: String::new(),
            sent: Arc::default(),
        }
    }

    /// Keeps the store's tables in `schema`.
    ///
    /// # Errors
    /// [`ConfigError::BadSchema`] when the name is not plain lower case.
    pub fn with_schema(mut self, schema: &str) -> Result<PostgresConfig, ConfigError> {
        let plain = !schema.is_empty()
            && schema.len() <= 63
            && !schema.starts_with(|c: char| c.is_ascii_digit())
            && schema
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !plain {
            return Err(ConfigError::BadSchema(schema.to_owned()));
        }
        self.schema = schema.to_owned();
        Ok(self)
    }

    /// Makes the store accept schema versions `min` to `max` instead of what this build
    /// embeds. For tests that play another release.
    #[doc(hidden)]
    #[must_use]
    pub fn with_supported_schema(mut self, min: u32, max: u32) -> PostgresConfig {
        self.supported = schema::Supported { min, max };
        self
    }

    /// Names the connections in `pg_stat_activity`.
    #[must_use]
    pub fn with_application_name(mut self, name: &str) -> PostgresConfig {
        self.pg.application_name(name);
        self
    }

    /// Opens at most `n` connections (at least one).
    #[must_use]
    pub fn with_max_connections(mut self, n: usize) -> PostgresConfig {
        self.max_connections = n.max(1);
        self
    }

    /// Whether the URL carries a password (user information or a `password` parameter).
    #[must_use]
    pub fn has_password(&self) -> bool {
        self.pg.get_password().is_some()
    }

    /// The role the URL names (`user=` or the user part of the authority).
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.pg.get_user()
    }

    /// The database the URL names.
    #[must_use]
    pub fn dbname(&self) -> Option<&str> {
        self.pg.get_dbname()
    }

    /// The directory of the first host when it is a Unix socket directory (`host=/run/...`),
    /// `None` when the first host is a name or address.
    #[must_use]
    pub fn socket_dir(&self) -> Option<&std::path::Path> {
        match self.pg.get_hosts().first() {
            Some(postgres::config::Host::Unix(dir)) => Some(dir.as_path()),
            _ => None,
        }
    }

    /// How many hosts the URL names (a list after `host=`, or comma-separated in the
    /// authority): each is a server the client may try.
    #[must_use]
    pub fn host_count(&self) -> usize {
        self.pg.get_hosts().len()
    }

    /// How many ports the URL names.
    #[must_use]
    pub fn port_count(&self) -> usize {
        self.pg.get_ports().len()
    }

    /// The port the URL names, `None` for the server's default.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.pg.get_ports().first().copied()
    }

    /// The schema the store's tables are in.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }
}

/// A failure inside the store, before it is reported as a [`StoreError`].
#[derive(Debug)]
pub(crate) enum Fail {
    Pg(postgres::Error),
    Refused(String),
    Tls(TlsError),
    /// No connection of the pool came free in time; the text says so.
    Busy(String),
}

impl From<postgres::Error> for Fail {
    fn from(err: postgres::Error) -> Fail {
        Fail::Pg(err)
    }
}

impl From<DialError> for Fail {
    fn from(err: DialError) -> Fail {
        match err {
            DialError::Pg(err) => Fail::Pg(err),
            DialError::Tls(err) => Fail::Tls(err),
        }
    }
}

impl Fail {
    /// A transaction the server rolled back for a reason that running it again cures.
    fn retryable(&self) -> bool {
        match self {
            Fail::Pg(err) => err.code().is_some_and(|code| {
                *code == SqlState::T_R_SERIALIZATION_FAILURE
                    || *code == SqlState::T_R_DEADLOCK_DETECTED
            }),
            Fail::Refused(_) | Fail::Tls(_) | Fail::Busy(_) => false,
        }
    }

    /// Whether the connection was gone: the call can be run again on a new one.
    fn lost(&self) -> bool {
        matches!(self, Fail::Pg(err) if connection_lost(err))
    }

    /// The failure of a call on an open store: a connection that was gone is
    /// [`StoreError::Disconnected`].
    fn into_store(self) -> StoreError {
        self.into_error(true)
    }

    /// The failure to open a store or an extension's pool, or of an extension's call: every
    /// failure is [`StoreError::Failed`].
    fn into_failed(self) -> StoreError {
        self.into_error(false)
    }

    fn into_error(self, call: bool) -> StoreError {
        match self {
            Fail::Pg(err) if call && connection_lost(&err) => {
                StoreError::Disconnected(describe(&err))
            }
            // A statement the server ended at its time limit, or that gave up waiting for a
            // lock: the store is busy, the transaction rolled back.
            Fail::Pg(err)
                if err.code().is_some_and(|code| {
                    *code == SqlState::QUERY_CANCELED || *code == SqlState::LOCK_NOT_AVAILABLE
                }) =>
            {
                StoreError::Unavailable(describe(&err))
            }
            Fail::Busy(why) => StoreError::Unavailable(why),
            Fail::Pg(err) => StoreError::Failed(describe(&err)),
            Fail::Refused(why) => StoreError::Failed(why),
            Fail::Tls(err) => StoreError::Failed(err.to_string()),
        }
    }
}

/// A row claimed by [`PostgresStore::claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The key.
    pub key: String,
    /// The value as it was when claimed.
    pub was: Versioned,
    /// The commit of the transaction the caller built for it.
    pub commit: Commit,
}

/// A failed client call, as an extension's call reports it.
fn failed(err: postgres::Error) -> StoreError {
    Fail::Pg(err).into_failed()
}

/// What the server said, which `Display` for the client's error leaves out ("db error").
fn describe(err: &postgres::Error) -> String {
    if let Some(db) = err.as_db_error() {
        return format!(
            "{}: {} (SQLSTATE {})",
            db.severity(),
            db.message(),
            db.code().code()
        );
    }
    match std::error::Error::source(err) {
        Some(source) => format!("{err}: {source}"),
        None => err.to_string(),
    }
}

/// How many of `results` applied (the rest met a conflict).
fn applied(results: &[Result<Commit, Conflict>]) -> usize {
    results.iter().filter(|result| result.is_ok()).count()
}

fn to_time(us: i64) -> StoreTime {
    StoreTime::ZERO.saturating_add(Duration::from_micros(u64::try_from(us).unwrap_or(0)))
}

fn from_time(time: StoreTime) -> i64 {
    micros(time.since(StoreTime::ZERO))
}

fn versioned(row: &Row) -> Versioned {
    Versioned {
        value: row.value.clone(),
        version: Version(u64::try_from(row.version).unwrap_or(0)),
        expires_at: row.expires_us.map(to_time),
    }
}

/// Called when a call is run again on a new connection, after the connection is made and
/// before the call runs on it. Test support ([`PostgresStore::with_reconnect_hook`]).
type Hook = Arc<dyn Fn() + Send + Sync>;

/// The state store on Postgres.
pub struct PostgresStore {
    pool: Pool,
    config: PostgresConfig,
    clock: Option<Arc<dyn Clock>>,
    flaw: Flaw,
    reconnected: Option<Hook>,
    txns: AtomicU64,
    syncs: AtomicU64,
    retries: AtomicU64,
}

/// A batch run in a database transaction that is not committed yet. Test support: it is
/// how a test holds one commit back while another goes through.
#[doc(hidden)]
pub struct Pending<'s> {
    store: &'s PostgresStore,
    tx: Tx<'s>,
    results: Vec<Result<Commit, Conflict>>,
}

impl Pending<'_> {
    /// Commits, and returns the result of each transaction.
    ///
    /// # Errors
    /// [`StoreError`] when the commit fails.
    pub fn commit(self) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let Pending { store, tx, results } = self;
        store
            .commit(tx, applied(&results))
            .map_err(Fail::into_store)?;
        Ok(results)
    }
}

impl PostgresStore {
    /// Connects, checks that the server makes commits durable and that the schema is one
    /// this build supports ([`schema`]). It creates nothing: `kbf-installer` owns the
    /// schema. Expiry is judged by the database's clock.
    ///
    /// # Errors
    /// [`StoreError`] when the server cannot be reached, does not flush commits
    /// (`synchronous_commit` is not on), or holds no schema or one outside the supported
    /// range (one line saying which and what to run).
    pub fn open(config: &PostgresConfig) -> Result<PostgresStore, StoreError> {
        Self::try_open(config).map_err(OpenError::into_store)
    }

    /// [`PostgresStore::open`], with a failure told by what a caller does about it: a server
    /// that is not up yet ([`OpenError::NotUp`]) can be waited for, any other
    /// ([`OpenError::Refused`]) cannot.
    ///
    /// # Errors
    /// [`OpenError`], as for [`PostgresStore::open`].
    pub fn try_open(config: &PostgresConfig) -> Result<PostgresStore, OpenError> {
        let pool = Pool::open(
            config.dial(),
            config.schema.clone(),
            (config.max_connections, config.timeouts.checkout),
            config.supported,
        )
        .map_err(OpenError::from)?;
        Ok(PostgresStore {
            pool,
            config: config.clone(),
            clock: None,
            flaw: Flaw::None,
            reconnected: None,
            txns: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        })
    }

    /// Judges expiry by `clock` instead of the database's. For tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> PostgresStore {
        self.clock = Some(clock);
        self
    }

    /// Plants a deliberate defect. For tests.
    #[must_use]
    pub fn with_flaw(mut self, flaw: Flaw) -> PostgresStore {
        self.flaw = flaw;
        self
    }

    /// Calls `hook` each time a call is run again on a new connection, once the connection is
    /// made and before the call runs. For tests: a hook that ends the new connection shows
    /// what a second loss does.
    #[doc(hidden)]
    #[must_use]
    pub fn with_reconnect_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> PostgresStore {
        self.reconnected = Some(hook);
        self
    }

    /// How many times a batch, a claim or a decided claim has been run again because the
    /// server rolled it back for a serialization failure or a deadlock. A writer that waits
    /// its turn for a key or for the commit head costs none.
    #[must_use]
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }

    /// How many messages carrying statements this store's connections have sent to the server,
    /// each a round trip: a read is one, and a transaction one for each of its statements, its
    /// `BEGIN` and its `COMMIT` included (several statements sent as one text count once). A
    /// new connection's own setup counts too; a statement's prepare does not.
    #[must_use]
    pub fn statements(&self) -> u64 {
        self.pool.sent()
    }

    /// The configuration this store was opened with.
    #[must_use]
    pub fn config(&self) -> &PostgresConfig {
        &self.config
    }

    /// Opens the same database again with the same clock, flaw and hook,
    /// dropping this store's connections: a crash of the frontend, for a test.
    ///
    /// # Errors
    /// [`StoreError`] as for [`PostgresStore::open`].
    pub fn reopen(self) -> Result<PostgresStore, StoreError> {
        let mut store = PostgresStore::open(&self.config)?.with_flaw(self.flaw);
        store.clock = self.clock;
        store.reconnected = self.reconnected;
        Ok(store)
    }

    fn over(&self) -> Option<i64> {
        self.clock.as_ref().map(|clock| from_time(clock.now()))
    }

    /// Runs `call` on a pooled session, and once more on a new connection when the first
    /// failed because its connection was gone.
    fn twice<'s, T>(
        &'s self,
        mut call: impl FnMut(Pooled<'s>) -> Result<T, Fail>,
    ) -> Result<T, Fail> {
        match self.pool.checkout().and_then(&mut call) {
            Err(fail) if fail.lost() => self.reconnect().and_then(call),
            done => done,
        }
    }

    /// A session on a new connection, for a call run again: the idle sessions are dropped
    /// first, since what ended one connection (a server restart, a failover, an
    /// administrator) most likely ended them all.
    fn reconnect(&self) -> Result<Pooled<'_>, Fail> {
        self.pool.discard_idle();
        let session = self.pool.checkout();
        if let Some(hook) = &self.reconnected {
            hook();
        }
        session
    }

    fn read<T>(
        &self,
        mut f: impl FnMut(&mut Session) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        self.twice(|mut session| {
            f(&mut session).map_err(|err| {
                session.note_failure(&err);
                Fail::Pg(err)
            })
        })
        .map_err(Fail::into_store)
    }

    /// Runs `attempt` until it is not rolled back for a serialization failure.
    fn retrying<T>(&self, mut attempt: impl FnMut() -> Result<T, Fail>) -> Result<T, StoreError> {
        for _ in 0..MAX_ATTEMPTS {
            match attempt() {
                Ok(done) => return Ok(done),
                Err(fail) if fail.retryable() => {
                    self.retries.fetch_add(1, Ordering::Relaxed);
                }
                Err(fail) => return Err(fail.into_store()),
            }
        }
        Err(StoreError::Failed(format!(
            "the transaction failed to serialize {MAX_ATTEMPTS} times"
        )))
    }

    /// Opens a transaction. One whose connection was gone before it began is begun again on
    /// a new connection; nothing was written on the old one.
    fn start(&self) -> Result<Tx<'_>, Fail> {
        let over = self.over();
        self.twice(|session| Tx::begin(session, over, self.flaw))
    }

    /// Runs `batch` to the point of commit: its keys locked, its transactions applied, and its
    /// tail run, so the transaction holds the commit head when it wrote. The results carry
    /// their revisions.
    fn run_batch(&self, batch: &[Txn]) -> Result<(Tx<'_>, Vec<Result<Commit, Conflict>>), Fail> {
        let mut tx = self.start()?;
        tx.load(&keys_of(batch))?;
        let mut results = Vec::with_capacity(batch.len());
        for txn in batch {
            results.push(tx.apply(txn)?);
        }
        let revs = tx.flush()?;
        let results = results
            .into_iter()
            .map(|result| result.map(|commit| revs.settle(commit)))
            .collect();
        Ok((tx, results))
    }

    /// Commits `tx`, whose tail has run, counting `applied` transactions and, when it made
    /// anything durable, one sync.
    fn commit(&self, tx: Tx<'_>, applied: usize) -> Result<(), Fail> {
        let wrote = tx.dirty();
        tx.commit()?;
        self.txns.fetch_add(applied as u64, Ordering::Relaxed);
        self.syncs.fetch_add(u64::from(wrote), Ordering::Relaxed);
        Ok(())
    }

    /// Runs `txn` to the point of commit and holds it there: when it writes, it holds the
    /// commit head. Test support.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails.
    #[doc(hidden)]
    pub fn begin(&self, txn: Txn) -> Result<Pending<'_>, StoreError> {
        let batch = [txn];
        let (tx, results) = self.retrying(|| self.run_batch(&batch))?;
        Ok(Pending {
            store: self,
            tx,
            results,
        })
    }
}

impl Store for PostgresStore {
    fn head(&self) -> Result<Option<Revision>, StoreError> {
        if self.flaw == Flaw::SequenceRevisions {
            return Ok(None);
        }
        let head: i64 = self.read(|s| Ok(s.client.query_one(&s.st.head_read, &[])?.get(0)))?;
        Ok(Some(Revision(u64::try_from(head).unwrap_or(0))))
    }

    fn claim_decide(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, StoreError> {
        self.claim_decide_retrying(claim, decide)
    }

    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        self.retrying(|| {
            let (tx, results) = self.run_batch(&batch)?;
            self.commit(tx, applied(&results))?;
            Ok(results)
        })
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let over = self.over();
        self.read(|s| {
            let row = s.client.query_opt(&s.st.get, &[&key, &over])?;
            Ok(row.map(|row| versioned(&Row::from_row(&row, 0))))
        })
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let over = self.over();
        self.read(|s| {
            let rows = s.client.query(&s.st.scan, &[&prefix, &over])?;
            Ok(rows
                .iter()
                .map(|row| (row.get(0), versioned(&Row::from_row(row, 1))))
                .collect())
        })
    }

    /// C8 in one statement, so the page is one snapshot of committed transactions. The
    /// statement for the span's shape asks for one row past the limit to learn whether a
    /// resume token is due.
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        let Some(span) = Span::of(&scan) else {
            return Ok(ScanPage {
                rows: Vec::new(),
                resume: None,
            });
        };
        let shape = RangeShape {
            lower_included: span.lower.included,
            upper: Edge::of(&span.upper),
            reverse: scan.direction == Direction::Reverse,
        };
        let over = self.over();
        let take = i64::try_from(scan.limit.get().saturating_add(1)).unwrap_or(i64::MAX);
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&over, &take, &span.lower.key];
        if let Bound::Included(key) | Bound::Excluded(key) = &span.upper {
            params.push(key);
        }
        let rows = self.read(|s| s.client.query(&s.st.ranges[shape.index()], &params))?;
        let rows = rows
            .iter()
            .map(|row| (row.get(0), versioned(&Row::from_row(row, 1))))
            .collect();
        Ok(page_of(rows, scan.limit))
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        let sequence = self.flaw == Flaw::SequenceRevisions;
        let after_i = i64::try_from(after.0).unwrap_or(i64::MAX);
        let take = i64::try_from(limit).unwrap_or(i64::MAX);
        let read = self.read(|s| {
            // The head first: every revision up to it has committed, so the rows read
            // next include all of them.
            let head: i64 = if sequence {
                i64::MAX
            } else {
                s.client.query_one(&s.st.head_read, &[])?.get(0)
            };
            let rows = s.client.query(&s.st.changes, &[&after_i, &head, &take])?;
            // The floor after the rows: a trim that commits between the two can only
            // make this read stale, never let it return a feed with a hole.
            let floor: i64 = if sequence {
                0
            } else {
                s.client.query_one(&s.st.floor, &[])?.get(0)
            };
            Ok((head, rows, floor))
        });
        let (head, rows, floor) = read?;
        if after_i < floor {
            return Err(StoreError::StaleCursor {
                after,
                floor: Revision(u64::try_from(floor).unwrap_or(0)),
            });
        }
        let mut out = Vec::with_capacity(rows.len());
        let mut last = after;
        for row in &rows {
            let rev: i64 = row.get(0);
            let revision = Revision(u64::try_from(rev).unwrap_or(0));
            last = revision;
            let kind: i16 = row.get(2);
            let value: Option<Vec<u8>> = row.get(3);
            let event = match kind {
                0 => Event::Put(value.unwrap_or_default()),
                1 => Event::Delete,
                2 => Event::Expire,
                other => {
                    // A newer release wrote it: skip it, and the cursor still passes it.
                    tracing::warn!("skipped feed row {revision:?} of unknown kind {other}");
                    continue;
                }
            };
            out.push(Change {
                revision,
                key: row.get(1),
                event,
            });
        }
        // A page that is not full has reached the head; a full one resumes after its
        // last row read, a skipped one included.
        let cursor = if rows.len() < limit && !sequence {
            Revision(u64::try_from(head).unwrap_or(0)).max(after)
        } else {
            last
        };
        Ok(Changes { rows: out, cursor })
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        let before = i64::try_from(before.0).unwrap_or(i64::MAX);
        self.retrying(|| {
            let mut tx = self.start()?;
            let removed = tx.trim(before)?;
            self.commit(tx, 0)?;
            Ok(removed)
        })
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.retrying(|| {
            let mut tx = self.start()?;
            let reaped = tx.reap()?;
            tx.flush()?;
            self.commit(tx, 0)?;
            Ok(reaped)
        })
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats {
            txns: self.txns.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
        }
    }

    fn now(&self) -> Result<StoreTime, StoreError> {
        if let Some(clock) = &self.clock {
            return Ok(clock.now());
        }
        // The database's clock, read as every other call reads: a database that cannot be
        // reached is the call's error, never a reading of the origin.
        self.read(|s| {
            let us: i64 = s.client.query_one(&s.st.now, &[&None::<i64>])?.get(0);
            Ok(to_time(us))
        })
    }
}

#[cfg(test)]
mod tests;
