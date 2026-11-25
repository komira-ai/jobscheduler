//! Connections: a small pool of sessions, each with its statements prepared once.
//!
//! Every session is checked when it connects: the server must make a commit durable
//! before it acknowledges it (`synchronous_commit` on), or the store refuses to run.

use std::ops::{Bound, Deref, DerefMut};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use postgres::Statement;
use postgres::error::Severity;

use super::Fail;
use super::conn::{Conn, Dial};
use super::schema::{SchemaError, Supported, check_conn, require_durable};

/// The time of the store's clock in microseconds since the Unix epoch, as SQL: the value
/// of parameter `$n` when it is not NULL (a test's injected clock), else the database's
/// own `clock_timestamp()` (`now()` is the transaction's start time, which a long batch
/// would judge expiry by).
fn now_sql(n: u8) -> String {
    format!(
        "COALESCE(${n}::bigint, floor(extract(epoch from clock_timestamp()) * 1000000)::bigint)"
    )
}

/// The statements every session runs, prepared once per connection. Table names resolve
/// through the session's `search_path`, which is the store's schema.
pub(super) struct Statements {
    /// `$1` key, `$2` clock override: `value, version, expires_us` of a live key.
    pub get: Statement,
    /// `$1` prefix, `$2` clock override: `key, value, version, expires_us`, in key order.
    pub scan: Statement,
    /// C8, one statement per [`RangeShape`] at [`RangeShape::index`]: `$1` clock override,
    /// `$2` limit, `$3` the lower key, `$4` the upper key when that side is bounded: `key,
    /// value, version, expires_us` of the live keys in the span, in the shape's direction.
    pub ranges: Vec<Statement>,
    /// `$1` clock override: the store's time.
    pub now: Statement,
    /// The commit head, read without a lock.
    pub head_read: Statement,
    /// `$1` how many revisions to take: moves the commit head past them and returns the new
    /// head (the tail runs it under `LOCK TABLE head`).
    pub head_move: Statement,
    /// `$1` keys, in key order: inserts a placeholder row for each key with none, which
    /// reserves it, and locks the row of each key with one (`ON CONFLICT DO UPDATE ...
    /// WHERE false` locks the conflicting row and changes nothing); returns the keys it
    /// reserved.
    pub reserve: Statement,
    /// `$1` keys, `$2` clock override: `key, value, version, expires_us` and the store's
    /// time, of every one of those keys this transaction sees.
    pub load: Statement,
    /// The tail's writes in one statement: `$1` to `$4` the feed rows' revisions, keys, kinds
    /// and values; `$5` to `$8` the rows' keys, values, versions and expiries; `$9` the keys
    /// whose rows (or placeholders) go.
    pub write: Statement,
    /// `$1` counter: its new value.
    pub counter: Statement,
    /// `$1` how many: that many values of the revision sequence, in order (only the
    /// sequence flaw uses it).
    pub seq: Statement,
    /// `$1` now: the keys the clock has expired, locked.
    pub due: Statement,
    /// `$1` after, `$2` upper bound, `$3` limit: feed rows.
    pub changes: Statement,
    /// `$1` the revision to trim through: deletes the feed rows at or below the lesser of
    /// it and the committed head (every revision at or below it has committed); how many
    /// rows go.
    pub trim: Statement,
    /// The feed's floor: one below its oldest row, or the head when it has none.
    pub floor: Statement,
    /// `$1` prefix, `$2` now, `$3` limit: live rows under a prefix, locked, skipping rows
    /// another transaction holds.
    pub claim: Statement,
    /// The same without `SKIP LOCKED` (the claim flaw).
    pub claim_waiting: Statement,
    /// `$1` keys, `$2` now: the live rows of those keys, locked, skipping rows another
    /// transaction holds.
    pub claim_keys: Statement,
}

