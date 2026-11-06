//! The loops a client of the job service runs over its calls, written by hand (the calls
//! themselves are generated): waiting for a job to end ([`wait_for_terminal`]), following
//! one of its output streams to its end ([`follow_logs`]), and submitting with retries that
//! cannot make a second job ([`submit_retrying`]).
//!
//! They make their calls through [`JobCalls`], which [`JobClient`] implements, so a test
//! drives them with a fake; and every pause is asked of a [`Waiter`], the only thing that
//! lets time pass, so nothing here reads a clock or sleeps on its own.

use std::future::{Future, poll_fn};
use std::io;
use std::pin::{Pin, pin};
use std::task::Poll;
use std::time::Duration;

use js_proto::v1::{self, JobState, LogStream, ResultReason};
use tonic::Code;

use super::{JobClient, JobError, NotSent};

/// The pause between two reads of a job that has not ended.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// The pause after a follow answered no data and the stream has not ended: the server's own
/// wait (up to 20 seconds) is over, or the server did not wait.
const FOLLOW_IDLE: Duration = Duration::from_secs(1);

/// The pause after the server refused a follow because as many as it allows wait already
/// (`RESOURCE_EXHAUSTED`, `too-many-follows`).
const FOLLOW_BUSY: Duration = Duration::from_secs(5);

/// How many times in a row a follow is sent when its answer is lost, or its connection
/// failed.
const FOLLOW_TRIES: u32 = 3;

/// The pause before a follow whose answer was lost is sent again.
const FOLLOW_LOST: Duration = Duration::from_secs(2);

/// How many times a submit with a job id is sent when its answer is lost.
const SUBMIT_TRIES: u32 = 3;

/// The pause between two tries of a submit.
const SUBMIT_PAUSE: Duration = Duration::from_secs(2);

/// The calls of the job service the loops make. [`JobClient`] makes them over its
/// connection; a test implements them with a fake that answers from a script.
pub trait JobCalls {
    /// `SubmitJob`.
    fn submit_job(
        &mut self,
        request: v1::SubmitJobRequest,
    ) -> impl Future<Output = Result<v1::SubmitJobResponse, JobError>>;

    /// `GetJob`.
    fn get_job(
        &mut self,
        request: v1::GetJobRequest,
    ) -> impl Future<Output = Result<v1::GetJobResponse, JobError>>;

    /// `CancelJob`.
    fn cancel_job(
        &mut self,
        request: v1::CancelJobRequest,
    ) -> impl Future<Output = Result<v1::CancelJobResponse, JobError>>;

    /// `GetResult`.
    fn get_result(
        &mut self,
        request: v1::GetResultRequest,
    ) -> impl Future<Output = Result<v1::GetResultResponse, JobError>>;

    /// `GetLogs`.
    fn get_logs(
        &mut self,
        request: v1::GetLogsRequest,
    ) -> impl Future<Output = Result<v1::GetLogsResponse, JobError>>;
}

impl JobCalls for JobClient {
    fn submit_job(
        &mut self,
        request: v1::SubmitJobRequest,
    ) -> impl Future<Output = Result<v1::SubmitJobResponse, JobError>> {
        JobClient::submit_job(self, request)
    }

    fn get_job(
        &mut self,
        request: v1::GetJobRequest,
    ) -> impl Future<Output = Result<v1::GetJobResponse, JobError>> {
        JobClient::get_job(self, request)
    }

    fn cancel_job(
        &mut self,
        request: v1::CancelJobRequest,
    ) -> impl Future<Output = Result<v1::CancelJobResponse, JobError>> {
        JobClient::cancel_job(self, request)
    }

    fn get_result(
        &mut self,
        request: v1::GetResultRequest,
    ) -> impl Future<Output = Result<v1::GetResultResponse, JobError>> {
        JobClient::get_result(self, request)
    }

    fn get_logs(
        &mut self,
        request: v1::GetLogsRequest,
    ) -> impl Future<Output = Result<v1::GetLogsResponse, JobError>> {
        JobClient::get_logs(self, request)
    }
}

/// How a loop lets time pass. A loop never sleeps on its own: it asks the waiter, so a test
/// hands in one that records the pause and returns at once.
pub trait Waiter: Send + Sync {
    /// Completes once `interval` has passed.
    fn wait(&self, interval: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// The waiter of a program: tokio's timer. It needs a runtime with time enabled.
#[derive(Debug, Clone, Copy)]
pub struct TokioWaiter;

impl Waiter for TokioWaiter {
    fn wait(&self, interval: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(interval))
    }
}

/// How an ended job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It exited with code 0.
    Succeeded,
    /// It exited with a nonzero code, or was stopped because it passed its memory.
    Failed,
    /// It was stopped at its wall limit: failed, with the result's reason `WALL_LIMIT`.
    TimedOut,
    /// It was cancelled.
    Cancelled,
    /// Its container is gone and no result exists.
    Lost,
    /// It did not get to run.
    FailedToStart,
}

