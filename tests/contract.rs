//! L0 contract tests: pure schema / serde shape — no DB, no subprocess.
//!
//! These tests lock in the wire contract described in
//! `rust/docs/starting-shape.md`. If a field is renamed, removed, or its type
//! changes, a test here fails loudly before integration tests or a caller
//! even notice.

use chitta::envelope::Envelope;
use chitta::error::{ChittaError, codes};
use chitta::facets::Facets;
use chitta::tools::{
    DeleteArgs, DeleteOutput, DerivationInput, GetArgs, GetOutput, GetProfileArgs,
    GetProfileOutput, ListArgs, ListItem, ListOutput, ReflectStatusArgs, SearchArgs, SearchHit,
    SearchOutput, StoreArgs, StoreOutput, SupersedeArgs, SupersedeOutput, UpdateArgs, UpdateOutput,
};
use serde_json::{Value, json};

/// Helper: assert that `value` is a JSON object and every `key` is present.
fn assert_keys(value: &Value, keys: &[&str]) {
    let obj = value.as_object().expect("object");
    for k in keys {
        assert!(obj.contains_key(*k), "missing key `{k}` in {value}");
    }
}

// ---- Arguments (wire -> struct) -------------------------------------

#[test]
fn store_args_accepts_minimum_fields() {
    let v = json!({
        "profile": "p",
        "content": "hello",
        "idempotency_key": "k",
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "p");
    assert_eq!(args.content, "hello");
    assert_eq!(args.idempotency_key, "k");
    assert!(args.event_time.is_none());
    assert!(args.tags.is_none());
}

#[test]
fn store_args_accepts_full_payload() {
    let v = json!({
        "profile": "p",
        "content": "hello",
        "idempotency_key": "k",
        "event_time": "2026-01-02T03:04:05Z",
        "tags": ["alpha", "beta"],
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    assert!(args.event_time.is_some());
    assert_eq!(
        args.tags.unwrap(),
        vec!["alpha".to_string(), "beta".to_string()]
    );
}

#[test]
fn get_args_shape() {
    let v = json!({"profile": "p", "id": "7e…"});
    let args: GetArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "p");
    assert_eq!(args.id, "7e…");
}

#[test]
fn search_args_all_optional_except_required() {
    let v = json!({"profile": "p", "query": "q"});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert!(args.k.is_none());
    assert!(args.max_tokens.is_none());
    assert!(args.tags.is_none());
    assert!(args.min_similarity.is_none());
}

// ---- Outputs (struct -> wire) ---------------------------------------

#[test]
fn store_output_wire_keys() {
    let t = chrono::Utc::now();
    let out = StoreOutput {
        id: uuid::Uuid::now_v7(),
        profile: "p".into(),
        content: "c".into(),
        event_time: t,
        record_time: t,
        tags: vec![],
        metadata: Some(json!({"k": "v"})),
        memory_type: "observation".into(),
        external_refs: None,
        facets: Facets::default(),
        confidence: None,
        source: None,
        idempotent_replay: false,
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(
        &v,
        &[
            "id",
            "profile",
            "content",
            "event_time",
            "record_time",
            "tags",
            "metadata",
            "memory_type",
            "idempotent_replay",
        ],
    );
    assert_eq!(v["idempotent_replay"], json!(false));
}

#[test]
fn get_output_wire_keys() {
    let t = chrono::Utc::now();
    let out = GetOutput {
        id: uuid::Uuid::now_v7(),
        profile: "p".into(),
        content: "c".into(),
        event_time: t,
        record_time: t,
        tags: vec!["x".into()],
        metadata: None,
        memory_type: "observation".into(),
        source: None,
        external_refs: None,
        facets: Facets::default(),
        superseded_by: None,
        confidence: None,
        reinforcement_count: 0,
        last_reinforced_at: None,
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(
        &v,
        &[
            "id",
            "profile",
            "content",
            "event_time",
            "record_time",
            "tags",
            "memory_type",
        ],
    );
}

#[test]
fn search_output_envelope_shape() {
    let t = chrono::Utc::now();
    let hit = SearchHit {
        id: uuid::Uuid::now_v7(),
        snippet: "snip".into(),
        similarity: 0.88,
        score: 0.88,
        event_time: t,
        record_time: t,
        tags: vec![],
        content: None,
        metadata: None,
        memory_type: "observation".into(),
        source: None,
        external_refs: None,
        confidence: None,
        effective_score: None,
        layer: "raw".into(),
    };
    let env: SearchOutput = Envelope::new(vec![hit], false, Some(1), 42);
    let v = serde_json::to_value(&env).unwrap();
    assert_keys(
        &v,
        &[
            "results",
            "truncated",
            "total_available",
            "budget_spent_tokens",
        ],
    );
    let first = &v["results"][0];
    assert_keys(
        first,
        &[
            "id",
            "snippet",
            "similarity",
            "score",
            "event_time",
            "record_time",
            "tags",
            "memory_type",
            "layer",
        ],
    );
}

// ---- Error contract ------------------------------------------------

/// Every error must carry `tool`, `constraint`, `next_action` on the wire.
/// This is Principle 8's enforcement from the caller's perspective — it
/// matches `error::tests::every_variant_populates_contract`, but serializes
/// through `serde_json::to_value` to catch any accidental skip-serialize
/// attribute that would hide a field from the wire.
#[test]
fn every_error_variant_serializes_with_contract_fields() {
    use std::io;

    let variants = vec![
        ChittaError::MissingConfig {
            name: "DATABASE_URL",
            next_action: "set it".to_string(),
        },
        ChittaError::InvalidArgument {
            tool: "store_memory",
            argument: "profile".to_string(),
            constraint: "1-128 chars".to_string(),
            received: Some(json!("")),
            next_action: "pass a profile".to_string(),
        },
        ChittaError::ContentTooLong {
            tool: "store_memory",
            token_count: 9001,
        },
        ChittaError::NotFound {
            tool: "get_memory",
            kind: "memory",
            next_action: "verify id".to_string(),
        },
        ChittaError::Embedding {
            tool: "store_memory",
            message: "ort error".to_string(),
            next_action: "restart".to_string(),
        },
        ChittaError::Db(sqlx::Error::PoolTimedOut),
        ChittaError::Db(sqlx::Error::Io(io::Error::other("connection reset"))),
        ChittaError::Migrate(sqlx::migrate::MigrateError::Execute(sqlx::Error::Io(
            io::Error::other("drift"),
        ))),
        ChittaError::Internal("unexpected".to_string()),
    ];

    for e in &variants {
        let data = serde_json::to_value(e.data()).unwrap();
        let obj = data.as_object().expect("object");

        let tool = obj.get("tool").and_then(|v| v.as_str()).unwrap_or("");
        let constraint = obj.get("constraint").and_then(|v| v.as_str()).unwrap_or("");
        let next_action = obj
            .get("next_action")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(!tool.is_empty(), "empty `tool` for {e:?}");
        assert!(!constraint.is_empty(), "empty `constraint` for {e:?}");
        assert!(!next_action.is_empty(), "empty `next_action` for {e:?}");

        let code = e.code();
        assert!(
            code == codes::INVALID_PARAMS || code == codes::INTERNAL_ERROR,
            "unexpected code {code} for {e:?}"
        );
    }
}

#[test]
fn error_data_skip_serializes_none_fields() {
    let e = ChittaError::NotFound {
        tool: "get_memory",
        kind: "memory",
        next_action: "verify id".to_string(),
    };
    let v = serde_json::to_value(e.data()).unwrap();
    // `argument` and `received` are None for NotFound; they should be
    // absent from the wire payload (not serialized as null).
    assert!(!v.as_object().unwrap().contains_key("argument"));
    assert!(!v.as_object().unwrap().contains_key("received"));
}

// ---- JSON-RPC wire mapping (chitta_to_rmcp) -------------------------
//
// Walk every variant through the mcp-side mapper and assert that the
// resulting `ErrorData` serializes with the JSON-RPC code we expect and a
// `data` payload carrying the Principle 8 triple (`tool`, `constraint`,
// `next_action`). If the mapper drops a field or misroutes a code, this
// test — not a client in the wild — surfaces the regression.

#[test]
fn chitta_to_rmcp_preserves_code_and_contract_fields() {
    use chitta::mcp::chitta_to_rmcp;
    use std::io;

    let variants: Vec<(ChittaError, i32)> = vec![
        (
            ChittaError::MissingConfig {
                name: "DATABASE_URL",
                next_action: "set it".to_string(),
            },
            codes::INVALID_PARAMS,
        ),
        (
            ChittaError::InvalidArgument {
                tool: "store_memory",
                argument: "profile".to_string(),
                constraint: "1-128 chars".to_string(),
                received: Some(json!("")),
                next_action: "pass a profile".to_string(),
            },
            codes::INVALID_PARAMS,
        ),
        (
            ChittaError::ContentTooLong {
                tool: "store_memory",
                token_count: 9001,
            },
            codes::INVALID_PARAMS,
        ),
        (
            ChittaError::NotFound {
                tool: "get_memory",
                kind: "memory",
                next_action: "verify id".to_string(),
            },
            codes::INVALID_PARAMS,
        ),
        (
            ChittaError::Embedding {
                tool: "store_memory",
                message: "ort error".to_string(),
                next_action: "restart".to_string(),
            },
            codes::INTERNAL_ERROR,
        ),
        (
            ChittaError::Db(sqlx::Error::PoolTimedOut),
            codes::INTERNAL_ERROR,
        ),
        (
            ChittaError::Db(sqlx::Error::Io(io::Error::other("reset"))),
            codes::INTERNAL_ERROR,
        ),
        (
            ChittaError::Migrate(sqlx::migrate::MigrateError::Execute(sqlx::Error::Io(
                io::Error::other("drift"),
            ))),
            codes::INTERNAL_ERROR,
        ),
        (
            ChittaError::Internal("unexpected".to_string()),
            codes::INTERNAL_ERROR,
        ),
    ];

    for (variant, expected_code) in variants {
        // Format the variant for diagnostics before moving it into the mapper.
        let label = format!("{variant:?}");
        let mapped = chitta_to_rmcp(variant);
        let wire = serde_json::to_value(&mapped).expect("ErrorData serializes");
        let obj = wire.as_object().expect("ErrorData is a JSON object");

        let code = obj
            .get("code")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| panic!("missing `code` for {label}: {wire}"));
        assert_eq!(code as i32, expected_code, "code mismatch for {label}");

        let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!message.is_empty(), "empty `message` for {label}");

        let data = obj
            .get("data")
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("missing `data` object for {label}: {wire}"));
        for required in ["tool", "constraint", "next_action"] {
            let v = data.get(required).and_then(|v| v.as_str()).unwrap_or("");
            assert!(
                !v.is_empty(),
                "missing `data.{required}` for {label}: {wire}"
            );
        }
    }
}

// ---- UpdateArgs / UpdateOutput ----------------------------------------

#[test]
fn update_args_shape() {
    // Minimum: profile + id + at least one of content/tags.
    let v = json!({
        "profile": "p",
        "id": "00000000-0000-0000-0000-000000000001",
        "content": "new content",
    });
    let args: UpdateArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "p");
    assert_eq!(args.id, "00000000-0000-0000-0000-000000000001");
    assert_eq!(args.content.as_deref(), Some("new content"));
    assert!(args.tags.is_none());

    // Tags only, no content.
    let v2 = json!({
        "profile": "p",
        "id": "00000000-0000-0000-0000-000000000002",
        "tags": ["a", "b"],
    });
    let args2: UpdateArgs = serde_json::from_value(v2).unwrap();
    assert!(args2.content.is_none());
    assert_eq!(args2.tags.unwrap(), vec!["a".to_string(), "b".to_string()]);

    // Both content and tags.
    let v3 = json!({
        "profile": "p",
        "id": "00000000-0000-0000-0000-000000000003",
        "content": "updated",
        "tags": ["x"],
    });
    let args3: UpdateArgs = serde_json::from_value(v3).unwrap();
    assert!(args3.content.is_some());
    assert!(args3.tags.is_some());
}

