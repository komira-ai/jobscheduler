//! What the black-box test does on the runner: run a program, write a file with its owner and
//! mode, ask a plain-HTTP route on the loopback. Each failure says what it was doing.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{PermissionsExt, chown};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// How long one plain-HTTP probe may take to connect, and then to be answered.
const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// Runs `program` with `args` and the environment of a non-interactive install, and returns
/// what it printed on its standard output.
///
/// # Errors
///
/// A line with the command, its exit status and what it printed on its standard error.
pub fn try_run(program: &str, args: &[&str]) -> Result<String, String> {
    try_run_in(None, program, args)
}

/// [`run`] in the working directory `dir`.
pub fn run_in(dir: &Path, program: &str, args: &[&str]) -> String {
    try_run_in(Some(dir), program, args).unwrap_or_else(|e| panic!("{e}"))
}

fn try_run_in(dir: Option<&Path>, program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .map_err(|e| format!("{program} {}: cannot start: {e}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "{program} {}: {}\nstdout: {stdout}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// [`try_run`], failing the test when the program fails.
pub fn run(program: &str, args: &[&str]) -> String {
    try_run(program, args).unwrap_or_else(|e| panic!("{e}"))
}

/// Writes `contents` to `path` (making its directory), with `mode`, owned by `owner`
/// (uid, gid).
pub fn put(path: &Path, contents: impl AsRef<[u8]>, mode: u32, owner: (u32, u32)) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("make {}: {e}", parent.display()));
    }
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    own(path, mode, owner);
}

/// Makes the directory `path` (and its parents), with `mode`, owned by `owner`.
pub fn directory(path: &Path, mode: u32, owner: (u32, u32)) {
    fs::create_dir_all(path).unwrap_or_else(|e| panic!("make {}: {e}", path.display()));
    own(path, mode, owner);
}

fn own(path: &Path, mode: u32, (uid, gid): (u32, u32)) {
    chown(path, Some(uid), Some(gid)).unwrap_or_else(|e| panic!("chown {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// `GET path` on `addr` over plain HTTP: the status and the body.
///
/// # Errors
///
/// A line saying why there is no answer (nothing listening, no answer within
/// [`PROBE_LIMIT`], an answer that is not HTTP).
pub fn http_get(addr: SocketAddr, path: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect_timeout(&addr, PROBE_LIMIT)
        .map_err(|e| format!("GET {addr}{path}: {e}"))?;
    stream
        .set_read_timeout(Some(PROBE_LIMIT))
        .and_then(|()| stream.set_write_timeout(Some(PROBE_LIMIT)))
        .map_err(|e| format!("GET {addr}{path}: {e}"))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("GET {addr}{path}: {e}"))?;
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .map_err(|e| format!("GET {addr}{path}: {e}"))?;
    let status = answer
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("GET {addr}{path}: not an HTTP answer: {answer:?}"))?;
    let body = answer
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .trim()
        .to_owned();
    Ok((status, body))
}
