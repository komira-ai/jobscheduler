//! The program's signal source: SIGINT, SIGTERM and SIGHUP, read on a thread of their own.

use std::io;
use std::thread;

use tokio::runtime::Builder;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::events::{ForwardedSignal, Signaller};

/// Installs handlers for SIGINT, SIGTERM and SIGHUP and reports every delivery to
/// `signaller` from a detached thread. The handlers stay installed for the life of the
/// process, which is the life of one `job run`. A signal that arrives before the
/// first report is read is held, not lost.
pub(crate) fn install(signaller: Signaller) -> io::Result<()> {
    let kinds = [
        (SignalKind::terminate(), ForwardedSignal::Terminate),
        (SignalKind::interrupt(), ForwardedSignal::Interrupt),
        (SignalKind::hangup(), ForwardedSignal::Hangup),
    ];
    Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            let streams = {
                let _inside = runtime.enter();
                kinds
                    .into_iter()
                    .map(|(kind, which)| signal(kind).map(|stream| (stream, which)))
                    .collect::<io::Result<Vec<_>>>()
            };
            streams.map(|streams| {
                for (stream, which) in streams {
                    runtime.spawn(forward(stream, which, signaller.clone()));
                }
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

#[cfg(test)]
mod tests {
    use rustix::process::{Signal, getpid, kill_process};
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    /// Asserts that after `install`, a SIGHUP sent to the process is reported to the
    /// signaller as `Hangup` (and the process lives on to read it). It catches a handler
    /// that is not installed (the signal would end the test process), a signal reported
    /// as another one, and a report that never reaches the channel.
    #[test]
    fn an_installed_handler_reports_the_signal_it_catches() {
        let (to, mut reported) = unbounded_channel();
        install(Signaller::job(to)).unwrap();
        kill_process(getpid(), Signal::HUP).unwrap();
        assert_eq!(reported.blocking_recv(), Some(ForwardedSignal::Hangup));
    }
}
