//! The line protocol between the host agent and `js-vmm lease`, the process that holds the
//! real [`VmDriver`]: one JSON [`Request`] per line from the agent, one JSON [`Reply`] per line
//! back, in order. [`serve`] is the `js-vmm` side, [`Session`] the agent's.
//!
//! Every request but [`Request::Kill`] gets exactly one reply, and they are answered one at a
//! time, in the order sent. A kill gets none and is acted on as soon as it is read, even while
//! a command runs: that is how a stop reaches a command the guest is running. End of input
//! ends the service once the request in hand is answered.

use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::{Collected, ExecEnd, ExecRequest, VmDriver, VmError, VmName, VmSize};

/// A call of [`VmDriver`], as one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// [`VmDriver::clone_base`].
    Clone {
        /// The VM.
        vm: VmName,
    },
    /// [`VmDriver::boot`].
    Boot {
        /// The VM.
        vm: VmName,
        /// Its size.
        size: VmSize,
    },
    /// [`VmDriver::wait_ready`].
    WaitReady {
        /// The VM.
        vm: VmName,
    },
    /// [`VmDriver::run_command`].
    RunCommand {
        /// The VM.
        vm: VmName,
        /// The command.
        request: ExecRequest,
    },
    /// [`VmDriver::kill`]; answered by nothing.
    Kill {
        /// The VM.
        vm: VmName,
    },
    /// [`VmDriver::collect`].
    Collect {
        /// The VM.
        vm: VmName,
        /// The most bytes of each stream.
        tail_bytes: usize,
    },
    /// [`VmDriver::stop`].
    Stop {
        /// The VM.
        vm: VmName,
    },
    /// [`VmDriver::destroy`].
    Destroy {
        /// The VM.
        vm: VmName,
    },
    /// [`VmDriver::leftovers`].
    Leftovers,
}

/// The answer to one [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// The call succeeded and has nothing to say.
    Done,
    /// [`Request::RunCommand`] ended so.
    Ended {
        /// How.
        end: ExecEnd,
    },
    /// What [`Request::Collect`] read.
    Collected {
        /// The output.
        output: Collected,
    },
    /// What [`Request::Leftovers`] found.
    Names {
        /// The VMs.
        names: Vec<VmName>,
    },
    /// The call failed.
    Failed {
        /// Why.
        error: VmError,
    },
    /// The line was no request.
    Refused {
        /// Why, from the decoder.
        why: String,
    },
}

/// The reply of `result`: its value as `ok` says, or [`Reply::Failed`].
fn reply_of<T>(result: Result<T, VmError>, ok: impl FnOnce(T) -> Reply) -> Reply {
    result.map_or_else(|error| Reply::Failed { error }, ok)
}

/// Makes the call `request` names on `driver`. [`serve`] makes a kill as it reads it and
/// sends no reply to it.
fn answer(driver: &dyn VmDriver, request: Request) -> Reply {
    let done = |()| Reply::Done;
    match request {
        Request::Clone { vm } => reply_of(driver.clone_base(&vm), done),
        Request::Boot { vm, size } => reply_of(driver.boot(&vm, size), done),
        Request::WaitReady { vm } => reply_of(driver.wait_ready(&vm), done),
        Request::RunCommand { vm, request } => reply_of(driver.run_command(&vm, &request), |end| {
            Reply::Ended { end }
        }),
        Request::Collect { vm, tail_bytes } => {
            reply_of(driver.collect(&vm, tail_bytes), |output| Reply::Collected {
                output,
            })
        }
        Request::Stop { vm } => reply_of(driver.stop(&vm), done),
        Request::Destroy { vm } => reply_of(driver.destroy(&vm), done),
        Request::Leftovers => reply_of(driver.leftovers(), |names| Reply::Names { names }),
        Request::Kill { vm } => {
            driver.kill(&vm);
            Reply::Done
        }
    }
}

/// Serves `driver` to the requests of `input`, writing the replies to `output`, until the
/// input ends (see the [module](self)).
///
/// # Errors
///
/// The input could not be read or a reply could not be written.
pub fn serve(
    driver: &dyn VmDriver,
    input: &mut (dyn BufRead + Send),
    output: &mut dyn Write,
) -> io::Result<()> {
    let (calls, queue) = mpsc::channel::<Result<Request, String>>();
    std::thread::scope(|scope| {
        let reader = scope.spawn(move || -> io::Result<()> {
            for line in input.lines() {
                match serde_json::from_str::<Request>(&line?) {
                    // A kill is made here, as it is read, and answered by nothing.
                    Ok(kill @ Request::Kill { .. }) => {
                        let _ = answer(driver, kill);
                    }
                    Ok(request) => {
                        let _ = calls.send(Ok(request));
                    }
                    Err(why) => {
                        let _ = calls.send(Err(why.to_string()));
                    }
                }
            }
            Ok(())
        });
        let mut written = Ok(());
        for call in queue {
            let reply = match call {
                Ok(request) => answer(driver, request),
                Err(why) => Reply::Refused { why },
            };
            written = write_line(output, &reply);
            if written.is_err() {
                break;
            }
        }
        // The reader panics only if the driver's kill does; that is an error of the service.
        let read = reader
            .join()
            .unwrap_or(Err(io::Error::other("the reader panicked")));
        written.and(read)
    })
}

/// Writes `value` as one JSON line and flushes it.
fn write_line(output: &mut dyn Write, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    output.write_all(&line)?;
    output.flush()
}

/// The agent's side of one connection to a [`serve`]: calls answered in order, and a kill that
/// may be sent from another thread while a call waits.
pub struct Session {
    requests: Mutex<Box<dyn Write + Send>>,
    replies: Mutex<Box<dyn BufRead + Send>>,
}

fn locked<T: ?Sized>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Session {
    /// A session that writes its requests to `requests` and reads the replies from `replies`.
    pub fn new(requests: Box<dyn Write + Send>, replies: Box<dyn BufRead + Send>) -> Self {
        Self {
            requests: Mutex::new(requests),
            replies: Mutex::new(replies),
        }
    }

    /// Sends `request` and waits for its reply. One call at a time: a second caller waits for
    /// the first one's reply.
    ///
    /// # Errors
    ///
    /// The request could not be sent, the connection ended, or the reply did not decode.
    pub fn call(&self, request: &Request) -> io::Result<Reply> {
        let mut replies = locked(&self.replies);
        self.send(request)?;
        let mut line = String::new();
        if replies.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the VM helper ended",
            ));
        }
        serde_json::from_str(&line).map_err(io::Error::other)
    }

    /// Sends `request` without waiting for a reply: a [`Request::Kill`], which has none, even
    /// while a [`Session::call`] waits.
    ///
    /// # Errors
    ///
    /// The request could not be sent.
    pub fn send(&self, request: &Request) -> io::Result<()> {
        write_line(&mut **locked(&self.requests), request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Call, FakeDriver};

    /// Asserts that `answer` makes a kill on the driver and answers done (a reply `serve`
    /// drops: a kill gets none). Catches a kill that is dropped on this path.
    #[test]
    fn a_kill_answered_in_turn_still_kills() {
        let driver = FakeDriver::new();
        let vm = VmName::new("run-1").unwrap();
        assert_eq!(
            answer(&driver, Request::Kill { vm: vm.clone() }),
            Reply::Done
        );
        assert_eq!(driver.calls(), [Call::Kill(vm)]);
    }
}
