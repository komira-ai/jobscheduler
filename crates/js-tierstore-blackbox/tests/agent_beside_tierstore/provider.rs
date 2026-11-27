//! The proxy on tierstore's mount-provider socket: an HTTP/2 server on the socket the agent's
//! `--mount-provider` names, which passes each gRPC call on to tierstore's own socket unchanged
//! and records its method and when it came. A call that cannot reach tierstore (its socket is
//! gone while it restarts) is answered `UNAVAILABLE`, as the agent counts a refused connection,
//! and recorded as not reached.

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{UnixListener, UnixStream};
use tokio::runtime::Handle;

/// How long the test waits for the calls it expects.
const CALLS_LIMIT: Duration = Duration::from_secs(15 * 60);

/// One call the agent made.
#[derive(Clone, Debug)]
pub struct Call {
    /// The method: `Prepare`, `Mount`, `Release` or `Sweep`.
    pub method: String,
    /// When it came.
    pub at: Instant,
    /// Whether it was passed on to tierstore.
    pub reached: bool,
}

/// The proxy and what it recorded.
pub struct Provider {
    calls: Arc<Mutex<Vec<Call>>>,
}

/// A body of either kind the proxy answers with.
type Body = BoxBody<Bytes, hyper::Error>;

impl Provider {
    /// The proxy on `listen` (mode 0600, root's), passing calls on to `tierstore`, on `runtime`.
    pub fn start(runtime: &Handle, listen: &Path, tierstore: &Path) -> Provider {
        let _ = std::fs::remove_file(listen);
        let listener = {
            let _entered = runtime.enter();
            UnixListener::bind(listen).unwrap_or_else(|e| panic!("bind {}: {e}", listen.display()))
        };
        std::fs::set_permissions(listen, std::fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("chmod {}: {e}", listen.display()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let tierstore = tierstore.to_path_buf();
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (calls, tierstore) = (Arc::clone(&recorded), tierstore.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (calls, tierstore) = (Arc::clone(&calls), tierstore.clone());
                        async move { Ok::<_, Infallible>(pass_on(request, &tierstore, &calls).await) }
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Provider { calls }
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// When each call of `method` that reached tierstore came, in order.
    pub fn reached(&self, method: &str) -> Vec<Instant> {
        let mut reached: Vec<Instant> = self
            .calls()
            .into_iter()
            .filter(|call| call.method == method && call.reached)
            .map(|call| call.at)
            .collect();
        reached.sort();
        reached
    }

    /// Waits until `count` calls of `method` have reached tierstore and returns when the first
    /// came; the test fails if they do not come within [`CALLS_LIMIT`].
    pub fn wait_for_calls(&self, method: &str, count: usize) -> Instant {
        let limit = Instant::now() + CALLS_LIMIT;
        loop {
            let reached = self.reached(method);
            if reached.len() >= count {
                return reached[0];
            }
            assert!(
                Instant::now() < limit,
                "{} {method} calls reached tierstore in {} s, not {count}:\n{}",
                reached.len(),
                CALLS_LIMIT.as_secs(),
                crate::host::journal()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Passes `request` on to tierstore's socket and returns its answer, recording the call.
async fn pass_on(
    request: Request<Incoming>,
    tierstore: &Path,
    calls: &Mutex<Vec<Call>>,
) -> Response<Body> {
    let method = request
        .uri()
        .path()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let at = Instant::now();
    let record = |reached: bool| {
        calls.lock().unwrap_or_else(|e| e.into_inner()).push(Call {
            method: method.clone(),
            at,
            reached,
        });
    };
    let stream = match UnixStream::connect(tierstore).await {
        Ok(stream) => stream,
        Err(e) => {
            record(false);
            return unavailable(&format!("connect {}: {e}", tierstore.display()));
        }
    };
    let (mut sender, connection) =
        match hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                record(false);
                return unavailable(&format!("HTTP/2 to tierstore: {e}"));
            }
        };
    tokio::spawn(connection);
    record(true);
    match sender.send_request(request).await {
        Ok(answer) => answer.map(BodyExt::boxed),
        Err(e) => unavailable(&format!("the call to tierstore: {e}")),
    }
}

/// A gRPC answer `UNAVAILABLE` saying `why`, in headers only.
fn unavailable(why: &str) -> Response<Body> {
    let message: String = why
        .chars()
        .map(|c| {
            if (c.is_ascii_graphic() && c != '%') || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .header("grpc-status", "14")
        .header("grpc-message", message)
        .body(
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("a gRPC answer")
}
