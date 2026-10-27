//! Behaviour of the lease server through its public interface and over real sockets on
//! 127.0.0.1: the routes, the authentication, the refusals to start, the tick loop,
//! TLS, and the program itself run as a child process; and, with agent registration on,
//! the agent routes, the layering of the sources of hosts, and the restart.

mod agents;
mod agents_restart;
mod agents_support;
mod agents_tokens;
mod auth;
mod binary;
mod limits;
mod restart;
mod routes;
mod startup;
mod support;
mod ticks;
mod tls;
