//! The readiness route (`--ready-listen`, `server.ready_listen`): `GET /readyz` on a
//! loopback listener of its own, plain HTTP, no token.
//!
//! The listener is bound and answering before the store's writer lock is tried, and it is
//! the last thing closed when the server stops, so a supervisor can tell the phases of the
//! process apart from outside:
//!
//! | phase | status | body |
//! |---|---|---|
//! | the server waits for another process to give up the store's writer lock | 503 | `waiting for the store's writer lock` |
//! | the lock is held and the listeners are being bound | 503 | `starting` |
//! | the server serves | 200 | `ready` |
//! | a stop was signalled, or the writer lock was lost, until the process exits | 503 | `draining` |
//!
//! A server without a store setting has no lock to wait for: it is `starting`, then `ready`.
//! Any other path answers 404.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::sync::watch;

use crate::store::LockWatch;

/// How far the start of the server has come, as the route says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Another process holds the store's writer lock and the server retries.
    Waiting,
    /// The lock is held (or there is none to take) and the listeners are not all serving yet.
    Starting,
    /// Every listener is bound and serving.
    Ready,
    /// The server is stopping, or it lost the writer lock and has stopped serving.
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

/// The phase the route answers, set by the start and the stop of the server, and read by
/// the route. A lost writer lock reads as draining whatever was set: the server has stopped
/// serving.
#[derive(Clone)]
pub(crate) struct Gate {
    phase: Arc<watch::Sender<Phase>>,
    lock_watch: LockWatch,
}

impl Gate {
    /// A gate in `first` phase, which reads the loss of the lock on `lock_watch`.
    pub(crate) fn new(lock_watch: LockWatch, first: Phase) -> Self {
        Self {
            phase: Arc::new(watch::Sender::new(first)),
            lock_watch,
        }
    }

    /// Moves to `phase`.
    pub(crate) fn set(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    /// The phase the route answers now.
    pub(crate) fn phase(&self) -> Phase {
        match self.lock_watch.lost() {
            Some(_) => Phase::Draining,
            None => *self.phase.borrow(),
        }
    }

    /// Moves to draining: the server stops.
    pub(crate) fn drain(&self) {
        self.set(Phase::Draining);
    }
}

/// The routes of the readiness listener: `/readyz` and nothing else.
pub(crate) fn router(gate: Gate) -> Router {
    Router::new().route("/readyz", get(readyz)).with_state(gate)
}

async fn readyz(State(gate): State<Gate>) -> (StatusCode, &'static str) {
    gate.phase().answer()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::store::LockLost;

    async fn get_path(app: &Router, path: &str) -> (StatusCode, String) {
        let request = Request::get(path).body(Body::empty()).unwrap();
        let reply = app.clone().oneshot(request).await.unwrap();
        let status = reply.status();
        let body = axum::body::to_bytes(reply.into_body(), 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Asserts the answer of each phase with its exact body, that the route follows the gate
    /// as it moves, that a lost writer lock reads as draining whatever phase was set, and that
    /// any other path is a 404. It catches a phase that answers another's status or body, and
    /// a lost lock still read as ready.
    #[tokio::test]
    async fn each_phase_answers_its_status_and_body_and_a_lost_lock_is_draining() {
        let watch = LockWatch::new();
        let gate = Gate::new(watch.clone(), Phase::Waiting);
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
        gate.set(Phase::Ready);
        watch.lose(LockLost::from(js_state::StoreError::Failed(
            "gone".to_owned(),
        )));
        let (got, text) = get_path(&app, "/readyz").await;
        assert_eq!((got.as_u16(), text.as_str()), (503, "draining"));
        assert_eq!(get_path(&app, "/healthz").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_path(&app, "/").await.0, StatusCode::NOT_FOUND);
    }
}
