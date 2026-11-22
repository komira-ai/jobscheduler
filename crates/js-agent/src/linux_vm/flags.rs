//! The command line of `js-agent __linux-vm`.
//!
//! ```text
//! js-agent __linux-vm run   --dir D [--cpus N] [--memory-mib N] [--share TAG=PATH[:ro]]...
//!                           [--forward PORT=HOST:PORT]... [--ready-port N] [--detach]
//!                           [--exit-when-ready] [--idle-stop-s N] [--start-stopped]
//! js-agent __linux-vm adopt --dir D <the flags of run>
//! js-agent __linux-vm ctl   --dir D <request words>
//! js-agent __linux-vm call  --dir D --port N --request JSON [--samples N]
//! js-agent __linux-vm time  [--samples N] -- PROGRAM [ARGS...]
//! js-agent __linux-vm place --socket S --uid N --ref R --run ID [--release]
//! ```

use std::path::PathBuf;

/// A directory of the Mac shared into the guest under a virtiofs tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// The tag the guest mounts it by.
    pub tag: String,
    /// The directory on the Mac.
    pub path: PathBuf,
    /// Whether the guest may only read it.
    pub read_only: bool,
}

/// A port of the host's virtio socket whose connections are carried to a TCP address on the
/// Mac (a loopback listener the guest cannot reach otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    /// The virtio socket port the guest connects to (host context id 2).
    pub port: u32,
    /// `host:port` on the Mac.
    pub to: String,
}

/// The flags of `run` and `adopt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFlags {
    /// The VM's directory: its disk, seed, firmware variables, identity and sockets.
    pub dir: PathBuf,
    /// Virtual CPUs.
    pub cpus: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
    /// Directories shared into the guest.
    pub shares: Vec<Share>,
    /// Ports carried to TCP addresses on the Mac.
    pub forwards: Vec<Forward>,
    /// The port the guest says it is ready on.
    pub ready_port: u32,
    /// Start the owner as a process of its own and return when the guest is ready.
    pub detach: bool,
    /// Stop the VM as soon as the guest is ready (a boot measurement).
    pub exit_when_ready: bool,
    /// Stop the VM after this many seconds with no guest connection open; 0 never.
    pub idle_stop_s: u64,
    /// Do not boot the VM until a connection needs it.
    pub start_stopped: bool,
}

/// What the verb was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Boot the VM and own it in this process (or, with `--detach`, in a new one).
    Run(RunFlags),
    /// Use the VM a live owner already holds in the directory, or boot one detached; then
    /// stay until stopped, as the agent would.
    Adopt(RunFlags),
    /// Send one request line to the owner's control socket and print the answer.
    Ctl {
        /// The VM's directory.
        dir: PathBuf,
        /// The request line.
        line: String,
    },
    /// Send a request to the guest program on a port, `samples` times, each on a new
    /// connection, and print each reply and the summary of their round trips.
    Call {
        /// The VM's directory.
        dir: PathBuf,
        /// The guest's port.
        port: u32,
        /// The request: a JSON line, where `{n}` is replaced by the sample's number.
        request: String,
        /// How many times.
        samples: u32,
    },
    /// Place (or release) a tree through a mount provider and print its directory.
    Place(super::place::Place),
    /// Run a program `samples` times and print the summary of its wall times.
    Time {
        /// How many times.
        samples: u32,
        /// The program.
        program: String,
        /// Its arguments.
        args: Vec<String>,
    },
}

/// The default number of virtual CPUs.
pub const CPUS: u32 = 4;
/// The default memory, in MiB.
pub const MEMORY_MIB: u64 = 8192;
/// The default port the guest says it is ready on.
pub const READY_PORT: u32 = 1024;

/// The command `args` (the words after the verb) ask for.
///
/// # Errors
///
/// A word is unknown, a value is missing or malformed, or a required flag is absent.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let (command, rest) = args.split_first().ok_or("a command is required")?;
    match command.as_str() {
        "run" => run_flags(rest).map(Command::Run),
        "adopt" => run_flags(rest).map(Command::Adopt),
        "ctl" => {
            let (dir, words) = dir_then_rest(rest)?;
            if words.is_empty() {
                return Err("ctl takes a request".to_owned());
            }
            Ok(Command::Ctl {
                dir,
                line: words.join(" "),
            })
        }
        "call" => call(rest),
        "time" => time(rest),
        "place" => place(rest),
        other => Err(format!("unknown command {other:?}")),
    }
}

