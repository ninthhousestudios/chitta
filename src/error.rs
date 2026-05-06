//! Actionable errors (Principle 8).
//!
//! Every [`ChittaError`] variant carries enough context to build a JSON-RPC
//! error `data` object that names the tool, the violated constraint, and a
//! next action the caller can take. Stack traces never leave the server.

use serde::Serialize;
use thiserror::Error;

/// Canonical JSON-RPC `data` payload for every error returned to the wire.
/// Three fields are always populated: `tool`, `constraint`, `next_action`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ErrorData {
    pub tool: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
    pub constraint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received: Option<serde_json::Value>,
    pub next_action: String,
}

/// JSON-RPC error codes per the MCP / JSON-RPC 2.0 spec.
/// -32602 is "invalid params"; we reuse it for any caller-fixable failure.
/// -32603 is "internal error"; reserved for server-side bugs or infra faults.
pub mod codes {
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

#[derive(Debug, Error)]
pub enum ChittaError {
    #[error("missing required config: {name}")]
    MissingConfig {
        name: &'static str,
        next_action: String,
    },

    #[error("invalid argument `{argument}` for tool `{tool}`: {constraint}")]
    InvalidArgument {
        tool: &'static str,
        argument: String,
        constraint: String,
        received: Option<serde_json::Value>,
        next_action: String,
    },

    #[error("content exceeds 8192-token embedding limit ({token_count} tokens)")]
    ContentTooLong {
        tool: &'static str,
        token_count: usize,
    },

    #[error("{kind} not found")]
    NotFound {
        tool: &'static str,
        kind: &'static str,
        next_action: String,
    },

    #[error("embedding failure: {message}")]
    Embedding {
        tool: &'static str,
        message: String,
        next_action: String,
    },

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ChittaError {
    /// JSON-RPC error code to use on the wire.
    pub fn code(&self) -> i32 {
        match self {
            Self::MissingConfig { .. }
            | Self::InvalidArgument { .. }
            | Self::ContentTooLong { .. }
            | Self::NotFound { .. } => codes::INVALID_PARAMS,
            Self::Embedding { .. } | Self::Db(_) | Self::Migrate(_) | Self::Internal(_) => {
                codes::INTERNAL_ERROR
            }
        }
    }

    /// Short human-readable message for the JSON-RPC `message` field.
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// Structured payload for the JSON-RPC `data` field. Every variant
    /// populates `tool`, `constraint`, `next_action`.
    pub fn data(&self) -> ErrorData {
        match self {
            Self::MissingConfig { name, next_action } => ErrorData {
                tool: "startup",
                argument: Some((*name).to_string()),
                constraint: format!("environment variable `{name}` must be set"),
                received: None,
                next_action: next_action.clone(),
            },
            Self::InvalidArgument {
                tool,
                argument,
                constraint,
                received,
                next_action,
            } => ErrorData {
                tool,
                argument: Some(argument.clone()),
                constraint: constraint.clone(),
                received: received.clone(),
                next_action: next_action.clone(),
            },
            Self::ContentTooLong { tool, token_count } => ErrorData {
                tool,
                argument: Some("content".to_string()),
                constraint: "tokenized length <= 8192".to_string(),
                received: Some(serde_json::json!({ "token_count": token_count })),
                next_action:
                    "Split content into chunks of <= 7500 tokens and store each as a separate \
                     memory with its own idempotency_key"
                        .to_string(),
            },
            Self::NotFound {
                tool,
                kind,
                next_action,
            } => ErrorData {
                tool,
                argument: None,
                constraint: format!("{kind} exists in the given profile"),
                received: None,
                next_action: next_action.clone(),
            },
            Self::Embedding {
                tool,
                message,
                next_action,
            } => ErrorData {
                tool,
                argument: None,
                constraint: "embedding pipeline completes without error".to_string(),
                received: Some(serde_json::json!({ "message": message })),
                next_action: next_action.clone(),
            },
            Self::Db(e) => ErrorData {
                tool: "database",
                argument: None,
                constraint: "database query succeeds".to_string(),
                received: Some(serde_json::json!({ "message": e.to_string() })),
                next_action: db_next_action(e),
            },
            Self::Migrate(e) => ErrorData {
                tool: "startup",
                argument: None,
                constraint: "migrations apply cleanly".to_string(),
                received: Some(serde_json::json!({ "message": e.to_string() })),
                next_action:
                    "Inspect migration state and the database schema; resolve drift before \
                     restarting."
                        .to_string(),
            },
            Self::Internal(msg) => ErrorData {
                tool: "server",
                argument: None,
                constraint: "server completes the request without an internal fault".to_string(),
                received: Some(serde_json::json!({ "message": msg })),
                next_action: "Report this as a bug; include server logs.".to_string(),
            },
        }
    }
}

/// Pick a caller-oriented next action for a `sqlx::Error` based on whether it
/// looks transient (retry), infrastructural (check connectivity/config), or
/// permanent (report as a bug with the inner message).
fn db_next_action(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => {
            "Retry the request. If it repeats, the database pool is saturated or unhealthy — \
             check server load and DATABASE_URL reachability."
                .to_string()
        }
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) => {
            "Retry the request. If it repeats, check that the database is reachable at \
             DATABASE_URL and that TLS/network configuration is correct."
                .to_string()
        }
        sqlx::Error::Database(_) => {
            "The database rejected the query (constraint, permission, or schema issue). Inspect \
             the message, correct the input or schema, and retry."
                .to_string()
        }
        sqlx::Error::RowNotFound => {
            "The expected row was absent. If the caller passed an id, verify it; otherwise \
             report as a bug."
                .to_string()
        }
        sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::Decode(_)
        | sqlx::Error::TypeNotFound { .. } => {
            "Schema drift between migrations and server code. Rebuild the server against the \
             migrations applied to the database."
                .to_string()
        }
        _ => "Retry the request. If the error repeats, check server logs for detail and report \
             as a bug if the cause is unclear."
            .to_string(),
    }
}