impl Statements {
    fn prepare(client: &mut Conn) -> Result<Statements, postgres::Error> {
        let now = now_sql(2);
        let claim = |tail: &str| {
            format!(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key >= $1 AND starts_with(key, $1) \
                 AND (expires_us IS NULL OR expires_us > $2) \
                 ORDER BY key LIMIT $3 FOR UPDATE{tail}"
            )
        };
        let ranges = RangeShape::ALL
            .iter()
            .map(|shape| client.prepare(&shape.sql(&now_sql(1))))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Statements {
            get: client.prepare(&format!(
                "SELECT value, version, expires_us FROM kv WHERE key = $1 \
                 AND (expires_us IS NULL OR expires_us > {now})"
            ))?,
            scan: client.prepare(&format!(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key >= $1 AND starts_with(key, $1) \
                 AND (expires_us IS NULL OR expires_us > {now}) ORDER BY key"
            ))?,
            ranges,
            now: client.prepare(&format!("SELECT {}", now_sql(1)))?,
            head_read: client.prepare("SELECT rev FROM head")?,
            head_move: client.prepare("UPDATE head SET rev = rev + $1 RETURNING rev")?,
            reserve: client.prepare(
                "INSERT INTO kv (key, value, version, expires_us) \
                 SELECT k, ''::bytea, 0, NULL FROM unnest($1::text[]) WITH ORDINALITY AS u(k, n) \
                 ORDER BY n \
                 ON CONFLICT (key) DO UPDATE SET version = kv.version WHERE false RETURNING key",
            )?,
            load: client.prepare(&format!(
                "SELECT key, value, version, expires_us, {now} FROM kv WHERE key = ANY($1::text[])"
            ))?,
            write: client.prepare(
                "WITH fed AS (INSERT INTO feed (rev, key, kind, value) \
                 SELECT * FROM unnest($1::int8[], $2::text[], $3::int2[], $4::bytea[])), \
                 put AS (INSERT INTO kv (key, value, version, expires_us) \
                 SELECT * FROM unnest($5::text[], $6::bytea[], $7::int8[], $8::int8[]) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, \
                 version = EXCLUDED.version, expires_us = EXCLUDED.expires_us), \
                 gone AS (DELETE FROM kv WHERE key = ANY($9::text[])) \
                 SELECT 1",
            )?,
            counter: client.prepare(
                "INSERT INTO counters (name, value) VALUES ($1, 1) \
                 ON CONFLICT (name) DO UPDATE SET value = counters.value + 1 RETURNING value",
            )?,
            seq: client.prepare(
                "SELECT nextval('rev_seq') FROM generate_series(1, $1::int8) ORDER BY 1",
            )?,
            due: client.prepare(
                "SELECT key FROM kv WHERE expires_us IS NOT NULL AND expires_us <= $1 \
                 ORDER BY key FOR UPDATE",
            )?,
            changes: client.prepare(
                "SELECT rev, key, kind, value FROM feed WHERE rev > $1 AND rev <= $2 \
                 ORDER BY rev LIMIT $3",
            )?,
            trim: client.prepare(
                "DELETE FROM feed WHERE rev <= least($1::bigint, (SELECT rev FROM head))",
            )?,
            floor: client.prepare(
                "SELECT coalesce((SELECT min(rev) FROM feed) - 1, (SELECT rev FROM head))",
            )?,
            claim: client.prepare(&claim(" SKIP LOCKED"))?,
            claim_waiting: client.prepare(&claim(""))?,
            claim_keys: client.prepare(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key = ANY($1::text[]) \
                 AND (expires_us IS NULL OR expires_us > $2) \
                 ORDER BY key FOR UPDATE SKIP LOCKED",
            )?,
        })
    }
}

/// How the upper side of a C8 span is bounded. (The lower side is always a key, so it
/// is only included or excluded: [`RangeShape::lower_included`].)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Included,
    Excluded,
    Open,
}

impl Edge {
    const ALL: [Edge; 3] = [Edge::Included, Edge::Excluded, Edge::Open];

    pub(super) fn of<T>(bound: &Bound<T>) -> Edge {
        match bound {
            Bound::Included(_) => Edge::Included,
            Bound::Excluded(_) => Edge::Excluded,
            Bound::Unbounded => Edge::Open,
        }
    }
}

/// The shape of a C8 statement: whether the span's lower key is included, how its upper
/// side is bounded, and which way the scan walks. Each shape has its own prepared
/// statement, so the planner sees plain range conditions on the primary key and walks
/// its index from the right end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RangeShape {
    pub lower_included: bool,
    pub upper: Edge,
    pub reverse: bool,
}

impl RangeShape {
    const ALL: [RangeShape; 12] = {
        let mut all = [RangeShape {
            lower_included: true,
            upper: Edge::Open,
            reverse: false,
        }; 12];
        let mut i = 0;
        while i < 12 {
            all[i] = RangeShape {
                lower_included: i / 6 == 0,
                upper: Edge::ALL[i / 2 % 3],
                reverse: i % 2 == 1,
            };
            i += 1;
        }
        all
    };

