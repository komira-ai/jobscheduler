//! How long a connection of the store waits before it gives up, on the network and on the
//! server, so a server that went away or a connection left half open ends a call with an
//! error instead of holding it (and whatever waits on it: the scheduler's tick, a heartbeat)
//! for ever.
//!
//! The network limits are the client's socket settings: the connect timeout, TCP keepalive
//! (idle, interval and count: a peer that answers no probe is dead after idle + interval x
//! count) and `TCP_USER_TIMEOUT` (sent data unacknowledged for that long ends the connection,
//! which keepalive alone does not cover while there is data in flight). The server limits are
//! session settings set on every connection when it opens: `statement_timeout` and
//! `idle_in_transaction_session_timeout`. The migration runner takes the network limits only:
//! a migration's DDL may run as long as it needs, and a bounded run says how long it waits for
//! locks itself (`schema::apply_within`).

use std::time::Duration;

/// The limits, each `None` for none. [`Timeouts::default`] is what a configuration starts
/// with; a URL's own `connect_timeout`, `tcp_user_timeout` and `keepalives_*` values replace
/// the matching ones (a zero there is no value, so the default stays), and
/// [`super::PostgresConfig::with_timeouts`] replaces them all.
///
/// More limits may be added: a caller outside this crate starts from [`Timeouts::default`]
/// and sets the fields it wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Timeouts {
    /// How long a connect, to each host tried, may take.
    pub connect: Option<Duration>,
    /// How long sent data may go unacknowledged before the connection is closed (Linux
    /// `TCP_USER_TIMEOUT`; ignored elsewhere and on a Unix socket).
    pub tcp_user: Option<Duration>,
    /// How long a connection is idle before the first keepalive probe.
    pub keepalive_idle: Duration,
    /// The time between keepalive probes.
    pub keepalive_interval: Duration,
    /// How many unanswered probes end the connection.
    pub keepalive_retries: u32,
    /// The server's `statement_timeout`: the longest one statement runs, waits for locks
    /// included (a writer's wait for the commit head is one: past it the batch fails, and
    /// is not run again). It applies to an extension's pooled sessions too. `None` sets no
    /// limit (0).
    pub statement: Option<Duration>,
    /// The server's `idle_in_transaction_session_timeout`: the longest a session sits in an
    /// open transaction with no statement running before the server ends it. `None` sets no
    /// limit (0).
    pub idle_in_transaction: Option<Duration>,
}

impl Default for Timeouts {
    /// Ten seconds to connect (as the migration runner's bounded connect); a dead peer found
    /// within a minute (30 s idle, then three probes 10 s apart) and unacknowledged data given
    /// up after 30 s; 30 s for a statement; a minute idle in a transaction.
    fn default() -> Timeouts {
        Timeouts {
            connect: Some(Duration::from_secs(10)),
            tcp_user: Some(Duration::from_secs(30)),
            keepalive_idle: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(10),
            keepalive_retries: 3,
            statement: Some(Duration::from_secs(30)),
            idle_in_transaction: Some(Duration::from_secs(60)),
        }
    }
}

impl Timeouts {
    /// These limits with the ones `pg` was given by the URL in their place. `idle_set` says
    /// whether the URL named `keepalives_idle`, which the client cannot say (it has a default
    /// of its own in place of no value).
    pub(super) fn with_url(self, pg: &postgres::Config, idle_set: bool) -> Timeouts {
        Timeouts {
            connect: pg.get_connect_timeout().copied().or(self.connect),
            tcp_user: pg.get_tcp_user_timeout().copied().or(self.tcp_user),
            keepalive_idle: if idle_set {
                pg.get_keepalives_idle()
            } else {
                self.keepalive_idle
            },
            keepalive_interval: pg
                .get_keepalives_interval()
                .unwrap_or(self.keepalive_interval),
            keepalive_retries: pg
                .get_keepalives_retries()
                .unwrap_or(self.keepalive_retries),
            ..self
        }
    }

