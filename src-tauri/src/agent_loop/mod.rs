//! Rust-native execution primitives for the durable agent loop.
//!
//! The desktop task snapshot remains the UI read model. These modules own the
//! smaller, lossless lifecycle protocol that makes a running task observable,
//! bounded, and recoverable without adding a runtime dependency.

pub(crate) mod compaction;
pub(crate) mod context;
pub(crate) mod events;
pub(crate) mod provenance;
pub(crate) mod retry;
pub(crate) mod scheduler;
pub(crate) mod summary;
pub(crate) mod surface;