#[test]
fn update_output_wire_keys() {
    let t = chrono::Utc::now();
    let out = UpdateOutput {
        id: uuid::Uuid::now_v7(),
        profile: "p".into(),
        content: "c".into(),
        event_time: t,
        record_time: t,
        tags: vec!["t".into()],
        metadata: None,
        memory_type: "decision".into(),
        external_refs: None,
        re_embedded: true,
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(
        &v,
        &[
            "id",
            "profile",
            "content",
            "event_time",
            "record_time",
            "tags",
            "memory_type",
            "re_embedded",
        ],
    );
    assert_eq!(v["re_embedded"], json!(true));
}

// ---- DeleteArgs / DeleteOutput ----------------------------------------

#[test]
fn delete_args_shape() {
    let v = json!({"profile": "p", "id": "abc-123"});
    let args: DeleteArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "p");
    assert_eq!(args.id, "abc-123");
}

#[test]
fn delete_output_wire_keys() {
    let out = DeleteOutput {
        id: uuid::Uuid::now_v7(),
        deleted: true,
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(&v, &["id", "deleted"]);
    assert_eq!(v["deleted"], json!(true));
}

// ---- ListArgs / ListOutput --------------------------------------------

#[test]
fn list_args_shape() {
    // Minimum: just profile.
    let v = json!({"profile": "p"});
    let args: ListArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "p");
    assert!(args.limit.is_none());
    assert!(args.tags.is_none());

    // Full payload.
    let v2 = json!({"profile": "p", "limit": 5, "tags": ["x"]});
    let args2: ListArgs = serde_json::from_value(v2).unwrap();
    assert_eq!(args2.limit, Some(5));
    assert_eq!(args2.tags.unwrap(), vec!["x".to_string()]);
}

