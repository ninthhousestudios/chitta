//! JSON Schema overrides for `serde_json::Value` fields.
//!
//! `serde_json::Value` produces a typeless schema (`true`), which causes MCP
//! clients to stringify nested objects/arrays. These helpers provide explicit
//! type hints so clients serialize them correctly.

use schemars::{json_schema, Schema, SchemaGenerator};

pub fn nullable_object(_gen: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": ["object", "null"]
    })
}

pub fn nullable_array(_gen: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": ["array", "null"]
    })
}
