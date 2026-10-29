//! The network layer and the boot renderer, through their public interface: the
//! Tailscale client against an in-process server on 127.0.0.1 (never the real API), the
//! renderer's golden text and limits, and the rule that a provider sees a payload and no
//! network type. Time is an injected clock; no test measures elapsed time.

mod default_mode;
mod fake;
mod golden;
mod lambda_host;
mod metadata_block;
mod oauth_file;
mod one_layer;
mod render;
mod render_limits;
mod support;
mod tailscale_errors;
mod tailscale_forget;
mod tailscale_mint;
mod tailscale_setup;
mod userns;