#[test]
fn list_output_wire_keys() {
    let t = chrono::Utc::now();
    let item = ListItem {
        id: uuid::Uuid::now_v7(),
        snippet: "snip".into(),
        event_time: t,
        record_time: t,
        tags: vec!["t".into()],
        memory_type: "observation".into(),
        source: None,
        external_refs: None,
        confidence: None,
    };
    let out = ListOutput {
        memories: vec![item],
        total_in_profile: 1,
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(&v, &["memories", "total_in_profile"]);
    let first = &v["memories"][0];
    assert_keys(
        first,
        &[
            "id",
            "snippet",
            "event_time",
            "record_time",
            "tags",
            "memory_type",
        ],
    );
}

// ---- Episode derivations (wire shape) ---------------------------------

#[test]
fn store_args_accepts_derivations() {
    let v = json!({
        "profile": "josh",
        "content": "session summary",
        "idempotency_key": "ep-1",
        "memory_type": "episode",
        "derivations": [
            {"source_id": "019e0725-aab3-7160-905b-a150603d16d9", "derivation_type": "synthesised_from"}
        ]
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    let derivs = args.derivations.unwrap();
    assert_eq!(derivs.len(), 1);
    assert_eq!(derivs[0].source_id, "019e0725-aab3-7160-905b-a150603d16d9");
    assert_eq!(derivs[0].derivation_type, "synthesised_from");
}

#[test]
fn store_args_accepts_multiple_derivations() {
    let v = json!({
        "profile": "josh",
        "content": "session summary",
        "idempotency_key": "ep-2",
        "memory_type": "episode",
        "derivations": [
            {"source_id": "019e0725-aab3-7160-905b-a150603d16d9", "derivation_type": "synthesised_from"},
            {"source_id": "019e0725-aab3-7160-905b-a150603d16da", "derivation_type": "synthesised_from"}
        ]
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.derivations.unwrap().len(), 2);
}

#[test]
fn store_args_episode_missing_derivations_is_none() {
    let v = json!({
        "profile": "josh",
        "content": "session summary",
        "idempotency_key": "ep-3",
        "memory_type": "episode",
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    assert!(args.derivations.is_none());
}

#[test]
fn store_args_episode_empty_derivations() {
    let v = json!({
        "profile": "josh",
        "content": "session summary",
        "idempotency_key": "ep-4",
        "memory_type": "episode",
        "derivations": []
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.derivations.unwrap().len(), 0);
}

// ---- search_memories: applies_to + include_raw (wire shape) -----------

#[test]
fn search_args_accepts_applies_to() {
    let v = json!({
        "profile": "josh",
        "query": "how does Josh debug?",
        "applies_to": {
            "domains": ["rust"],
            "skills": ["review"]
        }
    });
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    let at = args.applies_to.unwrap();
    assert_eq!(at.domains.unwrap(), vec!["rust"]);
    assert_eq!(at.skills.unwrap(), vec!["review"]);
    assert!(at.projects.is_none());
    assert!(at.situations.is_none());
}

#[test]
fn search_args_applies_to_all_four_facets() {
    let v = json!({
        "profile": "josh",
        "query": "preferences",
        "applies_to": {
            "domains": ["rust"],
            "skills": ["review"],
            "projects": ["chitta"],
            "situations": ["debugging"]
        }
    });
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    let at = args.applies_to.unwrap();
    assert_eq!(at.domains.unwrap(), vec!["rust"]);
    assert_eq!(at.skills.unwrap(), vec!["review"]);
    assert_eq!(at.projects.unwrap(), vec!["chitta"]);
    assert_eq!(at.situations.unwrap(), vec!["debugging"]);
}

#[test]
fn search_args_applies_to_empty_object_ok() {
    let v = json!({"profile": "josh", "query": "q", "applies_to": {}});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    let at = args.applies_to.unwrap();
    assert!(at.domains.is_none());
    assert!(at.skills.is_none());
}

#[test]
fn search_args_accepts_include_raw() {
    let v = json!({"profile": "josh", "query": "q", "include_raw": true});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.include_raw, Some(true));
}

#[test]
fn search_args_include_raw_defaults_to_none() {
    let v = json!({"profile": "josh", "query": "q"});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert!(args.include_raw.is_none());
    assert!(args.applies_to.is_none());
}

#[test]
fn store_args_non_episode_with_derivations() {
    let v = json!({
        "profile": "josh",
        "content": "just an observation",
        "idempotency_key": "obs-1",
        "memory_type": "observation",
        "derivations": [
            {"source_id": "019e0725-aab3-7160-905b-a150603d16d9", "derivation_type": "synthesised_from"}
        ]
    });
    let args: StoreArgs = serde_json::from_value(v).unwrap();
    assert!(args.derivations.is_some());
}

#[test]
fn episode_derivation_validation_rejects_missing() {
    use chitta::validators;
    let r = validators::episode_derivations("store_memory", "episode", &None);
    assert!(r.is_err());
    let err = r.unwrap_err();
    match &err {
        ChittaError::InvalidArgument { next_action, .. } => {
            assert!(
                next_action.contains("episode memory requires at least one entry in derivations"),
                "expected prescribed error text, got: {next_action}"
            );
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn episode_derivation_validation_rejects_empty() {
    use chitta::validators;
    let r = validators::episode_derivations("store_memory", "episode", &Some(vec![]));
    assert!(r.is_err());
    let err = r.unwrap_err();
    match &err {
        ChittaError::InvalidArgument { next_action, .. } => {
            assert!(
                next_action.contains("Either supply derivations, or use memory_type=observation"),
                "expected prescribed error text, got: {next_action}"
            );
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn episode_derivation_validation_accepts_valid() {
    use chitta::validators;
    let derivs = Some(vec![DerivationInput {
        source_id: "019e0725-aab3-7160-905b-a150603d16d9".to_string(),
        derivation_type: "synthesised_from".to_string(),
    }]);
    assert!(validators::episode_derivations("store_memory", "episode", &derivs).is_ok());
}

// ---- Decision validation contract ------------------------------------

#[test]
fn decision_missing_metadata_rejected() {
    use chitta::validators;
    let r = validators::validate_decision_metadata("store_memory", "decision", &None);
    assert!(r.is_err());
    let err = r.unwrap_err();
    match &err {
        ChittaError::InvalidArgument {
            argument,
            next_action,
            ..
        } => {
            assert_eq!(argument, "metadata");
            assert!(
                next_action.contains("observation"),
                "next_action should mention demoting to observation"
            );
            assert!(
                next_action.contains("yojana"),
                "next_action should mention routing to yojana"
            );
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn decision_missing_rationale_rejected() {
    use chitta::validators;
    let meta = Some(json!({"rejected_alternatives": ["B"]}));
    let r = validators::validate_decision_metadata("store_memory", "decision", &meta);
    assert!(r.is_err());
    match r.unwrap_err() {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "metadata.rationale");
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn decision_empty_rationale_rejected() {
    use chitta::validators;
    let meta = Some(json!({"rationale": "", "rejected_alternatives": ["B"]}));
    let r = validators::validate_decision_metadata("store_memory", "decision", &meta);
    assert!(r.is_err());
    match r.unwrap_err() {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "metadata.rationale");
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn decision_empty_alternatives_rejected() {
    use chitta::validators;
    let meta = Some(json!({"rationale": "good reason", "rejected_alternatives": []}));
    let r = validators::validate_decision_metadata("store_memory", "decision", &meta);
    assert!(r.is_err());
    match r.unwrap_err() {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "metadata.rejected_alternatives");
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn decision_missing_alternatives_rejected() {
    use chitta::validators;
    let meta = Some(json!({"rationale": "good reason"}));
    let r = validators::validate_decision_metadata("store_memory", "decision", &meta);
    assert!(r.is_err());
    match r.unwrap_err() {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "metadata.rejected_alternatives");
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[test]
fn decision_happy_path_accepted() {
    use chitta::validators;
    let meta = Some(json!({
        "rationale": "chose X because of Y",
        "rejected_alternatives": ["option A was too slow"]
    }));
    assert!(validators::validate_decision_metadata("store_memory", "decision", &meta).is_ok());
}

// ---- SupersedeArgs / SupersedeOutput (wire shape) ---------------------

#[test]
fn supersede_args_shape() {
    let v = json!({
        "profile": "josh",
        "old_id": "019e0725-aab3-7160-905b-a150603d16d9",
        "new_id": "019e0725-aab3-7160-905b-a150603d16da",
        "reason": "new observation contradicts old trait"
    });
    let args: SupersedeArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "josh");
    assert_eq!(args.old_id, "019e0725-aab3-7160-905b-a150603d16d9");
    assert_eq!(args.new_id, "019e0725-aab3-7160-905b-a150603d16da");
    assert_eq!(args.reason, "new observation contradicts old trait");
}

#[test]
fn supersede_args_rejects_missing_fields() {
    let v = json!({"profile": "josh", "old_id": "abc"});
    assert!(serde_json::from_value::<SupersedeArgs>(v).is_err());
}

#[test]
fn supersede_output_wire_keys() {
    let out = SupersedeOutput {
        old_id: uuid::Uuid::now_v7(),
        new_id: uuid::Uuid::now_v7(),
        derivation_id: uuid::Uuid::now_v7(),
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(&v, &["old_id", "new_id", "derivation_id"]);
}

// ---- get_profile (tier-0) -------------------------------------------

#[test]
fn get_profile_args_shape() {
    let v = serde_json::json!({"profile": "josh"});
    let args: GetProfileArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "josh");
}

#[test]
fn get_profile_args_rejects_missing_profile() {
    let v = serde_json::json!({});
    assert!(serde_json::from_value::<GetProfileArgs>(v).is_err());
}

#[test]
fn get_profile_output_wire_keys() {
    let out = GetProfileOutput {
        profile: "josh".into(),
        entries: vec![],
        total_candidates: 0,
        truncated: false,
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_keys(&v, &["profile", "entries", "total_candidates", "truncated"]);
}

// ---- reflect_status -------------------------------------------------

#[test]
fn reflect_status_args_shape() {
    let v = serde_json::json!({"profile": "josh"});
    let args: ReflectStatusArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.profile, "josh");
}

#[test]
fn reflect_status_args_rejects_missing_profile() {
    let v = serde_json::json!({});
    assert!(serde_json::from_value::<ReflectStatusArgs>(v).is_err());
}

// ---- min_similarity wire contract -----------------------------------

#[test]
fn search_args_accepts_min_similarity() {
    let v = json!({"profile": "josh", "query": "q", "min_similarity": 0.42});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert!((args.min_similarity.unwrap() - 0.42).abs() < 1e-6);
}

#[test]
fn search_args_min_similarity_defaults_to_none() {
    let v = json!({"profile": "josh", "query": "q"});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert!(args.min_similarity.is_none());
}

// ---- exclude_superseded wire rename ---------------------------------

#[test]
fn search_args_accepts_exclude_superseded() {
    let v = json!({"profile": "josh", "query": "q", "exclude_superseded": false});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert_eq!(args.exclude_superseded, Some(false));
}

#[test]
fn search_args_exclude_superseded_defaults_to_none() {
    let v = json!({"profile": "josh", "query": "q"});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert!(args.exclude_superseded.is_none());
}

#[test]
fn search_args_rejects_old_exclude_retired_field() {
    let v = json!({"profile": "josh", "query": "q", "exclude_retired": false});
    let args: SearchArgs = serde_json::from_value(v).unwrap();
    assert!(args.exclude_superseded.is_none());
}
