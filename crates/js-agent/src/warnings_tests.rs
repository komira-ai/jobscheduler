use std::io::Write;

use super::*;

/// A writer that keeps what is written, for the tests: clones share one buffer.
#[derive(Clone, Default)]
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn text(&self) -> String {
        let bytes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        String::from_utf8(bytes).expect("utf-8")
    }
}

/// The line of each event `emit` makes while the agent's subscriber, writing to a sink, is
/// this thread's; it is installed while no capture of `test_tracing` is, with the callsites'
/// interest rebuilt, as a capture is.
fn lines_of(emit: impl FnOnce()) -> String {
    let _serial = crate::test_tracing::serial();
    let sink = Sink::default();
    let writer = sink.clone();
    tracing::subscriber::with_default(subscriber(move || writer.clone()), || {
        tracing::callsite::rebuild_interest_cache();
        emit();
    });
    sink.text()
}

/// Asserts that a warning is one line that names the program, says `warning`, and carries
/// the message and then its fields, and that an error says `error`. Catches a warning that
/// reaches the journal without the agent's name, or split over lines.
#[test]
fn a_warning_and_an_error_are_one_line_each_naming_the_agent() {
    let text = lines_of(|| {
        tracing::warn!(lease = "js-0123456789abcdef-r1", "the network: gone");
        tracing::error!("the worker: lost");
    });
    assert_eq!(
        text,
        "js-agent: warning: the network: gone lease=\"js-0123456789abcdef-r1\"\n\
         js-agent: error: the worker: lost\n"
    );
}

/// Asserts that nothing at `INFO` or below is written. Catches a subscriber that fills the
/// journal with a line for every copied output and every adopted run.
#[test]
fn info_and_below_are_not_written() {
    let text = lines_of(|| {
        tracing::info!("an output was copied");
        tracing::debug!("a detail");
        tracing::trace!("a finer detail");
    });
    assert_eq!(text, "");
}

/// Asserts that the line the crun runtime writes when the input sources of a run whose job
/// failed do not answer names the run's lease (which holds the run's id) and the source's
/// path with the error the kernel gave. Catches the check of a run's inputs failing with no
/// trace on the host of which run and which source. Linux only, as the crun runtime is.
#[cfg(target_os = "linux")]
#[test]
fn the_failed_input_check_of_a_run_names_its_lease_and_the_source() {
    let text = lines_of(|| {
        crate::crun::warn_unreadable(
            "js-0123456789abcdef-k1-0",
            "/run/js-agent/mounts/js-0123456789abcdef-k1-0/src/0: Input/output error (os error 5)",
        );
    });
    assert_eq!(
        text,
        "js-agent: warning: the input sources of js-0123456789abcdef-k1-0 did not answer after \
         its job failed, so its result says input-unreadable: \
         /run/js-agent/mounts/js-0123456789abcdef-k1-0/src/0: Input/output error (os error 5)\n"
    );
}

/// Asserts that the subscriber [`install_with`] puts in place is every thread's, so a warning
/// made on a thread the agent spawned (the assemblies' pool, the worker) is written, and that
/// a second install keeps the first. The install is the process's for good, so this is the
/// one test of this binary that makes it. Catches a binary whose warnings go nowhere because
/// no subscriber was installed, or one installed for the main thread alone.
#[test]
fn an_installed_subscriber_writes_the_warnings_of_every_thread() {
    let _serial = crate::test_tracing::serial();
    let sink = Sink::default();
    let writer = sink.clone();
    assert!(install_with(move || writer.clone()));
    tracing::callsite::rebuild_interest_cache();
    assert!(!install_with(io::sink), "the first install stands");
    std::thread::spawn(|| tracing::warn!("from another thread"))
        .join()
        .expect("the thread ends");
    let text = sink.text();
    assert!(
        text.lines()
            .any(|line| line == "js-agent: warning: from another thread"),
        "{text}"
    );
}
