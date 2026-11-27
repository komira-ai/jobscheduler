//! The readiness route (`--ready-listen`, `server.ready_listen`): `GET /readyz` on a
//! loopback listener of its own, plain HTTP, no token.
//!
//! The listener is bound and answering before a SQLite store's file lock is tried, and it is
//! the last thing closed when the server stops, so a supervisor can tell the phases of the
//! process apart from outside:
//!
//! | phase | status | body |
//! |---|---|---|
//! | the server waits for another process to give up the SQLite store's writer lock | 503 | `waiting for the store's writer lock` |
//! | the store is open and the listeners are being bound | 503 | `starting` |
//! | the server serves | 200 | `ready` |
//! | a stop was signalled, until the process exits | 503 | `draining` |
//!
//! A server without a store setting, or over Postgres, has no lock to wait for: it is
//! `starting`, then `ready`.
//!
//! `GET /readyz/sizes` on the same listener says whether the job sizes are taught: whether the
//! server has finished reading the ended jobs its start rebuilds the sizes' history from
//! (`crate::job_sizes`), which it does after it answers `ready`. Until then a job that books
//! nothing books its class's cold size, and a replacement starts the read over: a deploy can
//! wait for this route before it replaces the server.
//!
//! | the teaching | status | body |
//! |---|---|---|
//! | not ended (or not begun yet: the start has not opened the store) | 503 | `teaching` |
//! | every ended job read and taught | 200 | `taught` |
//! | the server keeps no job tables | 200 | `nothing to teach` |
//! | the start's read of the live allocations failed | 200 | `not taught: the live allocations were not read` |
//! | a read of the ended jobs failed | 200 | `not taught: the ended jobs were not read` |
//! | the server stopped before the ended jobs were read | 200 | `not taught: the server stopped before the ended jobs were read` |
//!
//! Only `teaching` is a 503: once the read has ended, however it ended, waiting longer teaches
//! nothing. The route answers whatever the phase of `/readyz`. Any other path answers 404.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::sync::watch;

use crate::job_sizes::Taught;

/// How far the start of the server has come, as the route says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Another process holds the SQLite store's writer lock and the server retries.
    Waiting,
    /// The store is open (or there is none) and the listeners are not all serving yet.
    Starting,
    /// Every listener is bound and serving.
    Ready,
    /// The server is stopping.
    Draining,
}

impl Phase {
    /// The status and the body the route answers in this phase.
    fn answer(self) -> (StatusCode, &'static str) {
        match self {
            Self::Waiting => (
                StatusCode::SERVICE_UNAVAILABLE,
                "waiting for the store's writer lock",
            ),
            Self::Starting => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
            Self::Ready => (StatusCode::OK, "ready"),
            Self::Draining => (StatusCode::SERVICE_UNAVAILABLE, "draining"),
        }
    }
}

/// The status and the body `/readyz/sizes` answers for how the teaching of the job sizes
/// ended, `None` while it has not.
fn sizes_answer(taught: Option<Taught>) -> (StatusCode, &'static str) {
    match taught {
        None => (StatusCode::SERVICE_UNAVAILABLE, "teaching"),
        Some(Taught::Read { .. }) => (StatusCode::OK, "taught"),
        Some(Taught::Nothing) => (StatusCode::OK, "nothing to teach"),
        Some(Taught::AllocationsUnread) => (
            StatusCode::OK,
            "not taught: the live allocations were not read",
        ),
        Some(Taught::EndedUnread) => (StatusCode::OK, "not taught: the ended jobs were not read"),
        Some(Taught::Stopped) => (
            StatusCode::OK,
            "not taught: the server stopped before the ended jobs were read",
        ),
    }
}

/// The phase the route answers, set by the start and the stop of the server, and how the
/// teaching of the job sizes ended, set once it has; both read by the routes.
#[derive(Clone)]
pub(crate) struct Gate {
    phase: Arc<watch::Sender<Phase>>,
    sizes: Arc<watch::Sender<Option<Taught>>>,
}

impl Gate {
    /// A gate in `first` phase, its sizes still teaching.
    pub(crate) fn new(first: Phase) -> Self {
        Self {
            phase: Arc::new(watch::Sender::new(first)),
            sizes: Arc::new(watch::Sender::new(None)),
        }
    }

    /// Records how the teaching of the job sizes ended.
    pub(crate) fn set_sizes(&self, taught: Taught) {
        self.sizes.send_replace(Some(taught));
    }