/// The value after the flag at `at`.
fn value(args: &[String], at: usize) -> Result<&str, String> {
    args.get(at + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("{} takes a value", args[at]))
}

/// `text` as a number for `flag`.
fn number<T: std::str::FromStr>(flag: &str, text: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag}: {text:?} is not a number"))
}

fn run_flags(args: &[String]) -> Result<RunFlags, String> {
    let mut flags = RunFlags {
        dir: PathBuf::new(),
        cpus: CPUS,
        memory_mib: MEMORY_MIB,
        shares: Vec::new(),
        forwards: Vec::new(),
        ready_port: READY_PORT,
        detach: false,
        exit_when_ready: false,
        idle_stop_s: 0,
        start_stopped: false,
    };
    let mut at = 0;
    while at < args.len() {
        let flag = args[at].as_str();
        match flag {
            "--detach" => flags.detach = true,
            "--exit-when-ready" => flags.exit_when_ready = true,
            "--start-stopped" => flags.start_stopped = true,
            "--dir" | "--cpus" | "--memory-mib" | "--share" | "--forward" | "--ready-port"
            | "--idle-stop-s" => {
                let v = value(args, at)?;
                match flag {
                    "--dir" => flags.dir = PathBuf::from(v),
                    "--cpus" => flags.cpus = number(flag, v)?,
                    "--memory-mib" => flags.memory_mib = number(flag, v)?,
                    "--share" => flags.shares.push(share(v)?),
                    "--forward" => flags.forwards.push(forward(v)?),
                    "--idle-stop-s" => flags.idle_stop_s = number(flag, v)?,
                    _ => flags.ready_port = number(flag, v)?,
                }
                at += 1;
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
        at += 1;
    }
    if flags.dir.as_os_str().is_empty() {
        return Err("--dir is required".to_owned());
    }
    if flags.cpus == 0 || flags.memory_mib == 0 {
        return Err("--cpus and --memory-mib must be more than 0".to_owned());
    }
    Ok(flags)
}

/// `TAG=PATH` or `TAG=PATH:ro`.
fn share(text: &str) -> Result<Share, String> {
    let (tag, path) = text
        .split_once('=')
        .filter(|(tag, path)| !tag.is_empty() && !path.is_empty())
        .ok_or_else(|| format!("--share: {text:?} is not TAG=PATH[:ro]"))?;
    let (path, read_only) = match path.strip_suffix(":ro") {
        Some(path) => (path, true),
        None => (path, false),
    };
    Ok(Share {
        tag: tag.to_owned(),
        path: PathBuf::from(path),
        read_only,
    })
}

/// `PORT=HOST:PORT`.
fn forward(text: &str) -> Result<Forward, String> {
    let bad = || format!("--forward: {text:?} is not PORT=HOST:PORT");
    let (port, to) = text.split_once('=').ok_or_else(bad)?;
    let port = port.parse::<u32>().map_err(|_| bad())?;
    let (host, tcp) = to.rsplit_once(':').ok_or_else(bad)?;
    if host.is_empty() || tcp.parse::<u16>().is_err() {
        return Err(bad());
    }
    Ok(Forward {
        port,
        to: to.to_owned(),
    })
}

/// `--dir D` first, then the rest.
fn dir_then_rest(args: &[String]) -> Result<(PathBuf, &[String]), String> {
    match args {
        [flag, dir, rest @ ..] if flag == "--dir" => Ok((PathBuf::from(dir), rest)),
        _ => Err("--dir D comes first".to_owned()),
    }
}

fn call(args: &[String]) -> Result<Command, String> {
    let (dir, rest) = dir_then_rest(args)?;
    let (mut port, mut request, mut samples) = (None, None, 1u32);
    let mut at = 0;
    while at < rest.len() {
        let flag = rest[at].as_str();
        let v = value(rest, at)?;
        match flag {
            "--port" => port = Some(number::<u32>(flag, v)?),
            "--request" => request = Some(v.to_owned()),
            "--samples" => samples = number(flag, v)?,
            other => return Err(format!("unknown flag {other:?}")),
        }
        at += 2;
    }
    Ok(Command::Call {
        dir,
        port: port.ok_or("--port is required")?,
        request: request.ok_or("--request is required")?,
        samples: samples.max(1),
    })
}

fn place(args: &[String]) -> Result<Command, String> {
    let (mut socket, mut uid, mut reference, mut run, mut release) =
        (None, None, None, None, false);
    let mut at = 0;
    while at < args.len() {
        let flag = args[at].as_str();
        if flag == "--release" {
            release = true;
            at += 1;
            continue;
        }
        let v = value(args, at)?;
        match flag {
            "--socket" => socket = Some(v.to_owned()),
            "--uid" => uid = Some(number::<u32>(flag, v)?),
            "--ref" => reference = Some(v.to_owned()),
            "--run" => run = Some(v.to_owned()),
            other => return Err(format!("unknown flag {other:?}")),
        }
        at += 2;
    }
    Ok(Command::Place(super::place::Place {
        socket: socket.ok_or("--socket is required")?,
        uid: uid.ok_or("--uid is required")?,
        reference: reference.unwrap_or_default(),
        run: run.ok_or("--run is required")?,
        release,
    }))
}

fn time(args: &[String]) -> Result<Command, String> {
    let dashes = args
        .iter()
        .position(|a| a == "--")
        .ok_or("time takes -- PROGRAM [ARGS...]")?;
    let mut samples = 1u32;
    let before = &args[..dashes];
    match before {
        [] => {}
        [flag, v] if flag == "--samples" => samples = number(flag, v)?,
        _ => return Err(format!("unknown flags {before:?}")),
    }
    let (program, rest) = args[dashes + 1..]
        .split_first()
        .ok_or("time takes a program after --")?;
    Ok(Command::Time {
        samples: samples.max(1),
        program: program.clone(),
        args: rest.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    /// Asserts that every flag of `run` lands in its field and the defaults hold otherwise,
    /// and that `adopt` takes the same flags. Catches a flag read into the wrong field.
    #[test]
    fn run_reads_every_flag() {
        let all = parse(&words(
            "run --dir /v --cpus 8 --memory-mib 16384 --share in=/a:ro --share out=/b \
             --forward 2049=127.0.0.1:12179 --ready-port 1100 --detach --exit-when-ready \
             --idle-stop-s 300 --start-stopped",
        ))
        .unwrap();
        assert_eq!(
            all,
            Command::Run(RunFlags {
                dir: "/v".into(),
                cpus: 8,
                memory_mib: 16384,
                shares: vec![
                    Share {
                        tag: "in".into(),
                        path: "/a".into(),
                        read_only: true
                    },
                    Share {
                        tag: "out".into(),
                        path: "/b".into(),
                        read_only: false
                    },
                ],
                forwards: vec![Forward {
                    port: 2049,
                    to: "127.0.0.1:12179".into()
                }],
                ready_port: 1100,
                detach: true,
                exit_when_ready: true,
                idle_stop_s: 300,
                start_stopped: true,
            })
        );
        assert_eq!(
            parse(&words("adopt --dir /v")),
            Ok(Command::Adopt(RunFlags {
                dir: "/v".into(),
                cpus: CPUS,
                memory_mib: MEMORY_MIB,
                shares: vec![],
                forwards: vec![],
                ready_port: READY_PORT,
                detach: false,
                exit_when_ready: false,
                idle_stop_s: 0,
                start_stopped: false,
            }))
        );
    }

    /// Asserts the refusal of each malformed `run`, by its exact message. Catches a VM booted
    /// from a missing directory, with no CPU, or with a share or forward it cannot make.
    #[test]
    fn malformed_runs_are_refused_by_name() {
        let cases = [
            ("", "a command is required"),
            ("boot", "unknown command \"boot\""),
            ("run", "--dir is required"),
            ("run --dir", "--dir takes a value"),
            (
                "run --dir /v --cpus 0",
                "--cpus and --memory-mib must be more than 0",
            ),
            ("run --dir /v --cpus x", "--cpus: \"x\" is not a number"),
            ("run --dir /v --bogus", "unknown flag \"--bogus\""),
            (
                "run --dir /v --share =/a",
                "--share: \"=/a\" is not TAG=PATH[:ro]",
            ),
            (
                "run --dir /v --share tag",
                "--share: \"tag\" is not TAG=PATH[:ro]",
            ),
            (
                "run --dir /v --forward 2049",
                "--forward: \"2049\" is not PORT=HOST:PORT",
            ),
            (
                "run --dir /v --forward x=127.0.0.1:1",
                "--forward: \"x=127.0.0.1:1\" is not PORT=HOST:PORT",
            ),
            (
                "run --dir /v --forward 1=127.0.0.1",
                "--forward: \"1=127.0.0.1\" is not PORT=HOST:PORT",
            ),
            (
                "run --dir /v --forward 1=:80",
                "--forward: \"1=:80\" is not PORT=HOST:PORT",
            ),
            (
                "run --dir /v --forward 1=h:99999",
                "--forward: \"1=h:99999\" is not PORT=HOST:PORT",
            ),
        ];
        for (line, why) in cases {
            assert_eq!(parse(&words(line)), Err(why.to_owned()), "{line:?}");
        }
    }

    /// Asserts `place` with and without `--release`, and its refusals. Catches a tree placed
    /// for no run or by a provider of no named user.
    #[test]
    fn place_reads_its_flags() {
        assert_eq!(
            parse(&words("place --socket /s --uid 7 --ref cas/l/ab --run r1")).unwrap(),
            Command::Place(crate::linux_vm::place::Place {
                socket: "/s".into(),
                uid: 7,
                reference: "cas/l/ab".into(),
                run: "r1".into(),
                release: false
            })
        );
        assert_eq!(
            parse(&words("place --release --socket /s --uid 7 --run r1")),
            Ok(Command::Place(crate::linux_vm::place::Place {
                socket: "/s".into(),
                uid: 7,
                reference: String::new(),
                run: "r1".into(),
                release: true
            }))
        );
        for (line, why) in [
            ("place --uid 7 --run r", "--socket is required"),
            ("place --socket /s --run r", "--uid is required"),
            ("place --socket /s --uid 7", "--run is required"),
            (
                "place --socket /s --uid x --run r",
                "--uid: \"x\" is not a number",
            ),
            ("place --who 1", "unknown flag \"--who\""),
        ] {
            assert_eq!(parse(&words(line)), Err(why.to_owned()), "{line:?}");
        }
    }

    /// Asserts `ctl`, `call` and `time` with their values and defaults, and their refusals.
    /// Catches a call with no port sent to port 0, or a timed program missing its arguments.
    #[test]
    fn ctl_call_and_time_read_their_words() {
        assert_eq!(
            parse(&words("ctl --dir /v balloon 4096")).unwrap(),
            Command::Ctl {
                dir: "/v".into(),
                line: "balloon 4096".into()
            }
        );
        assert_eq!(
            parse(&words(
                "call --dir /v --port 7000 --request {} --samples 30"
            ))
            .unwrap(),
            Command::Call {
                dir: "/v".into(),
                port: 7000,
                request: "{}".into(),
                samples: 30
            }
        );
        assert_eq!(
            parse(&words("call --dir /v --port 7000 --request {} --samples 0")).unwrap(),
            Command::Call {
                dir: "/v".into(),
                port: 7000,
                request: "{}".into(),
                samples: 1
            }
        );
        assert_eq!(
            parse(&words("time --samples 5 -- ssh -o x host true")).unwrap(),
            Command::Time {
                samples: 5,
                program: "ssh".into(),
                args: words("-o x host true")
            }
        );
        assert_eq!(
            parse(&words("time -- true")).unwrap(),
            Command::Time {
                samples: 1,
                program: "true".into(),
                args: vec![]
            }
        );
        let cases = [
            ("ctl --dir /v", "ctl takes a request"),
            ("ctl state", "--dir D comes first"),
            ("ctl --where /v state", "--dir D comes first"),
            ("call --dir /v --request {}", "--port is required"),
            ("call --dir /v --port 1", "--request is required"),
            ("call --dir /v --port", "--port takes a value"),
            ("call --dir /v --who 1", "unknown flag \"--who\""),
            ("time true", "time takes -- PROGRAM [ARGS...]"),
            ("time --", "time takes a program after --"),
            ("time --n 3 -- true", "unknown flags [\"--n\", \"3\"]"),
            (
                "time --samples x -- true",
                "--samples: \"x\" is not a number",
            ),
        ];
        for (line, why) in cases {
            assert_eq!(parse(&words(line)), Err(why.to_owned()), "{line:?}");
        }
    }
}
