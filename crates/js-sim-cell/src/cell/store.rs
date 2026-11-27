//! The run loop's side of the server's store: hosts that say hello with a certificate (one
//! of them another host's), the server stopped between two inputs with a read of the
//! store's keys while it is down, and a second server started on the same store.
//!
//! The trace says `join h<n> certificate h<k>` when a host joins presenting the
//! certificate of host `h<k>`, `stop server` and then `store seated=<n> bindings=<n>` (or
//! `store unread`) when the server is stopped, and `second server started` or `second
//! server refused <flag>`. No line holds a digest or a connection string.

use std::collections::{BTreeMap, BTreeSet};

use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::sched::FarmTime;
use serde_json::Value;

use super::Cell;
use crate::agent::AgentKind;
use crate::bus::{Route, agent_name};
use crate::invariant::Touched;

/// The keys the server keeps a host's binding under (`hosts/binding/<host id>`).
pub const BINDINGS: &str = "hosts/binding/";

/// What the cell keeps of the store side of a run.
#[derive(Default)]
pub(super) struct Kept {
    /// Every start of a second server, in order.
    seconds: Vec<SecondServer>,
    /// Every read of the store at a stop of the server, in order.
    reads: Vec<Result<StoreRead, String>>,
}

/// One start of a second server on the store ([`Cell::start_second_server`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondServer {
    /// When it was started.
    pub at: FarmTime,
    /// Its refusal (`<flag>: <reason>`), or `None` when it started; it is stopped at once.
    pub refused: Option<String>,
}

/// What the store held of the host bindings when the cell read it, with the server down
/// ([`Cell::stop_server`]), and which hosts the server had welcomed by then.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreRead {
    /// When it was read.
    pub at: FarmTime,
    /// The hosts whose hello the server had answered 200 by then.
    pub seated: BTreeSet<String>,
    /// Each bound host, with the digest of the credential it is bound to.
    pub bindings: BTreeMap<String, String>,
}

impl StoreRead {
    /// The read of the rows under [`BINDINGS`] at `at`, with the hosts `seated`. A value that
    /// does not hold the field the server writes reads as bound to nothing.
    #[must_use]
    pub fn from_rows(
        at: FarmTime,
        seated: BTreeSet<String>,
        bindings: Vec<(String, Vec<u8>)>,
    ) -> Self {
        let bindings = bindings.into_iter().map(|(key, value)| {
            let credential = field(&value, "credential").unwrap_or_default();
            (after(&key, BINDINGS), credential)
        });
        Self {
            at,
            seated,
            bindings: bindings.collect(),
        }
    }

    /// Every host the server welcomed whose binding is not in the store: a welcome commits
    /// the binding with it, so the store holds one for every host that was seated. Empty when
    /// every seated host is bound.
    #[must_use]
    pub fn unbound(&self) -> Vec<String> {
        let missing = self.seated.iter();
        let missing = missing.filter(|host| !self.bindings.contains_key(*host));
        missing
            .map(|host| format!("{host} was welcomed and has no binding in the store"))
            .collect()
    }

    /// The trace line of the read: how many hosts were welcomed and how many bindings.
    fn line(&self) -> String {
        format!(
            "store seated={} bindings={}",
            self.seated.len(),
            self.bindings.len()
        )
    }
}

/// `key` after `prefix`.
fn after(key: &str, prefix: &str) -> String {
    key.strip_prefix(prefix).unwrap_or(key).to_owned()
}

/// The text field `name` of the JSON object `value`.
fn field(value: &[u8], name: &str) -> Option<String> {
    let value: Value = serde_json::from_slice(value).ok()?;
    value.get(name)?.as_str().map(str::to_owned)
}

impl Cell {
    /// Adds the next agent, a correct fake, presenting the host certificate of agent
    /// `of` (by its index) instead of its own: a host the server refuses, its hello naming
    /// another host than the certificate does. `None` when the cell has no such agent.
    pub fn join_presenting(&mut self, of: usize) -> Option<HostId> {
        let certificate = (of < self.agents).then(|| agent_name(of))?;
        let presenting = HostId::new(certificate.as_str()).ok()?;
        let host = self.add_as(AgentKind::Fake(None), Some(presenting)).ok()?;
        self.record(format!("join {host} certificate {certificate}"));
        Some(host)
    }

    /// Stops the server between two inputs, as a process is killed
    /// ([`crate::ServerNode::stop`]), and reads the store's host bindings
    /// while it is down ([`Cell::store_reads`]); then reads the listing again and checks.
    /// [`Cell::restart_server`] starts it again.
    pub fn stop_server(&mut self) {
        let at = self.sim.now();
        let agents = self.agents;
        let server = self
            .sim
            .nodes_mut()
            .find_map(|(_, node)| node.as_server_mut());
        let read: Option<Result<StoreRead, String>> = server.map(|server| {
            server.stop();
            let welcomed = |index: usize| server.answered(&agent_name(index), Route::Hello, 200);
            let seated = (0..agents).filter(|index| welcomed(*index) > 0);
            let seated = seated.map(|index| agent_name(index).to_string()).collect();
            let bindings = server.stored(BINDINGS)?;
            Ok(StoreRead::from_rows(at, seated, bindings))
        });
        let unread = |_: &String| "store unread".to_owned();
        let line = read
            .as_ref()
            .map(|read| read.as_ref().map_or_else(unread, StoreRead::line));
        self.record("stop server".to_owned());
        line.into_iter().for_each(|line| self.record(line));
        self.kept.reads.extend(read);
        self.reread(true);
        self.check(Touched::nothing().server());
    }

    /// Every read of the store at a stop of the server, in order: what it held, or why it
    /// could not be read.
    pub fn store_reads(&self) -> &[Result<StoreRead, String>] {
        &self.kept.reads
    }

    /// Starts a second server over the server's store, its files in a directory of its own,
    /// as an operator starts a second process on it ([`crate::ServerNode::start_beside`]),
    /// and stops it again at once if it started: whether it starts is the store's writer
    /// lock's to decide. Kept for [`Cell::second_servers`].
    pub fn start_second_server(&mut self) {
        let dir = TestDir::under(&self.dir, "second");
        let (at, seed, spec) = (self.sim.now(), self.seed, self.spec.clone());
        let server = self.server();
        let started = server.map(|server| server.start_beside(&spec, dir, seed).map(drop));
        let refused = started.and_then(Result::err);
        let line = refused.as_ref().map_or_else(
            || "second server started".to_owned(),
            |error| format!("second server refused {}", error.flag()),
        );
        self.record(line);
        let refused = refused.map(|error| error.to_string());
        self.kept.seconds.push(SecondServer { at, refused });
        self.check(Touched::nothing());
    }

    /// Every start of a second server, in order.
    #[must_use]
    pub fn second_servers(&self) -> &[SecondServer] {
        &self.kept.seconds
    }
}
