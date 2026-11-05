//! One job's upload as a machine that takes one step at a time: poll the two capture files,
//! then make every store request that is due, then say when to step again. The thread that
//! drives it ([`super::Uploader`]) only sleeps between steps.

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use js_logstore::layout::{FIRST_BYTE_AT_MS, encode_gap};
use js_logstore::{Clock, Credential, Gap, HttpRequest, HttpResponse, Metadata, Store, StoreError};
use js_logstore::{Stream, Transport, TransportError};

use super::Limits;
use super::grant::{HostGrant, epoch_ms};
use super::record::{Recorder, Streams};
use super::sink::Sink;
use super::stream::{Action, End, Tail};

/// What the step said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    /// Step again at this time, or when woken.
    Sleep(SystemTime),
    /// The upload is over: every stream is done, or it gave up.
    Done,
}

/// The end of the job, as `finish` sends it: per stream (stdout, stderr), the file's final
/// length and the bytes dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ends(pub(super) [End; 2]);

/// What the handle tells the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Message {
    /// The job has ended: upload the rest, the markers and the finals.
    End(Ends),
    /// The handle was dropped without an end (the run was removed before its end was read):
    /// the upload stops.
    Dropped,
}

/// The transport, shared by every uploader of the host.
struct Port(Arc<dyn Transport>);

impl Transport for Port {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.0.send(request)
    }
}

/// When the next request may go after failures.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
    retry_at: Option<SystemTime>,
    /// The generation of the credential the store answered as expired: a newer one ends the
    /// wait at once.
    expired: Option<u64>,
}

impl Backoff {
    /// Whether requests wait at `now`, the host holding the credential of `generation`.
    fn waiting(&self, now: SystemTime, generation: u64) -> bool {
        let until_later = self.retry_at.is_some_and(|at| now < at);
        until_later && self.expired.is_none_or(|expired| expired == generation)
    }

    /// Records a failure at `now`: the next request waits `first` doubled for each failure
    /// before it, at most `max`, less a spread of up to three sixteenths drawn from `seed` so
    /// that the uploaders of a host do not all retry at one instant.
    fn failed(&mut self, now: SystemTime, limits: &Limits, seed: u64, expired: Option<u64>) {
        let doubled = 2u32.saturating_pow(self.failures);
        let delay = limits
            .backoff_first
            .saturating_mul(doubled)
            .min(limits.backoff_max);
        let spread = (seed ^ u64::from(self.failures)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 62;
        let spread = u32::try_from(spread).unwrap_or(0);
        let delay = delay.saturating_sub((delay / 16).saturating_mul(spread));
        self.failures = self.failures.saturating_add(1);
        // At least a millisecond, so that a failed stream always waits past the step it failed
        // in.
        self.retry_at = now.checked_add(delay.max(Duration::from_millis(1)));
        self.expired = expired;
    }
}

/// One job's upload.
pub(super) struct Upload {
    sink: Sink,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    grant: Arc<HostGrant>,
    limits: Limits,
    store: Option<Store<Port, Arc<dyn Clock>>>,
    streams: [Tail; 2],
    /// The writer of the progress record.
    recorder: Recorder,
    /// When the end's work is given up: [`Limits::budget`] after `finish`.
    deadline: Option<SystemTime>,
    /// Each stream's backoff (stdout, stderr).
    backoff: [Backoff; 2],
    /// The stream asked first for its next request: the one that did not go last.
    turn: usize,
    seed: u64,
}

/// A failed request: the store's answer, or the capture file could not be read.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("{0}")]
    Store(StoreError),
    #[error("read the capture file: {0}")]
    Read(std::io::Error),
}

impl Upload {
    /// The upload of the job whose sink is `sink`, tailing `files` (stdout, stderr) from where
    /// `streams` says each is, and keeping its progress record with `recorder`.
    pub(super) fn new(
        sink: Sink,
        files: [Arc<File>; 2],
        (transport, clock, grant): (Arc<dyn Transport>, Arc<dyn Clock>, Arc<HostGrant>),
        limits: Limits,
        (recorder, streams): (Recorder, Streams),
    ) -> Self {
        let [stdout, stderr] = files;
        let seed = sink
            .prefix()
            .as_str()
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
        Self {
            sink,
            transport,
            clock,
            grant,
            limits,
            store: None,
            streams: [
                Tail::new(Stream::Stdout, stdout, streams.stdout),
                Tail::new(Stream::Stderr, stderr, streams.stderr),
            ],
            recorder,
            deadline: None,
            backoff: [Backoff::default(), Backoff::default()],
            turn: 0,
            seed,
        }
    }

    /// Writes the progress record as the upload begins, before its first step: a job adopted
    /// after a restart that comes before its first put still has one.
    pub(super) fn begin(&self) {
        self.note();
    }

    /// Writes the progress record of where both streams are.
    fn note(&self) {
        self.recorder
            .note(self.streams.each_ref().map(Tail::progress));
    }