/// Why [`wait_for_terminal`] could not say how the job ended.
#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    /// A call failed.
    #[error(transparent)]
    Call(#[from] JobError),
    /// The job is in a state this client does not know: one a newer server added, or none.
    #[error("job {job_id} is in a state this client does not know ({state})")]
    UnknownState {
        /// The job.
        job_id: String,
        /// The state's number on the wire.
        state: i32,
    },
}

/// What a read of a job says.
enum Seen {
    /// The job has not ended.
    Live,
    /// The job ended in this state. A failed job's result says whether it timed out.
    Ended(Outcome),
    /// A state this client does not know.
    Unknown,
}

/// What the state numbered `state` says.
fn seen(state: i32) -> Seen {
    match JobState::try_from(state) {
        Ok(JobState::Queued | JobState::Placed | JobState::Starting | JobState::Running) => {
            Seen::Live
        }
        Ok(JobState::Succeeded) => Seen::Ended(Outcome::Succeeded),
        Ok(JobState::Failed) => Seen::Ended(Outcome::Failed),
        Ok(JobState::Cancelled) => Seen::Ended(Outcome::Cancelled),
        Ok(JobState::Lost) => Seen::Ended(Outcome::Lost),
        Ok(JobState::FailedToStart) => Seen::Ended(Outcome::FailedToStart),
        Ok(JobState::Unspecified) | Err(_) => Seen::Unknown,
    }
}

/// Reads job `job_id` until it has ended, with a pause of two seconds asked of `waiter`
/// after each read that finds it live, and says how it ended. A failed job's result is read
/// too, to tell a job stopped at its wall limit ([`Outcome::TimedOut`]) from one that failed
/// otherwise.
///
/// When `cancel` completes while the loop pauses, the job is cancelled (`CancelJob`, once)
/// and the loop goes on reading until the job has ended, so the answer is how it really
/// ended: cancelled, or another end it reached first. A wait that should never cancel
/// passes [`std::future::pending`].
///
/// # Errors
///
/// [`WaitError::Call`] when a call fails (the loop does not retry, so the job's end is then
/// unknown, not some other answer); [`WaitError::UnknownState`] when a read finds the job in
/// a state this client does not know.
pub async fn wait_for_terminal<C, W>(
    client: &mut C,
    waiter: &W,
    job_id: &str,
    cancel: impl Future<Output = ()>,
) -> Result<Outcome, WaitError>
where
    C: JobCalls + ?Sized,
    W: Waiter + ?Sized,
{
    let mut cancel = pin!(cancel);
    let mut cancelled = false;
    loop {
        let read = v1::GetJobRequest {
            job_id: job_id.to_owned(),
        };
        let state = client.get_job(read).await?.job.unwrap_or_default().state;
        match seen(state) {
            Seen::Live => {}
            Seen::Ended(Outcome::Failed) => return failed(client, job_id).await,
            Seen::Ended(outcome) => return Ok(outcome),
            Seen::Unknown => {
                return Err(WaitError::UnknownState {
                    job_id: job_id.to_owned(),
                    state,
                });
            }
        }
        let pause = waiter.wait(POLL_EVERY);
        let asked = match cancelled {
            true => {
                pause.await;
                false
            }
            false => first(cancel.as_mut(), pause).await,
        };
        if asked {
            let request = v1::CancelJobRequest {
                job_id: job_id.to_owned(),
            };
            client.cancel_job(request).await?;
            cancelled = true;
        }
    }
}

/// Waits for `cancel` or `pause`, whichever completes first, and says whether it was
/// `cancel`. `cancel` is looked at first, so a cancel that is already due is never lost to a
/// pause that is over too.
async fn first(
    mut cancel: Pin<&mut impl Future<Output = ()>>,
    mut pause: Pin<Box<dyn Future<Output = ()> + Send + '_>>,
) -> bool {
    poll_fn(move |cx| match cancel.as_mut().poll(cx) {
        Poll::Ready(()) => Poll::Ready(true),
        Poll::Pending => pause.as_mut().poll(cx).map(|()| false),
    })
    .await
}

/// How failed job `job_id` failed: its result's reason tells a wall limit from the rest.
async fn failed<C: JobCalls + ?Sized>(client: &mut C, job_id: &str) -> Result<Outcome, WaitError> {
    let request = v1::GetResultRequest {
        job_id: job_id.to_owned(),
    };
    let reason = client
        .get_result(request)
        .await?
        .result
        .unwrap_or_default()
        .reason;
    Ok(match ResultReason::try_from(reason) {
        Ok(ResultReason::WallLimit) => Outcome::TimedOut,
        _ => Outcome::Failed,
    })
}

