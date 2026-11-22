//! What the Mac side and the guest program say to each other over the VM's virtio socket,
//! one JSON object per line each way, and the summary of a set of timed samples.
//!
//! A connection carries one [`Request`] and gets one [`Reply`]; the guest program closes it
//! after the reply. A `run` that is still running when its connection drops keeps running in
//! the guest, and its exit is kept until a `wait` for its id asks for it.

use serde::{Deserialize, Serialize};

/// The virtio socket port the guest program listens on.
pub const GUEST_PORT: u32 = 7000;

/// The most output of one stream a reply carries: the end of the stream.
pub const OUTPUT_TAIL: usize = 4096;

/// One request to the guest program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Answer at once: the channel's own round trip.
    Ping,
    /// Power the guest off: answer, then shut the guest down.
    Poweroff,
    /// Start a container `id` from the guest's prepared root with `args` as its process, wait
    /// for it to end, and answer with its exit. `wall_ms`, when not 0, is the guest's limit
    /// on its wall time: the container is killed when it is reached.
    Run {
        /// The container's id, unique among the guest's containers.
        id: String,
        /// The container's process and its arguments.
        args: Vec<String>,
        /// The wall-time limit in milliseconds, 0 for none.
        #[serde(default)]
        wall_ms: u64,
    },
    /// Run `args` in the guest itself (not in a container) as the guest program's user, wait
    /// for it, and answer with its exit and output: the measurements' way in. The channel is
    /// reached only through the owner's control socket, which only the owner's user can open.
    Exec {
        /// The program and its arguments.
        args: Vec<String>,
    },
    /// Send `signal` (a name `crun kill` takes, such as `KILL` or `TERM`) to the container `id`.
    Kill {
        /// The container's id.
        id: String,
        /// The signal's name.
        signal: String,
    },
    /// Answer with the exit of the container `id` once it has ended.
    Wait {
        /// The container's id.
        id: String,
    },
    /// Start `script` (run by `sh -c`) through the agent's own crun runtime, with the egress
    /// allow-list `allow` (`ip:port` items) enforced around the container, wait for it, and
    /// answer with its exit and output.
    Hardened {
        /// The run's id.
        id: String,
        /// The job's script.
        script: String,
        /// The endpoints the container may reach.
        allow: Vec<String>,
    },
}

/// The guest program's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Reply {
    /// Whether the request was carried out; `error` says why not.
    pub ok: bool,
    /// The container's id, when the request named one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The exit code, when the process exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// The signal that ended the process, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// The end of the standard output ([`OUTPUT_TAIL`] bytes at most).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    /// The end of the standard error ([`OUTPUT_TAIL`] bytes at most).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    /// How long the guest took, from the request read to the reply, in microseconds.
    #[serde(default)]
    pub guest_us: u64,
    /// Whether the guest's wall-time limit ended the container.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wall_limit_hit: bool,
    /// Why the request was not carried out.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Reply {
    /// A refusal saying `why`.
    #[must_use]
    pub fn refused(why: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: why.into(),
            ..Self::default()
        }
    }
}

/// `request` as one line, without its newline.
///
/// # Errors
///
/// The request could not be written as JSON.
pub fn request_line(request: &Request) -> Result<String, serde_json::Error> {
    serde_json::to_string(request)
}

/// The request on `line`.
///
/// # Errors
///
/// The line is not a request.
pub fn parse_request(line: &str) -> Result<Request, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

/// `reply` as one line, without its newline.
///
/// # Errors
///
/// The reply could not be written as JSON.
pub fn reply_line(reply: &Reply) -> Result<String, serde_json::Error> {
    serde_json::to_string(reply)
}

/// The reply on `line`.
///
/// # Errors
///
/// The line is not a reply.
pub fn parse_reply(line: &str) -> Result<Reply, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

/// The last `limit` bytes of `bytes` as text, cut at a character boundary.
#[must_use]
pub fn tail(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= limit {
        return text.into_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// The median and spread of a set of samples, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// How many samples.
    pub n: usize,
    /// The 10th percentile (nearest rank).
    pub p10: u64,
    /// The median (nearest rank).
    pub median: u64,
    /// The 90th percentile (nearest rank).
    pub p90: u64,
    /// The smallest sample.
    pub min: u64,
    /// The largest sample.
    pub max: u64,
}

/// The summary of `samples`, or `None` when there are none.
#[must_use]
pub fn summarise(samples: &[u64]) -> Option<Summary> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let (&min, &max) = (sorted.first()?, sorted.last()?);
    Some(Summary {
        n: sorted.len(),
        p10: rank(&sorted, 10),
        median: rank(&sorted, 50),
        p90: rank(&sorted, 90),
        min,
        max,
    })
}

/// The nearest-rank `percent` percentile of the sorted, non-empty `sorted`.
fn rank(sorted: &[u64], percent: usize) -> u64 {
    let at = (percent * sorted.len()).div_ceil(100).max(1) - 1;
    sorted[at.min(sorted.len() - 1)]
}