    /// One step: takes the message when one has come (the end, or that the run was dropped
    /// without one, which ends the upload), polls the files, and makes every request that is
    /// due, the two streams taking turns: a success lets the other stream go next; a failure
    /// holds that stream until its backoff has passed, while the other goes on. With no
    /// credential held, or one that has expired, no request is made: the upload waits as for a
    /// store that is down. An answer that the credential has expired holds both streams until
    /// the backoff has passed or a newer credential has come; after the end, with no newer
    /// one, it gives up at once. After the end the budget is checked before every request, so
    /// a step never runs past it.
    pub(super) fn step(&mut self, message: Option<Message>) -> Next {
        let started = self.clock.now();
        match message {
            Some(Message::Dropped) => return Next::Done,
            Some(Message::End(Ends(ends))) => {
                self.deadline = Some(started.checked_add(self.limits.budget).unwrap_or(started));
                for (tail, end) in self.streams.iter_mut().zip(ends) {
                    tail.poll(started);
                    tail.end(end);
                }
            }
            None => {}
        }
        self.streams.iter_mut().for_each(|tail| tail.poll(started));
        loop {
            let now = self.clock.now();
            if self.deadline.is_some_and(|deadline| now >= deadline) {
                return Next::Done;
            }
            let Some((index, action)) = self.due(started) else {
                break;
            };
            let Some((credential, generation)) = self.grant.current() else {
                break;
            };
            // The cut is on record before its put: a restart puts the same chunk again.
            if let Action::Chunk { .. } = action {
                self.note();
            }
            let failure = match self.request(index, action, credential) {
                Ok(()) => {
                    self.streams[index].succeeded(action);
                    self.backoff[index] = Backoff::default();
                    self.turn = 1 - index;
                    // The deletes are recorded once the last is done, not one by one: a
                    // restart deletes again what was deleted since, which changes nothing.
                    let deleting =
                        matches!(action, Action::Delete { .. }) && self.streams[index].deleting();
                    if !deleting {
                        self.note();
                    }
                    continue;
                }
                Err(failure) => failure,
            };
            let now = self.clock.now();
            let stream = self.streams[index].stream.name();
            tracing::debug!(
                prefix = self.sink.prefix().as_str(),
                stream,
                "live logs: {failure}"
            );
            if let Failure::Store(StoreError::Expired { .. }) = failure {
                if self.gives_up(generation) {
                    return Next::Done;
                }
                // Both streams wait for the backoff unless a newer credential is held, which
                // `due` sees at once.
                for backoff in &mut self.backoff {
                    backoff.failed(now, &self.limits, self.seed, Some(generation));
                }
                continue;
            }
            if let Action::Delete { .. } = action {
                self.streams[index].delete_failed(self.limits.delete_tries);
            }
            self.backoff[index].failed(now, &self.limits, self.seed, None);
        }
        let (limits, max_put) = (self.limits.cut(), self.limits.max_put());
        let now = self.clock.now();
        match self.deadline.is_some()
            && self
                .streams
                .iter_mut()
                .all(|t| t.done(now, limits, max_put))
        {
            true => Next::Done,
            false => Next::Sleep(now.checked_add(self.limits.poll).unwrap_or(now)),
        }
    }

    /// Records that the store answered the credential of `generation` as expired, and says
    /// whether the upload gives up: after the end, when no newer credential has come since (a
    /// newer one that came while the request was out is used at once).
    fn gives_up(&self, generation: u64) -> bool {
        self.grant.refused(generation);
        let newer = self.grant.generation() != generation;
        self.deadline.is_some() && !newer
    }

    /// The stream and request due at `at` (the step's start), the stream whose turn it is
    /// first, skipping a stream whose backoff holds it.
    fn due(&mut self, at: SystemTime) -> Option<(usize, Action)> {
        let generation = self.grant.generation();
        let (limits, max_put) = (self.limits.cut(), self.limits.max_put());
        let order = [self.turn, 1 - self.turn];
        order.into_iter().find_map(|index| {
            let held = self.backoff[index].waiting(at, generation);
            let action = self.streams[index].action(at, limits, max_put);
            action.filter(|_| !held).map(|action| (index, action))
        })
    }

    /// Makes the request of `action` for the stream at `index`, signed with `credential`.
    fn request(
        &mut self,
        index: usize,
        action: Action,
        credential: Credential,
    ) -> Result<(), Failure> {
        let tail = &self.streams[index];
        let prefix = self.sink.prefix();
        let (key, body, metadata) = match action {
            Action::Chunk {
                offset,
                len,
                first_at,
            } => {
                let body = tail.read(offset, len).map_err(Failure::Read)?;
                let metadata =
                    Metadata::from([(FIRST_BYTE_AT_MS.to_owned(), epoch_ms(first_at).to_string())]);
                (prefix.chunk_key(tail.stream, offset), body, metadata)
            }
            Action::Gap { offset, length } => {
                let body = Gap::new(offset, length)
                    .map(|gap| encode_gap(&gap))
                    .unwrap_or_default();
                (prefix.gap_key(tail.stream, offset), body, Metadata::new())
            }
            Action::Final { len } => {
                let len = usize::try_from(len).unwrap_or(usize::MAX);
                let body = tail.read(0, len).map_err(Failure::Read)?;
                (prefix.final_key(tail.stream), body, Metadata::new())
            }
            Action::Delete { offset } => {
                let key = prefix.chunk_key(tail.stream, offset);
                return self.store(credential).delete(&key).map_err(Failure::Store);
            }
        };
        self.store(credential)
            .put(&key, body, &metadata)
            .map_err(Failure::Store)
    }

    /// The client, signing with `credential` from now on.
    fn store(&mut self, credential: Credential) -> &Store<Port, Arc<dyn Clock>> {
        let (transport, clock, endpoint) = (&self.transport, &self.clock, self.sink.endpoint());
        let store = self.store.get_or_insert_with(|| {
            Store::new(
                Port(Arc::clone(transport)),
                Arc::clone(clock),
                endpoint.clone(),
                credential.clone(),
            )
        });
        store.set_credential(credential);
        store
    }
}