    /// `pg` with the network limits set. Keepalive stays on or off as the URL says
    /// (`keepalives=0` turns it off).
    pub(super) fn network(&self, mut pg: postgres::Config) -> postgres::Config {
        // The client has no way to unset a limit once set, and a fresh `Config` has none.
        if let Some(limit) = self.connect {
            pg.connect_timeout(limit);
        }
        if let Some(limit) = self.tcp_user {
            pg.tcp_user_timeout(limit);
        }
        pg.keepalives_idle(self.keepalive_idle)
            .keepalives_interval(self.keepalive_interval)
            .keepalives_retries(self.keepalive_retries);
        pg
    }

    /// The statements that set the server limits on a new session.
    pub(super) fn session_sql(&self) -> String {
        format!(
            "SET statement_timeout = {}; SET idle_in_transaction_session_timeout = {}",
            millis(self.statement),
            millis(self.idle_in_transaction)
        )
    }
}

/// A limit in whole milliseconds as the server takes it: at least 1, since 0 is none.
fn millis(limit: Option<Duration>) -> u128 {
    limit.map_or(0, |limit| limit.as_millis().max(1))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Timeouts;

    fn pg(url: &str) -> postgres::Config {
        url.parse().expect("parses")
    }

    // Catches a URL limit that is ignored (an operator's `connect_timeout` would not take)
    // and a URL without limits that loses the defaults: each URL key replaces its default,
    // and the defaults stay where the URL says nothing.
    #[test]
    fn the_urls_limits_replace_the_defaults_and_the_rest_stay() {
        let set = pg(
            "postgres://h/db?connect_timeout=3&tcp_user_timeout=4&keepalives_idle=5\
             &keepalives_interval=6&keepalives_retries=7",
        );
        let from_url = Timeouts::default().with_url(&set, true);
        assert_eq!(
            from_url,
            Timeouts {
                connect: Some(Duration::from_secs(3)),
                tcp_user: Some(Duration::from_secs(4)),
                keepalive_idle: Duration::from_secs(5),
                keepalive_interval: Duration::from_secs(6),
                keepalive_retries: 7,
                ..Timeouts::default()
            }
        );
        let none = Timeouts::default().with_url(&pg("postgres://h/db"), false);
        assert_eq!(none, Timeouts::default());
    }

    // Catches a network limit not handed to the client (a dead peer would hold a call until
    // the operating system gives up, which can be hours), and `None` that sets a limit.
    #[test]
    fn the_network_limits_go_to_the_client() {
        let limited = Timeouts::default().network(pg("postgres://h/db"));
        assert_eq!(
            limited.get_connect_timeout(),
            Some(&Duration::from_secs(10))
        );
        assert_eq!(
            limited.get_tcp_user_timeout(),
            Some(&Duration::from_secs(30))
        );
        assert_eq!(limited.get_keepalives_idle(), Duration::from_secs(30));
        assert_eq!(
            limited.get_keepalives_interval(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(limited.get_keepalives_retries(), Some(3));
        let unlimited = Timeouts {
            connect: None,
            tcp_user: None,
            ..Timeouts::default()
        }
        .network(pg("postgres://h/db"));
        assert_eq!(unlimited.get_connect_timeout(), None);
        assert_eq!(unlimited.get_tcp_user_timeout(), None);
    }

    // Catches a limit under a millisecond sent as 0, which the server takes as no limit at
    // all, and `None` sent as anything but 0.
    #[test]
    fn the_server_limits_are_whole_milliseconds_and_none_is_zero() {
        let sql = Timeouts {
            statement: Some(Duration::from_micros(10)),
            idle_in_transaction: None,
            ..Timeouts::default()
        }
        .session_sql();
        assert_eq!(
            sql,
            "SET statement_timeout = 1; SET idle_in_transaction_session_timeout = 0"
        );
        assert_eq!(
            Timeouts::default().session_sql(),
            "SET statement_timeout = 30000; SET idle_in_transaction_session_timeout = 60000"
        );
    }
}
