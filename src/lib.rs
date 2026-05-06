//! chitta-rs — agent-native persistent memory server.
//!
//! The library side exists so integration tests and `main.rs` share types.
//! Principles live in `rust/docs/principles.md`; scope in `starting-shape.md`.

pub mod config;
pub mod db;
pub mod embedding;
pub mod envelope;
pub mod error;
pub mod ingest;
pub mod mcp;
pub mod retrieval;
pub mod tools;