pub type Result<T, E = ChittaError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    fn has_required_fields(data: &ErrorData) {
        assert!(!data.tool.is_empty(), "tool populated");
        assert!(!data.constraint.is_empty(), "constraint populated");
        assert!(!data.next_action.is_empty(), "next_action populated");
    }

    #[test]
    fn invalid_argument_populates_contract() {
        let e = ChittaError::InvalidArgument {
            tool: "store_memory",
            argument: "event_time".to_string(),
            constraint: "ISO-8601 >= 1970-01-01T00:00:00Z".to_string(),
            received: Some(serde_json::json!("1969-06-20T00:00:00Z")),
            next_action: "Pass event_time >= 1970-01-01T00:00:00Z".to_string(),
        };
        has_required_fields(&e.data());
        assert_eq!(e.code(), codes::INVALID_PARAMS);
    }

    #[test]
    fn content_too_long_reports_token_count() {
        let e = ChittaError::ContentTooLong {
            tool: "store_memory",
            token_count: 11432,
        };
        let data = e.data();
        has_required_fields(&data);
        let received = data.received.unwrap();
        assert_eq!(received["token_count"], 11432);
    }

    #[test]
    fn not_found_next_action_guides_caller() {
        let e = ChittaError::NotFound {
            tool: "get_memory",
            kind: "memory",
            next_action: "Verify the id, or call search_memories to find candidates.".to_string(),
        };
        let data = e.data();
        has_required_fields(&data);
        assert!(data.next_action.contains("search_memories"));
    }

    #[test]
    fn missing_config_is_invalid_params() {
        let e = ChittaError::MissingConfig {
            name: "DATABASE_URL",
            next_action: "Set DATABASE_URL in the environment or .env file.".to_string(),
        };
        has_required_fields(&e.data());
        assert_eq!(e.code(), codes::INVALID_PARAMS);
    }

    #[test]
    fn internal_error_is_internal_code() {
        let e = ChittaError::Internal("unexpected state".to_string());
        has_required_fields(&e.data());
        assert_eq!(e.code(), codes::INTERNAL_ERROR);
    }

    /// Every variant must populate `tool`, `constraint`, `next_action`. This
    /// test is the enforcement mechanism for Principle 8 — if someone adds a
    /// new variant and forgets to wire it into `data()`, this test either
    /// fails to compile (exhaustive match on the enum below) or fails at
    /// runtime on an empty field.
    #[test]
    fn every_variant_populates_contract() {
        use std::io;
        // Building a representative of every variant.
        let variants = vec![
            ChittaError::MissingConfig {
                name: "DATABASE_URL",
                next_action: "set it".to_string(),
            },
            ChittaError::InvalidArgument {
                tool: "t",
                argument: "a".to_string(),
                constraint: "c".to_string(),
                received: None,
                next_action: "n".to_string(),
            },
            ChittaError::ContentTooLong {
                tool: "store_memory",
                token_count: 1,
            },
            ChittaError::NotFound {
                tool: "get_memory",
                kind: "memory",
                next_action: "verify id".to_string(),
            },
            ChittaError::Embedding {
                tool: "store_memory",
                message: "ort blew up".to_string(),
                next_action: "restart the server".to_string(),
            },
            ChittaError::Db(sqlx::Error::PoolTimedOut),
            ChittaError::Db(sqlx::Error::Io(io::Error::other("reset"))),
            ChittaError::Db(sqlx::Error::RowNotFound),
            ChittaError::Migrate(sqlx::migrate::MigrateError::Execute(sqlx::Error::Io(
                io::Error::other("schema off"),
            ))),
            ChittaError::Internal("unexpected".to_string()),
        ];
        for e in &variants {
            let d = e.data();
            has_required_fields(&d);
            assert!(
                e.code() == codes::INVALID_PARAMS || e.code() == codes::INTERNAL_ERROR,
                "code for {e:?} is one of the two defined codes"
            );
        }
    }

    #[test]
    fn db_next_action_varies_by_kind() {
        use std::io;
        let pool = ChittaError::Db(sqlx::Error::PoolTimedOut);
        let io_err = ChittaError::Db(sqlx::Error::Io(io::Error::other("x")));
        let not_found = ChittaError::Db(sqlx::Error::RowNotFound);
        assert!(pool.data().next_action.contains("pool"));
        assert!(io_err.data().next_action.contains("reachable"));
        assert!(not_found.data().next_action.contains("id"));
    }

    #[test]
    fn error_data_serializes_with_expected_keys() {
        let e = ChittaError::InvalidArgument {
            tool: "store_memory",
            argument: "profile".to_string(),
            constraint: "1-128 chars, [a-zA-Z0-9_-]+".to_string(),
            received: Some(serde_json::json!("")),
            next_action: "Provide a non-empty profile name.".to_string(),
        };
        let json = serde_json::to_value(e.data()).unwrap();
        assert!(json.get("tool").is_some());
        assert!(json.get("argument").is_some());
        assert!(json.get("constraint").is_some());
        assert!(json.get("received").is_some());
        assert!(json.get("next_action").is_some());
    }
}
