//! Behaviour of the lease server through its public interface and over real sockets on
//! 127.0.0.1: the routes, the authentication, the refusals to start, the tick loop,
//! TLS, and the program itself run as a child process; and, with agent registration on,
//! the agent routes, the layering of the sources of hosts, the restart, and the container
//! runs a lease holder queues on a registered host.

mod agents;
mod agents_contract;
mod agents_restart;
mod agents_support;
mod agents_tokens;
mod agents_whole;
mod auth;
mod binary;
mod gpu_path_pin;
mod joins;
mod joins_credential;
mod joins_revoke;
mod limits;
mod restart;
mod routes;
mod runs;
mod runs_more;
mod runs_routes;
mod runs_support;
mod startup;
mod support;
mod ticks;
mod tls;
