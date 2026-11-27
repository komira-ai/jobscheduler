//! What the test sees of the agent while an order runs, from a thread of its own: the process id
//! launchd gives the agent's job, and what the lease server shows of the host (its state, the
//! agent version its hello named, and the instant its session began), read every [`EVERY`],
//! each change kept with the instant it was first seen. The runner of an order blocks until its
//! verdict, so this is how a test knows when the agent restarted and when its session began, and
//! that nothing restarted at all.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::host::{HOST_ID, SessionReader};
use crate::look::launchd_pid;

/// How often the agent's process and the host are read.
const EVERY: Duration = Duration::from_millis(250);

/// The agent's launchd job.
const AGENT_LABEL: &str = "js-agent";

/// One reading that differs from the one before it (the first is the start's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// When it was read.
    pub at: Instant,
    /// The agent's process, `None` when launchd shows it none.
    pub pid: Option<u32>,
    /// Whether the host list shows the host `live`.
    pub live: bool,
    /// The agent version the host list shows, `None` when it does not list the host.
    pub version: Option<String>,
    /// The instant the host's session began, `None` when it is in none.
    pub session: Option<u64>,
}

impl Seen {
    /// Whether the host is live with an agent of `version` in a session.
    pub fn in_session_at(&self, version: &str) -> bool {
        self.live && self.version.as_deref() == Some(version) && self.session.is_some()
    }
}

/// The thread that reads, until [`Watch::stop`].
pub struct Watch {
    stop: Sender<()>,
    thread: JoinHandle<Vec<Seen>>,
}

impl Watch {
    /// Starts reading the agent's job and the host through `reader`.
    pub fn start(reader: SessionReader) -> Watch {
        let (stop, stopped) = channel::<()>();
        let thread = std::thread::spawn(move || read(&reader, &stopped));
        Watch { stop, thread }
    }

    /// Stops reading; every change seen, in order, the reading at the start first.
    pub fn stop(self) -> Vec<Seen> {
        drop(self.stop);
        self.thread.join().expect("the watch thread")
    }
}

fn read(reader: &SessionReader, stopped: &Receiver<()>) -> Vec<Seen> {
    let mut seen: Vec<Seen> = Vec::new();
    loop {
        let entry = reader.host(HOST_ID);
        let now = Seen {
            at: Instant::now(),
            pid: launchd_pid(AGENT_LABEL),
            live: entry.as_ref().is_some_and(|entry| entry["state"] == "live"),
            version: entry
                .as_ref()
                .and_then(|entry| entry["agent_version"].as_str().map(str::to_owned)),
            session: reader.session_started(HOST_ID),
        };
        let changed = seen.last().is_none_or(|last| {
            (&last.pid, last.live, &last.version, &last.session)
                != (&now.pid, now.live, &now.version, &now.session)
        });
        if changed {
            seen.push(now);
        }
        match stopped.recv_timeout(EVERY) {
            Err(RecvTimeoutError::Timeout) => continue,
            _ => return seen,
        }
    }
}

/// The first reading of `seen` with an agent process other than `before`.
pub fn first_new_process(seen: &[Seen], before: Option<u32>) -> Option<&Seen> {
    seen.iter()
        .find(|reading| reading.pid.is_some() && reading.pid != before)
}

/// The first reading of `seen` that shows the host live, with an agent of `version`, in a
/// session: the session of the agent of that version (an update from another version shows that
/// one's until the new agent's hello).
pub fn first_session_of<'a>(seen: &'a [Seen], version: &str) -> Option<&'a Seen> {
    seen.iter().find(|reading| reading.in_session_at(version))
}

/// The readings of `seen` as lines, for a failure's message and the run's log.
pub fn lines(seen: &[Seen], from: Instant) -> String {
    seen.iter()
        .map(|reading| {
            format!(
                "  +{:>7.2}s pid {:?} live {} version {:?} session {:?}",
                reading.at.saturating_duration_since(from).as_secs_f64(),
                reading.pid,
                reading.live,
                reading.version,
                reading.session
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
