//! The operator's calls on the cell's hosts (cordon and uncordon, through the job service's
//! route) and the cell's read of the usage history.

use std::time::Duration;

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Method, Request};
use serde_json::json;

use super::Cell;
use crate::cordon::{Cordon, Uncordon};
use crate::server::CLIENT_TOKEN;
use crate::usage::{self, UsageReadFault};

/// The longest cordon the server grants, in seconds: it cuts a longer one to this.
pub const MAX_CORDON_SECS: u32 = 3_600;

impl Cell {
    /// Cordons the host of agent `index` for `secs` seconds, as an operator does: `POST
    /// /v1/hosts/{id}:cordon` with the client token, after the cell reads the server's
    /// tables (so what is running now is read before the cordon). A cordon the server
    /// answers 200 is written down ([`crate::Observations::cordons`]), its end at the call
    /// plus `secs`, cut to [`MAX_CORDON_SECS`]. Returns the status (0 when the cell has no
    /// such agent, and nothing is called).
    pub fn cordon_host(&mut self, index: usize, secs: u32) -> u16 {
        let body = json!({ "cordonSecs": secs }).to_string();
        let (status, host, at) = self.operate(index, "cordon", body);
        let length = Duration::from_secs(u64::from(secs.min(MAX_CORDON_SECS)));
        if let (200, Some(host)) = (status, host) {
            let until = at.saturating_add(length);
            self.observations.cordons.push(Cordon { host, at, until });
        }
        status
    }

    /// Lifts the cordon of the host of agent `index`, as an operator does (`POST
    /// /v1/hosts/{id}:uncordon`); an uncordon the server answers 200 is written down
    /// ([`crate::Observations::uncordons`]). Returns the status, as [`Cell::cordon_host`].
    pub fn uncordon_host(&mut self, index: usize) -> u16 {
        let (status, host, at) = self.operate(index, "uncordon", "{}".to_owned());
        if let (200, Some(host)) = (status, host) {
            self.observations.uncordons.push(Uncordon { host, at });
        }
        status
    }

    /// One operator call `verb` on the host of agent `index` with `body`: the status, the
    /// host and the time of the call.
    fn operate(
        &mut self,
        index: usize,
        verb: &str,
        body: String,
    ) -> (u16, Option<js_types::HostId>, js_types::sched::FarmTime) {
        let host = self.agent(index).map(|agent| agent.host().clone());
        let at = self.sim.now();
        let Some(named) = &host else {
            return (0, None, at);
        };
        self.reread(true);
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/hosts/{named}:{verb}"))
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            // A host id is a path the request can carry; were it not, the empty request is
            // refused and the status says so.
            .unwrap_or_default();
        let answer = self.client_call(request);
        let status = answer.map_or(0, |answer| answer.status().as_u16());
        (status, host, at)
    }

    /// Makes the cell apply `fault` to the usage rows it reads from now on
    /// ([`UsageReadFault`]).
    pub fn set_usage_fault(&mut self, fault: UsageReadFault) {
        self.usage_fault = fault;
    }

    /// Reads the usage history of the server into the observations, with the cell's
    /// [`UsageReadFault`] applied; none when the server is down.
    pub(super) fn read_usage(&mut self) {
        let now = self.sim.now();
        let fault = self.usage_fault;
        let server = self.sim.nodes_mut().find_map(|(_, n)| n.as_server_mut());
        let rows = server.and_then(|server| usage::read(server, now));
        self.observations.usage = rows.map(|rows| fault.apply(rows));
    }
}
