//! Tests driving the [`Handler`](super::Handler) through the mock backend.
//!
//! Run with `RUST_LOG=trace cargo test -- --nocapture` to see the handler's logs.

mod connect;
mod disconnect;
mod helpers;
mod multi;
mod notify;
mod runtime;
mod scan;
mod stress;
