//! Host ports over a real `crun`, as root: a job at level `none`, which has its own loopback
//! and nothing else, reaches a listener of the test's on the host's loopback through the job
//! port the agent listens on in the job's network namespace, both ways; the job's program is
//! started only once that listener is there (its init waits), so it never races it.
//!
//! The job is busybox's `wget` against the test's listener, which answers one HTTP request:
//! the request line shows what the job sent, and the body the job saved in the run's
//! directory of the test (`sync`) shows what it read. Nothing waits for a clock.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use js_agent::lifecycle::{Body, Exit, Runtime, Status};
use js_agent_wire::{HostPort, PortProtocol};

use crate::runtime_suite::{Program, Subject, spec_of};
use crate::subject::crun_subject;

/// The port of the job's own loopback it connects to: in the job's network namespace alone,
/// so no other process of the host can hold it.
const JOB_PORT: u16 = 8080;

/// Proves a job at level `none` reaches the host port at a job port of its own: the agent's
/// listener is in the job's namespace (entered from a thread of its own, bound before the
/// job's loopback may be up), the job's request arrives at the test's listener on the host's
/// loopback, the answer reaches the job, and the job ends 0 having run once the listener was
/// there. Catches a listener made in the host's namespace (the job would reach nothing), a
/// bind that fails while `crun` has not brought the loopback up, and an init that starts the
/// program before the listener exists.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_job_with_no_network_reaches_a_host_port_at_its_own_port() {
    let mut s = crun_subject("host-ports");
    let host = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let host_port = host.local_addr().expect("an address").port();
    let run = "hp1";
    let sync = s.sync_dir(run);
    let mut spec = spec_of(&mut s, run, Program::Hold);
    spec.host_ports = vec![HostPort {
        host_port,
        container_port: Some(JOB_PORT),
        protocol: PortProtocol::Tcp,
    }];
    let script =
        format!("wget -q -O /mnt/sync/{run}/answer http://127.0.0.1:{JOB_PORT}/from-the-job");
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
    };
    let mut runtime = s.open_as("host-ports");
    let id = runtime.create_and_start(&spec).expect("create and start");
    let (served, _) = host.accept().expect("the job reaches the host port");
    let mut request = String::new();
    let mut reader = BufReader::new(served);
    reader.read_line(&mut request).expect("the request line");
    assert!(request.starts_with("GET /from-the-job "), "{request:?}");
    let mut served = reader.into_inner();
    served
        .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 13\r\n\r\nfrom-the-host")
        .expect("answer");
    drop(served);
    runtime.wait_ended(&id);
    let ended = Status::Exited(Exit {
        code: 0,
        oom_killed: false,
    });
    assert_eq!(runtime.poll(&id), Ok(ended));
    let answer = fs::read_to_string(sync.join("answer")).expect("the job saved the answer");
    assert_eq!(answer, "from-the-host");
    runtime.remove(&id).expect("removed");
}
