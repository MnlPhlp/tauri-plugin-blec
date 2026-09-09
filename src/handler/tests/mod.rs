//! Tests driving the [`Handler`](super::Handler) through the mock backend.
//!
//! Run with `RUST_LOG=trace cargo test -- --nocapture` to see the handler's logs.

mod connect;
mod disconnect;
mod helpers;
mod notify;
mod scan;
mod stress;
