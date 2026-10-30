//! Behaviour of the lease server through its public interface and over real sockets on
//! 127.0.0.1: the routes, the authentication, the refusals to start, the tick loop,
//! TLS, and the program itself run as a child process; and, with agent registration on,
//! the agent routes, the layering of the sources of hosts, the restart, and the container
//! runs a lease holder queues on a registered host.

mod agents;
mod agents_contract;
mod agents_load;
mod agents_no_shared_token;
mod agents_restart;
mod agents_support;
mod agents_tokens;
mod agents_whole;
mod auth;
mod binary;
mod config_file;
mod config_joined;
mod config_launch;
mod config_lifetime;
mod config_one_file;
mod config_pool_refusals;
mod config_refusals;
mod config_serve;
mod config_support;
mod config_usage;
mod elastic_fake_e2e;
mod gpu_path_pin;
mod guard_names;
mod joins;
mod joins_credential;
mod joins_revoke;
mod lambda_e2e;
mod lambda_e2e_failures;
mod lambda_e2e_support;
mod layering;
mod limits;
mod memory_percent;
mod pools_checks;
mod pools_policy;
mod pools_provider_checks;
mod pools_status;
mod pools_support;
mod restart;
mod routes;
mod runs;
mod runs_files;
mod runs_more;
mod runs_routes;
mod runs_support;
mod scaler_network;
mod scaler_registry;
mod scaler_start;
mod scaler_status;
mod scaler_support;
mod scaler_unremoved;
mod startup;
mod support;
#[allow(dead_code)]
#[path = "../network/fake.rs"]
mod tailnet;
mod ticks;
mod timings;
mod tls;
mod usage;
mod usage_keys;
mod usage_support;
