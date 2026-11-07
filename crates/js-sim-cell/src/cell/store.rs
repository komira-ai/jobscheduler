//! The run loop's side of the server's store: one-time join tokens minted through the
//! operator route and the hosts that say hello with them, the server stopped between two
//! inputs with a read of the store's keys while it is down, and a second server started on
//! the same store.
//!
//! The trace says `join h<n> token <k>` when a host joins with the `k`-th token the cell
//! minted (the mint itself is the line of its client call, `client POST 201`), `stop server`
//! and then `store joins=<n> used=<n> bindings=<n>` (or `store unread`) when the server is
//! stopped, and `second server started` or `second server refused <flag>`. No line holds a
//! token, a digest or a connection string.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Method, Request};
use js_agent_wire::InstanceIdentity;
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::sched::FarmTime;
use serde_json::{Value, json};

use super::Cell;
use crate::agent::AgentKind;
use crate::invariant::Touched;
use crate::server::CLIENT_TOKEN;

/// The operator route that mints a one-time join token.
pub const JOIN_TOKENS_PATH: &str = "/v1/agent/join-tokens";

/// The provider every minted token's instance names; the instance ids are `i-0`, `i-1`,
/// ..., in the order the cell minted them.
pub const TOKEN_PROVIDER: &str = "sim";

/// The keys the server keeps a join token under, by its digest (`hosts/join/<digest>`).
pub const JOINS: &str = "hosts/join/";

/// The keys the server keeps a host's binding under (`hosts/binding/<host id>`).
pub const BINDINGS: &str = "hosts/binding/";

/// What the cell keeps of the store side of a run.
#[derive(Default)]
pub(super) struct Kept {
    /// Every token minted, in order.
    minted: Vec<Minted>,
    /// Every start of a second server, in order.
    seconds: Vec<SecondServer>,
    /// Every read of the store at a stop of the server, in order.
    reads: Vec<Result<StoreRead, String>>,
}

/// One minted token and the instance it is bound to. An empty token is one the server did
/// not mint: a hello with it is refused.
#[derive(Clone)]
struct Minted {
    instance: InstanceIdentity,
    token: String,
}

/// One start of a second server on the store ([`Cell::start_second_server`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondServer {
    /// When it was started.
    pub at: FarmTime,
    /// Its refusal (`<flag>: <reason>`), or `None` when it started; it is stopped at once.
    pub refused: Option<String>,
}

/// What the store held of the join tokens and the host bindings when the cell read it, with
/// the server down ([`Cell::stop_server`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreRead {
    /// When it was read.
    pub at: FarmTime,
    /// Each join token by its digest, with the host that used it, `None` while it is unused.
    pub joins: BTreeMap<String, Option<String>>,
    /// Each bound host, with the digest of the credential it is bound to.
    pub bindings: BTreeMap<String, String>,
}

impl StoreRead {
    /// The read of the rows under [`JOINS`] and [`BINDINGS`], at `at`. A value that does
    /// not hold the field the server writes reads as unused, or as bound to nothing.
    #[must_use]
    pub fn from_rows(
        at: FarmTime,
        joins: Vec<(String, Vec<u8>)>,
        bindings: Vec<(String, Vec<u8>)>,
    ) -> Self {
        let joins = joins.into_iter();
        let joins = joins.map(|(key, value)| (after(&key, JOINS), field(&value, "used_by")));
        let bindings = bindings.into_iter().map(|(key, value)| {
            let credential = field(&value, "credential").unwrap_or_default();
            (after(&key, BINDINGS), credential)
        });
        Self {
            at,
            joins: joins.collect(),
            bindings: bindings.collect(),
        }
    }

    /// Every way a token's use and a host's binding fail to go together: a token used by a
    /// host that has no binding to it, and a host bound to a token that it did not use. A
    /// host bound to a credential that is no token (the pool's shared join token) is no
    /// pair. Empty when every use has its binding and every binding to a token its use.
    #[must_use]
    pub fn unpaired(&self) -> Vec<String> {
        let used = self.joins.iter();
        let used = used.filter_map(|(digest, host)| Some((digest, host.as_ref()?)));
        let unbound = used.filter(|(digest, host)| self.bindings.get(*host) != Some(*digest));
        let mut found: Vec<String> = unbound
            .map(|(_, host)| format!("a token used by {host}, which has no binding to it"))
            .collect();
        let to_tokens = self.bindings.iter();
        let to_tokens = to_tokens.filter(|(_, credential)| self.joins.contains_key(*credential));
        let unused = to_tokens.filter(|(host, credential)| {
            self.joins.get(*credential) != Some(&Some((*host).clone()))
        });
        found.extend(unused.map(|(host, _)| format!("{host} is bound to a token it did not use")));
        found
    }

    /// The trace line of the read: how many tokens, how many of them used, and how many
    /// bindings.
    fn line(&self) -> String {
        let used = self.joins.values().filter(|host| host.is_some()).count();
        format!(
            "store joins={} used={used} bindings={}",
            self.joins.len(),
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

/// The token of a mint's answer.
fn token_of(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.get("token")?.as_str().map(str::to_owned)
}

impl Cell {
    /// Mints one one-time join token through the operator route (`POST
    /// /v1/agent/join-tokens`, with the client token, through [`Cell::client_call`]), bound
    /// to the instance [`TOKEN_PROVIDER`] `i-<n>`, `n` counting the tokens the cell minted.
    /// The cell keeps the token for [`Cell::join`]; it is in no trace line.
    pub fn mint_join_token(&mut self) {
        let instance = InstanceIdentity {
            provider: TOKEN_PROVIDER.to_owned(),
            instance_id: format!("i-{}", self.kept.minted.len()),
        };
        let body = json!({
            "provider": instance.provider,
            "instance_id": instance.instance_id,
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri(JOIN_TOKENS_PATH)
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap_or_default();
        let answer = self.client_call(request);
        let token = answer.and_then(|answer| token_of(answer.body()));
        let token = token.unwrap_or_default();
        self.kept.minted.push(Minted { instance, token });
    }

    /// How many join tokens the cell has minted.
    #[must_use]
    pub fn minted(&self) -> usize {
        self.kept.minted.len()
    }

    /// Adds the next agent, a correct fake, saying hello with the `token`-th join token the
    /// cell minted and naming its instance: a host of its own token. `None` for a token the
    /// cell did not mint.
    pub fn join(&mut self, token: usize) -> Option<HostId> {
        let minted = self.kept.minted.get(token).cloned()?;
        let kind = AgentKind::Fake(None);
        let host = self
            .add_as(kind, minted.token, Some(minted.instance))
            .ok()?;
        self.record(format!("join {host} token {token}"));
        Some(host)
    }

    /// Stops the server between two inputs, as a process is killed
    /// ([`crate::ServerNode::stop`]), and reads the store's join tokens and host bindings
    /// while it is down ([`Cell::store_reads`]); then reads the listing again and checks.
    /// [`Cell::restart_server`] starts it again.
    pub fn stop_server(&mut self) {
        let at = self.sim.now();
        let server = self
            .sim
            .nodes_mut()
            .find_map(|(_, node)| node.as_server_mut());
        let read: Option<Result<StoreRead, String>> = server.map(|server| {
            server.stop();
            let joins = server.stored(JOINS)?;
            let bindings = server.stored(BINDINGS)?;
            Ok(StoreRead::from_rows(at, joins, bindings))
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
    #[must_use]
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