    /// Where this shape's statement sits in [`Statements::ranges`]: the inverse of the
    /// order of [`RangeShape::ALL`].
    pub(super) fn index(self) -> usize {
        let upper = match self.upper {
            Edge::Included => 0,
            Edge::Excluded => 1,
            Edge::Open => 2,
        };
        usize::from(!self.lower_included) * 6 + upper * 2 + usize::from(self.reverse)
    }

    fn sql(self, now: &str) -> String {
        let lower = if self.lower_included { ">=" } else { ">" };
        let mut conditions = vec![
            format!("(expires_us IS NULL OR expires_us > {now})"),
            format!("key {lower} $3"),
        ];
        let upper = match self.upper {
            Edge::Included => Some("<="),
            Edge::Excluded => Some("<"),
            Edge::Open => None,
        };
        if let Some(op) = upper {
            conditions.push(format!("key {op} $4"));
        }
        let order = if self.reverse { "DESC" } else { "ASC" };
        format!(
            "SELECT key, value, version, expires_us FROM kv WHERE {} ORDER BY key {order} LIMIT $2",
            conditions.join(" AND ")
        )
    }
}

/// One connection and its prepared statements.
pub(super) struct Session {
    pub client: Conn,
    pub st: Statements,
    /// Set when the connection is in an unknown state; the pool drops it.
    pub broken: bool,
}

/// Opens a connection. The first one of a store (`judge` is `Some`) also refuses a schema
/// outside what this build supports; later ones do not, so a serving child keeps working
/// when the installer migrates under it.
fn connect(dial: &Dial, schema: &str, judge: Option<Supported>) -> Result<Session, Fail> {
    let mut client = Conn::connect(dial)?;
    require_durable(&mut client).map_err(refuse)?;
    if let Some(supported) = judge {
        check_conn(&mut client, schema, supported).map_err(refuse)?;
    }
    client.batch_execute(&format!(
        "SET search_path TO {schema}; SET default_transaction_isolation = 'read committed'"
    ))?;
    let st = Statements::prepare(&mut client)?;
    Ok(Session {
        client,
        st,
        broken: false,
    })
}

impl Session {
    /// Records a failed call on this session. Only an error the server answered with, and
    /// that leaves the connection open (a statement refused), keeps the session fit for
    /// reuse. Any other error (a lost connection, a `FATAL` the server sent as it closed
    /// the connection) marks it broken, without asking [`Conn::is_closed`]: the client
    /// reports the error to the caller before its connection task has finished, so the
    /// flag can still read open and the pool would hand the dead session out again.
    pub(super) fn note_failure(&mut self, err: &postgres::Error) {
        let kept = err
            .as_db_error()
            .is_some_and(|db| db.parsed_severity() == Some(Severity::Error));
        if !kept {
            self.broken = true;
        }
    }
}

fn refuse(err: SchemaError) -> Fail {
    Fail::Refused(err.to_string())
}

struct PoolState {
    idle: Vec<Session>,
    /// Sessions that exist: idle, checked out, or being connected.
    open: usize,
}

/// At most `max` sessions; a caller past that waits for one to come back, for `wait` at most.
pub(super) struct Pool {
    dial: Dial,
    schema: String,
    max: usize,
    wait: Duration,
    state: Mutex<PoolState>,
    ready: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Pool {
    /// Connects once, refusing a server that does not make commits durable and a schema
    /// outside `supported`. It creates nothing: the installer owns the schema. A checkout
    /// waits `wait` at most for a session to come back.
    pub(super) fn open(
        dial: Dial,
        schema: String,
        (max, wait): (usize, Duration),
        supported: Supported,
    ) -> Result<Pool, Fail> {
        let first = connect(&dial, &schema, Some(supported))?;
        Ok(Pool {
            dial,
            schema,
            max,
            wait,
            state: Mutex::new(PoolState {
                idle: vec![first],
                open: 1,
            }),
            ready: Condvar::new(),
        })
    }

    /// A session: an idle one, or a new connection while there are fewer than `max`, or
    /// else the first to come back within `wait`. Past `wait` the call fails with
    /// [`Fail::Busy`], which says the store is busy, not broken: a caller may try again.
    pub(super) fn checkout(&self) -> Result<Pooled<'_>, Fail> {
        let mut state = lock(&self.state);
        loop {
            if let Some(session) = state.idle.pop() {
                return Ok(Pooled {
                    pool: self,
                    session: Some(session),
                });
            }
            if state.open < self.max {
                state.open += 1;
                drop(state);
                return match connect(&self.dial, &self.schema, None) {
                    Ok(session) => Ok(Pooled {
                        pool: self,
                        session: Some(session),
                    }),
                    Err(err) => {
                        self.forget_one();
                        Err(err)
                    }
                };
            }
            let (back, waited) = self
                .ready
                .wait_timeout_while(state, self.wait, |state| {
                    state.idle.is_empty() && state.open >= self.max
                })
                .unwrap_or_else(PoisonError::into_inner);
            if waited.timed_out() {
                return Err(Fail::Busy(format!(
                    "every one of the store's {} connections stayed in use for {:?}",
                    self.max, self.wait
                )));
            }
            state = back;
        }
    }

