//! A server that answers with something the client cannot use is not one to wait for. Added
//! with the move to this repository: the answer here has no SQLSTATE and no I/O error among
//! its causes, a case the open path classifies but no test reached.
#![cfg(feature = "postgres")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use js_state::postgres::{PostgresConfig, PostgresStore};

/// Asserts that a server that asks for Kerberos, which the client does not speak, is refused
/// and not reported as a server that is not up yet.
#[test]
fn a_server_that_asks_for_an_unsupported_login_is_refused_not_waited_for() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut startup = [0; 512];
        let _ = stream.read(&mut startup).expect("the startup message");
        // AuthenticationKerberosV5: 'R', length 8, code 2.
        stream
            .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 2])
            .expect("answer");
    });
    let config = PostgresConfig::parse(&format!("postgres://u@127.0.0.1:{port}/state"))
        .expect("the URL parses");
    let err = PostgresStore::try_open(&config).err().expect("refused");
    assert!(!err.is_not_up(), "{err:?}");
    server.join().expect("the server thread");
}
