//! The program's signal source: SIGINT and SIGTERM, read on a thread of their own.

use std::io;
use std::thread;

use tokio::runtime::Builder;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::events::{ForwardedSignal, Signaller};

/// Installs handlers for SIGINT and SIGTERM and reports every delivery to `signaller`
/// from a detached thread. The handlers stay installed for the life of the process,
/// which is the life of one `lease run`.
pub(crate) fn install(signaller: Signaller) -> io::Result<()> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            let streams = {
                let _inside = runtime.enter();
                signal(SignalKind::terminate())
                    .and_then(|term| signal(SignalKind::interrupt()).map(|int| (term, int)))
            };
            streams.map(|(term, interrupt)| {
                runtime.spawn(forward(term, ForwardedSignal::Terminate, signaller.clone()));
                runtime.spawn(forward(interrupt, ForwardedSignal::Interrupt, signaller));
                thread::spawn(move || runtime.block_on(std::future::pending::<()>()));
            })
        })
}

/// Reports every delivery on `stream` as `signal`, for as long as the process lives.
async fn forward(mut stream: Signal, signal: ForwardedSignal, to: Signaller) {
    loop {
        stream.recv().await;
        to.deliver(signal);
    }
}