/// One piece of a stream, in the stream's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Bytes of the stream, exactly as the job wrote them. Possibly empty.
    Data(&'a [u8]),
    /// Bytes of the stream that are not in the log store and never will be.
    Gap {
        /// The offset of the gap's first byte.
        offset: u64,
        /// How many bytes the gap holds.
        length: u64,
    },
}

/// Why [`follow_logs`] stopped before the stream's end.
#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    /// A call failed.
    #[error(transparent)]
    Call(#[from] JobError),
    /// The sink refused a piece.
    #[error("cannot write the log: {0}")]
    Write(#[source] io::Error),
}

/// Reads `stream` of job `job_id` from its first byte to its end and hands each piece to
/// `sink`, in order, each byte once: every `GetLogs` asks the server to wait for new bytes
/// (`follow`) and starts at the `next_offset` the one before it answered. The loop ends only
/// at `eof`, which the server sets once the job has ended and every byte was answered, so
/// bytes still unread when the job ends are read before it returns.
///
/// An answer with no data that is not the end is followed by a pause of one second asked
/// of `waiter`; a follow the server refuses because too many wait (`RESOURCE_EXHAUSTED`) by
/// a pause of five seconds and the same read again; and a follow whose answer was lost or
/// whose connection failed (the call reads only, so a repeat is safe; the server asks a
/// client to retry `log-store-unavailable`) by a pause of two seconds and the same read
/// again, three times in a row at most.
///
/// # Errors
///
/// [`FollowError::Call`] when a call fails otherwise, or is lost three times in a row, and
/// [`FollowError::Write`] when `sink` fails.
pub async fn follow_logs<C, W>(
    client: &mut C,
    waiter: &W,
    job_id: &str,
    stream: LogStream,
    mut sink: impl FnMut(Piece<'_>) -> io::Result<()>,
) -> Result<(), FollowError>
where
    C: JobCalls + ?Sized,
    W: Waiter + ?Sized,
{
    let mut from = 0;
    let mut lost = 0;
    loop {
        let request = v1::GetLogsRequest {
            job_id: job_id.to_owned(),
            stream: stream.into(),
            from_offset: Some(from),
            max_bytes: None,
            follow: Some(true),
        };
        let page = match client.get_logs(request).await {
            Ok(page) => page,
            Err(JobError::Rejected {
                code: Code::ResourceExhausted,
                ..
            }) => {
                waiter.wait(FOLLOW_BUSY).await;
                continue;
            }
            Err(error) if lost + 1 < FOLLOW_TRIES && again(&error) => {
                lost += 1;
                waiter.wait(FOLLOW_LOST).await;
                continue;
            }
            Err(other) => return Err(other.into()),
        };
        lost = 0;
        for gap in &page.gaps {
            let (offset, length) = (gap.offset, gap.length);
            sink(Piece::Gap { offset, length }).map_err(FollowError::Write)?;
        }
        sink(Piece::Data(&page.data)).map_err(FollowError::Write)?;
        if page.eof {
            return Ok(());
        }
        if page.data.is_empty() {
            waiter.wait(FOLLOW_IDLE).await;
        }
        from = page.next_offset;
    }
}

/// Submits `request`, sending it again, up to three times in all with a pause of two
/// seconds asked of `waiter` between, while its answer is lost (the call may have acted) or
/// its connection failed before it was written. Only a request whose spec names its job id
/// is sent again: the service answers a repeat of that id from the job the first one made,
/// so the retries make one job at most. A request without an id is sent once, since the
/// service would mint a new id for each try.
///
/// # Errors
///
/// The last try's failure, or the first one that is not worth another try (a refusal, a
/// refused token).
pub async fn submit_retrying<C, W>(
    client: &mut C,
    waiter: &W,
    request: v1::SubmitJobRequest,
) -> Result<v1::SubmitJobResponse, JobError>
where
    C: JobCalls + ?Sized,
    W: Waiter + ?Sized,
{
    let named = request
        .spec
        .as_ref()
        .is_some_and(|spec| !spec.job_id.is_empty());
    let tries = match named {
        true => SUBMIT_TRIES,
        false => 1,
    };
    let mut tried = 1;
    loop {
        match client.submit_job(request.clone()).await {
            Err(error) if tried < tries && again(&error) => {
                waiter.wait(SUBMIT_PAUSE).await;
                tried += 1;
            }
            done => return done,
        }
    }
}

/// Whether a call that failed with `error` is worth another try of the same request: its
/// answer was lost, or its connection failed before it was written.
fn again(error: &JobError) -> bool {
    matches!(
        error,
        JobError::MayHaveActed(_) | JobError::NotSent(NotSent::Unreached(_))
    )
}
