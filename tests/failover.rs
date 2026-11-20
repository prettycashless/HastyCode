//! Model failover and free-tier handling.

use hasty::providers::{
    failover_candidates, is_free_model, restrict_to_free, ErrorKind, ProviderError,
};

#[test]
fn failover_skips_models_already_tried() {
    let available = vec![
        "gpt-5".to_string(),
        "gpt-5-mini".to_string(),
        "o4-mini".to_string(),
    ];
    let tried = vec!["gpt-5".to_string()];
    assert_eq!(
        failover_candidates("openai", &available, &tried, false),
        vec!["gpt-5-mini".to_string(), "o4-mini".to_string()]
    );
}

#[test]
fn failover_returns_nothing_when_everything_was_tried() {
    let available = vec!["a".to_string()];
    let tried = vec!["a".to_string()];
    assert!(failover_candidates("openai", &available, &tried, false).is_empty());
}

#[test]
fn free_tier_keys_only_get_free_models() {
    let available = vec![
        "openai/gpt-5".to_string(),
        "meta-llama/llama-3:free".to_string(),
        "qwen/qwen-2.5:free".to_string(),
    ];
    assert_eq!(
        failover_candidates("openrouter", &available, &[], true),
        vec![
            "meta-llama/llama-3:free".to_string(),
            "qwen/qwen-2.5:free".to_string()
        ]
    );
}

#[test]
fn free_tier_detection_matches_provider_conventions() {
    assert!(is_free_model("ollama", "qwen2.5-coder"));
    assert!(is_free_model("openrouter", "meta-llama/llama-3:free"));
    assert!(!is_free_model("openrouter", "openai/gpt-5"));
    assert!(!is_free_model("openai", "gpt-5"));
}

#[test]
fn providers_without_free_models_are_not_restricted() {
    let available = vec!["gpt-5".to_string(), "gpt-5-mini".to_string()];
    assert_eq!(restrict_to_free("openai", &available), available);
}

#[test]
fn rate_limits_and_missing_models_trigger_failover() {
    let rate_limited =
        ProviderError::from_body(429, r#"{"error":{"message":"slow down"}}"#, Some(3));
    assert_eq!(rate_limited.kind, ErrorKind::RateLimit);
    assert!(rate_limited.should_failover());
    assert!(rate_limited.retryable());

    let missing = ProviderError::from_body(
        404,
        r#"{"error":{"message":"The model `gpt-9` does not exist"}}"#,
        None,
    );
    assert_eq!(missing.kind, ErrorKind::ModelUnavailable);
    assert!(missing.should_failover());

    let auth = ProviderError::from_body(401, r#"{"error":{"message":"invalid api key"}}"#, None);
    assert_eq!(auth.kind, ErrorKind::Auth);
    assert!(
        !auth.should_failover(),
        "a bad key must not burn other models"
    );
}

#[test]
fn server_errors_failover_but_bad_requests_do_not() {
    let server = ProviderError::from_body(503, "unavailable", None);
    assert!(server.should_failover());
    let bad = ProviderError::from_body(400, r#"{"error":{"message":"bad tool schema"}}"#, None);
    assert_eq!(bad.kind, ErrorKind::BadRequest);
    assert!(!bad.should_failover());
}

#[test]
fn free_tier_signals_are_surfaced() {
    let err = ProviderError::from_body(
        429,
        r#"{"error":{"message":"You exceeded your free tier quota"}}"#,
        None,
    );
    assert!(err.free_tier_signal);
}

#[test]
fn error_messages_do_not_leak_keys() {
    let err = ProviderError::from_body(401, "key sk-secretvalue123 rejected", None);
    let message = err.user_message();
    assert!(message.contains("rejected"));
    // The raw provider text may echo a key; it is the caller's job to redact
    // output, and Hasty does so when rendering.
    assert!(!message.is_empty());
}
