//! `js job run`, written by hand: submits a job with the flags of `job submit`, writes its
//! stdout and stderr to `js`'s own as the job writes them, waits for it to end, and exits
//! with the status `job wait` gives that end ([`super::wait::OUTCOMES`]).
//!
//! The submit names its job id (the one given with `--id`, or one `js` draws and prints
//! first), so the tries `js_client::jobs::submit_retrying` makes after a lost answer make
//! one job at most. The wait and the two follows then run together, each on a connection of
//! its own; a wait that fails ends the run at once, and one that sees the end lets the follows
//! write the streams to their end. The first SIGINT, SIGTERM or SIGHUP while the job is read
//! cancels it (`CancelJob`, once, at the wait's next pause) and the wait goes on until the job
//! has ended (a first signal after that, while the streams are drained, cancels nothing); a
//! second one ends `js` at once, with 128 plus its number, leaving the job to the server.

use std::cell::RefCell;
use std::future::pending;
use std::io::Write;

use clap::ArgMatches;
use js_client::jobs::{Piece, Waiter, follow_logs, submit_retrying, wait_for_terminal};
use js_proto::v1::{LogStream, SubmitJobRequest};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::follow::{put, write};
use super::wait::{ended, statuses};
use super::{Command, Connection, Key, decode, request, runtime, unanswered, with_flags};
use crate::Services;
use crate::events::{ForwardedSignal, Signaller};
use crate::failure::Failure;
use crate::render::Render;

/// The writers a run shares between its follows and its own lines. Each is borrowed only
/// while it is written to.
type Shared<'a> = RefCell<&'a mut dyn Write>;

/// The command line of `job run`: the flags of `submit`.
pub(crate) fn command(submit: &'static Command) -> clap::Command {
    let more = [
        (
            "1, 3, 4, 9",
            "the submit failed, as for js job submit; with 9 the job may exist, and the same command with the same --id is safe to run again",
        ),
        (
            "129, 130, 143",
            "a second SIGHUP, SIGINT or SIGTERM ended js before the job ended; the job may still be running",
        ),
    ];
    with_flags(clap::Command::new("run"), submit)
        .mut_arg("id", |id| {
            id.help("The job's id: 1 to 63 bytes of letters, digits, dot, underscore and dash (the service checks it). Not given: js draws one and prints it on stderr before it submits. A lost answer to the submit is sent again with this id, and the same command run again with it after exit 9 is answered from the job already made.")
        })
        .about("Submits a job, writes its output as it runs, and exits with a status that says how it ended.")
        .long_about(
            "Submits a job, writes its output as it runs, and exits with a status that says how it ended. Takes the flags of `js job submit`. The submit names its job id (--id, or one js draws and prints on stderr first), and is sent again with that id, up to three times in all, when its answer is lost, so it makes one job at most. Then the job's stdout and stderr are written to js's own as the job writes them (as `js job follow` writes them), while the job is read until it has ended (as `js job wait` reads it); the end is printed on stderr, `job <id> <end>`, once both streams are written to their end. A call of the wait that fails ends js at once with 15. The first SIGINT, SIGTERM or SIGHUP before the job has ended cancels it and js goes on waiting for the end; a second one ends js at once. Written by hand, not generated from the job service.",
        )
        .after_help(statuses(&more))
}

/// Runs `job run`.
pub(crate) fn run(
    globals: &ArgMatches,
    args: &ArgMatches,
    render: Render,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: Services,
) -> Result<u8, Failure> {
    let submit = super::surface()
        .iter()
        .find(|command| command.path == super::SUBMIT)
        .ok_or(Failure::Server(
            "the job command table has no submit".to_owned(),
        ))?;
    let connection = Connection::of(globals, &services.dialer)?;
    let (encoded, key) = request(submit, args, render, err)?;
    let sent: SubmitJobRequest = decode(&encoded)?;
    let job = sent.spec.clone().unwrap_or_default().job_id;
    let (signaller, signals) = unbounded_channel();
    (services.signals)(Signaller::job(signaller))
        .map_err(|why| Failure::Server(format!("cannot watch for signals: {why}")))?;
    let run = Run {
        connection,
        submit,
        key,
        job,
        waiter: &*services.job_waiter,
        render,
    };
    runtime()?.block_on(run.drive(sent, signals, out, err))
}

