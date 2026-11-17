//! soju is a user-friendly IRC bouncer.

pub mod auth;
pub mod certfp;
pub mod config;
pub mod conn;
pub mod database;
pub mod downstream;
pub mod flags;
pub mod identd;
pub mod irc;
pub mod ircutil;
pub mod listen;
pub mod logger;
pub mod metrics;
pub mod msgstore;
pub mod proxyproto;
pub mod rate;
pub mod scfg;
pub mod server;
pub mod service;
pub mod tls;
pub mod upstream;
pub mod user;
pub mod util;
pub mod webpush;
pub mod xirc;

/// The soju-rs version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Counts a downstream connection until serving ends, including cancellation.
pub async fn metrics_guard<F: std::future::Future>(srv: &server::Server, f: F) -> F::Output {
    metrics::with_gauge(&srv.metrics.downstreams, f).await
}