    /// How the teaching of the job sizes ended, `None` while it has not.
    pub(crate) fn sizes(&self) -> Option<Taught> {
        *self.sizes.borrow()
    }

    /// Moves to `phase`.
    pub(crate) fn set(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    /// The phase the route answers now.
    pub(crate) fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    /// Moves to draining: the server stops.
    pub(crate) fn drain(&self) {
        self.set(Phase::Draining);
    }
}

/// The routes of the readiness listener: `/readyz`, `/readyz/sizes` and nothing else.
pub(crate) fn router(gate: Gate) -> Router {
    Router::new()
        .route("/readyz", get(readyz))
        .route("/readyz/sizes", get(sizes))
        .with_state(gate)
}

async fn readyz(State(gate): State<Gate>) -> (StatusCode, &'static str) {
    gate.phase().answer()
}

async fn sizes(State(gate): State<Gate>) -> (StatusCode, &'static str) {
    sizes_answer(gate.sizes())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn get_path(app: &Router, path: &str) -> (StatusCode, String) {
        let request = Request::get(path).body(Body::empty()).unwrap();
        let reply = app.clone().oneshot(request).await.unwrap();
        let status = reply.status();
        let body = axum::body::to_bytes(reply.into_body(), 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Asserts the answer of each phase with its exact body, that the route follows the gate
    /// as it moves, and that any other path is a 404. It catches a phase that answers
    /// another's status or body.
    #[tokio::test]
    async fn each_phase_answers_its_status_and_body() {
        let gate = Gate::new(Phase::Waiting);
        let app = router(gate.clone());
        let phases = [
            (Phase::Waiting, 503, "waiting for the store's writer lock"),
            (Phase::Starting, 503, "starting"),
            (Phase::Ready, 200, "ready"),
            (Phase::Draining, 503, "draining"),
        ];
        assert_eq!(gate.phase(), Phase::Waiting, "a gate starts waiting");
        for (phase, status, body) in phases {
            gate.set(phase);
            let (got, text) = get_path(&app, "/readyz").await;
            assert_eq!((got.as_u16(), text.as_str()), (status, body), "{phase:?}");
        }
        assert_eq!(get_path(&app, "/healthz").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_path(&app, "/").await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            get_path(&app, "/readyz/other").await.0,
            StatusCode::NOT_FOUND
        );
    }

    /// Asserts that `/readyz/sizes` answers 503 `teaching` from the gate's first moment, through
    /// every phase of `/readyz`, until the teaching ends, and then each ending's status and
    /// exact body: 200 for every ending, since none is followed by more teaching. Catches a
    /// route that says taught before the read ended, one that follows the phase of `/readyz`
    /// (which is `ready` while the sizes still teach), and an ending that keeps a deploy
    /// waiting for a read that will never come.
    #[tokio::test]
    async fn the_sizes_route_answers_teaching_until_the_teaching_ends_then_how_it_ended() {
        let gate = Gate::new(Phase::Waiting);
        let app = router(gate.clone());
        assert_eq!(gate.sizes(), None, "a gate starts teaching");
        for phase in [
            Phase::Waiting,
            Phase::Starting,
            Phase::Ready,
            Phase::Draining,
        ] {
            gate.set(phase);
            let (got, text) = get_path(&app, "/readyz/sizes").await;
            assert_eq!(
                (got.as_u16(), text.as_str()),
                (503, "teaching"),
                "{phase:?}"
            );
        }
        gate.set(Phase::Ready);
        let endings = [
            (Taught::Read { classes: 0 }, "taught"),
            (Taught::Read { classes: 7 }, "taught"),
            (Taught::Nothing, "nothing to teach"),
            (
                Taught::AllocationsUnread,
                "not taught: the live allocations were not read",
            ),
            (
                Taught::EndedUnread,
                "not taught: the ended jobs were not read",
            ),
            (
                Taught::Stopped,
                "not taught: the server stopped before the ended jobs were read",
            ),
        ];
        for (taught, body) in endings {
            gate.set_sizes(taught);
            assert_eq!(gate.sizes(), Some(taught));
            let (got, text) = get_path(&app, "/readyz/sizes").await;
            assert_eq!((got.as_u16(), text.as_str()), (200, body), "{taught:?}");
            assert_eq!(
                get_path(&app, "/readyz").await,
                (StatusCode::OK, "ready".to_owned())
            );
        }
    }
}