impl std::fmt::Display for Summary {
    /// `n=30 median=1.234ms p10=1.000ms p90=2.000ms min=0.900ms max=3.000ms`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |us: u64| format!("{}.{:03}ms", us / 1000, us % 1000);
        write!(
            f,
            "n={} median={} p10={} p90={} min={} max={}",
            self.n,
            ms(self.median),
            ms(self.p10),
            ms(self.p90),
            ms(self.min),
            ms(self.max)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that every request survives the line format unchanged, under its `op` name.
    /// Catches a renamed operation or a field the guest would read under another name.
    #[test]
    fn requests_round_trip_through_a_line() {
        let all = [
            Request::Ping,
            Request::Poweroff,
            Request::Run {
                id: "c1".into(),
                args: vec!["/bin/true".into()],
                wall_ms: 5,
            },
            Request::Kill {
                id: "c1".into(),
                signal: "KILL".into(),
            },
            Request::Wait { id: "c1".into() },
            Request::Exec {
                args: vec!["/bin/true".into()],
            },
            Request::Hardened {
                id: "h1".into(),
                script: "nc -z 192.0.2.1 22".into(),
                allow: vec!["192.0.2.1:22".into()],
            },
        ];
        for request in all {
            let line = request_line(&request).unwrap();
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(parse_request(&format!("{line}\n")).unwrap(), request);
        }
        assert_eq!(request_line(&Request::Ping).unwrap(), r#"{"op":"ping"}"#);
    }

    /// Asserts that a run without `wall_ms` has no limit, and that an unknown operation or a
    /// run without its id is refused. Catches a guest that would start a container with no id.
    #[test]
    fn a_run_without_a_limit_has_none_and_bad_lines_are_refused() {
        assert_eq!(
            parse_request(r#"{"op":"run","id":"a","args":["x"]}"#).unwrap(),
            Request::Run {
                id: "a".into(),
                args: vec!["x".into()],
                wall_ms: 0
            }
        );
        assert!(parse_request(r#"{"op":"reboot"}"#).is_err());
        assert!(parse_request(r#"{"op":"run","args":["x"]}"#).is_err());
        assert!(parse_request("not json").is_err());
    }

    /// Asserts that a reply leaves out its empty fields and reads back equal, and that a
    /// refusal is not ok and says why. Catches a refusal read as a success.
    #[test]
    fn replies_round_trip_and_a_refusal_says_why() {
        let done = Reply {
            ok: true,
            id: "c1".into(),
            exit: Some(0),
            stdout: "hi\n".into(),
            guest_us: 42,
            ..Reply::default()
        };
        let line = reply_line(&done).unwrap();
        assert_eq!(
            line,
            r#"{"ok":true,"id":"c1","exit":0,"stdout":"hi\n","guest_us":42}"#
        );
        assert_eq!(parse_reply(&line).unwrap(), done);
        let no = Reply::refused("no such container");
        assert_eq!(
            reply_line(&no).unwrap(),
            r#"{"ok":false,"guest_us":0,"error":"no such container"}"#
        );
        let killed = Reply {
            ok: true,
            signal: Some(9),
            wall_limit_hit: true,
            stderr: "x".into(),
            ..Reply::default()
        };
        assert_eq!(parse_reply(&reply_line(&killed).unwrap()).unwrap(), killed);
        assert!(parse_reply("{}").is_err());
    }

    /// Asserts that the tail keeps the last bytes and never splits a character. Catches a
    /// tail that keeps the start of the output or panics in the middle of a character.
    #[test]
    fn the_tail_keeps_the_end_at_a_character_boundary() {
        assert_eq!(tail(b"abc", 5), "abc");
        assert_eq!(tail(b"abcdef", 3), "def");
        // "é" is two bytes: a cut inside it moves forward to the next character.
        assert_eq!(tail("aéb".as_bytes(), 2), "b");
        assert_eq!(tail("aéb".as_bytes(), 3), "éb");
        assert_eq!(tail(b"", 3), "");
    }

    /// Asserts the nearest-rank percentiles of a known set, of one sample, and that no samples
    /// have no summary. Catches an off-by-one rank or a median of an unsorted set.
    #[test]
    fn the_summary_takes_nearest_ranks() {
        let samples: Vec<u64> = (1..=10).rev().collect();
        let s = summarise(&samples).unwrap();
        assert_eq!(
            s,
            Summary {
                n: 10,
                p10: 1,
                median: 5,
                p90: 9,
                min: 1,
                max: 10
            }
        );
        let one = summarise(&[7]).unwrap();
        assert_eq!((one.p10, one.median, one.p90, one.n), (7, 7, 7, 1));
        assert_eq!(summarise(&[]), None);
        assert_eq!(
            summarise(&[1500, 2500, 1000]).unwrap().to_string(),
            "n=3 median=1.500ms p10=1.000ms p90=2.500ms min=1.000ms max=2.500ms"
        );
    }
}