/// What a run needs once its request is built.
struct Run<'a> {
    connection: Connection<'a>,
    submit: &'static Command,
    key: Option<Key>,
    job: String,
    waiter: &'a dyn Waiter,
    render: Render,
}

impl Run<'_> {
    /// Submits `sent` and waits for the job's end while following its streams, until the end
    /// or a second signal.
    async fn drive(
        &self,
        sent: SubmitJobRequest,
        signals: UnboundedReceiver<ForwardedSignal>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<u8, Failure> {
        let (out, err) = (RefCell::new(out), RefCell::new(err));
        let first = Notify::new();
        tokio::select! {
            biased;
            signal = self.watch(signals, &first, &err) => Err(Failure::Abandoned {
                signal,
                job: self.job.clone(),
            }),
            done = self.work(sent, &first, &out, &err) => done,
        }
    }

    /// The run itself: the submit, then the wait and the two follows together. The wait
    /// cancels the job once `first` is notified.
    async fn work(
        &self,
        sent: SubmitJobRequest,
        first: &Notify,
        out: &Shared<'_>,
        err: &Shared<'_>,
    ) -> Result<u8, Failure> {
        let mut control = self.connection.client().await?;
        let submitted = submit_retrying(&mut control, self.waiter, sent).await;
        submitted.map_err(|why| unanswered(why.into(), self.submit, self.key.as_ref()))?;
        let mut stdout = self.connection.client().await?;
        let mut stderr = self.connection.client().await?;
        let wait = wait_for_terminal(&mut control, self.waiter, &self.job, first.notified());
        let follows = async {
            tokio::join!(
                self.follow(&mut stdout, LogStream::Stdout, out, err),
                self.follow(&mut stderr, LogStream::Stderr, err, err),
            )
        };
        tokio::pin!(wait, follows);
        // The streams are written to their end once the job has ended, but a wait that
        // fails ends the run at once: the streams would go on until the job ends, and a
        // signal would cancel nothing.
        let (waited, drained) = tokio::select! {
            waited = &mut wait => (waited, false),
            _ = &mut follows => (wait.await, true),
        };
        if waited.is_ok() && !drained {
            follows.await;
        }
        ended(&self.job, waited, self.render, &mut **err.borrow_mut())
    }

    /// Writes the job's `stream` to `data` (and its gaps to `notes`) to its end, or says on
    /// `notes` why it stopped: a run's status is the job's end, which a stream that could
    /// not be followed does not change.
    async fn follow(
        &self,
        client: &mut js_client::jobs::JobClient,
        stream: LogStream,
        data: &Shared<'_>,
        notes: &Shared<'_>,
    ) {
        let name = match stream {
            LogStream::Stdout => "stdout",
            _ => "stderr",
        };
        let sink = |piece: Piece<'_>| put(piece, name, self.render, data, notes);
        let followed = follow_logs(client, self.waiter, &self.job, stream, sink).await;
        if let Err(why) = followed {
            let why = Failure::from(why);
            let failure = Failure::Server(format!(
                "cannot follow the {name} of job {}: {why}",
                self.job
            ));
            let _ = write(
                &mut **notes.borrow_mut(),
                self.render.failure(&failure).as_bytes(),
            );
        }
    }

    /// Reads the signals: notifies `first` at the first, and completes with the second. A
    /// signal source that reports nothing leaves it pending.
    async fn watch(
        &self,
        mut signals: UnboundedReceiver<ForwardedSignal>,
        first: &Notify,
        err: &Shared<'_>,
    ) -> ForwardedSignal {
        let mut cancelling = false;
        loop {
            let Some(signal) = signals.recv().await else {
                return pending().await;
            };
            if cancelling {
                return signal;
            }
            cancelling = true;
            first.notify_one();
            let line = self.render.cancelling(signal.name(), &self.job);
            let _ = write(&mut **err.borrow_mut(), line.as_bytes());
        }
    }
}