    /// How many messages carrying statements the pool's connections have sent ([`super::Conn`]).
    pub(super) fn sent(&self) -> u64 {
        self.dial.sent.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Drops every idle session. The sessions on loan are judged when they come back.
    pub(super) fn discard_idle(&self) {
        let idle = {
            let mut state = lock(&self.state);
            state.open -= state.idle.len();
            std::mem::take(&mut state.idle)
        };
        self.ready.notify_all();
        drop(idle);
    }

    fn forget_one(&self) {
        lock(&self.state).open -= 1;
        self.ready.notify_one();
    }

    fn give_back(&self, session: Session) {
        // `|`, not `||`: whether a dead session was marked or only reads closed depends on
        // when the client saw the end, so both are asked every time.
        if session.broken | session.client.is_closed() {
            self.forget_one();
        } else {
            lock(&self.state).idle.push(session);
            self.ready.notify_one();
        }
    }
}

/// A session on loan from the pool; it goes back on drop unless it is broken.
pub(super) struct Pooled<'p> {
    pool: &'p Pool,
    session: Option<Session>,
}

impl Deref for Pooled<'_> {
    type Target = Session;

    fn deref(&self) -> &Session {
        // The session is taken only in `drop`.
        self.session
            .as_ref()
            .expect("a pooled session is present until it is dropped")
    }
}

impl DerefMut for Pooled<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        self.session
            .as_mut()
            .expect("a pooled session is present until it is dropped")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        // Taken here and nowhere else: the session is present for the whole loan.
        self.session
            .take()
            .into_iter()
            .for_each(|session| self.pool.give_back(session));
    }
}

#[cfg(test)]
mod tests {
    use super::{Pool, RangeShape};
    use crate::postgres::PostgresConfig;

    // Catches an index that does not invert the order of the prepared statements, which
    // would run a scan on another shape's statement (wrong bounds or direction), and a
    // repeated shape that would leave one shape with no statement.
    #[test]
    fn each_shape_indexes_its_own_statement() {
        for (place, shape) in RangeShape::ALL.iter().enumerate() {
            assert_eq!(shape.index(), place, "{shape:?}");
        }
    }

    // Catches a checkout that hands out a session that is already broken or closed, and
    // a pool that does not give a returned session out again: the one connection a pool of
    // one holds is the same one for the second loan.
    #[test]
    fn a_loan_is_a_working_session_and_comes_back_to_the_pool() {
        let url = js_testkit::database::server_url();
        let config = PostgresConfig::parse(&url)
            .and_then(|config| config.with_schema("kbf_unit_pool"))
            .expect("a usable URL");
        postgres::Client::connect(&url, postgres::NoTls)
            .and_then(|mut client| {
                client.batch_execute("DROP SCHEMA IF EXISTS kbf_unit_pool CASCADE")
            })
            .expect("a clean schema");
        crate::postgres::schema::apply(
            &config,
            &crate::postgres::schema::embedded(),
            crate::postgres::schema::latest(),
        )
        .expect("the installer's step");
        let pool = Pool::open(
            config.dial(),
            "kbf_unit_pool".to_owned(),
            (1, config.timeouts.checkout),
            config.supported,
        )
        .expect("opens");
        let pid = |pooled: &mut super::Pooled<'_>| -> i32 {
            pooled
                .client
                .query_one("SELECT pg_backend_pid()", &[])
                .expect("pid")
                .get(0)
        };
        let mut first = pool.checkout().expect("a session");
        assert!(!first.broken);
        assert!(!first.client.is_closed());
        let first_pid = pid(&mut first);
        drop(first);
        let mut second = pool.checkout().expect("the same session again");
        assert_eq!(pid(&mut second), first_pid);
    }
}
