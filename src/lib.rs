//! chitta — working model of Josh.
//!
//! The library side exists so integration tests and `main.rs` share types.
//! See `docs/plans/working-model-prd.md` for the v0 design.

pub mod config;
pub mod consolidated;
pub mod db;
pub mod embedding;
pub mod envelope;
pub mod error;
pub mod facets;
pub mod ingest;
pub mod llm;
pub mod mcp;
pub mod reflect;
pub mod retrieval;
pub mod synthesis;
pub mod tools;
pub mod validators;
