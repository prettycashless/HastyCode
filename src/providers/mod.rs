//! Provider adapters.
//!
//! All providers are normalized into [`Message`] / [`ToolSpec`] / [`StreamEvent`].
//! Three wire shapes are implemented: OpenAI-compatible (OpenAI, OpenRouter,
//! Groq, Mistral, Ollama, custom endpoints), Anthropic Messages, and Google
//! generateContent.
//!
//! Requests never leave the machine except to the configured provider.

use reqwest::blocking::{Client as HttpClient, Response};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Duration;

pub const PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("google", "Google"),
    ("openrouter", "OpenRouter"),
    ("groq", "Groq"),
    ("mistral", "Mistral"),
    ("ollama", "Local (Ollama)"),
    ("custom", "Custom OpenAI-compatible"),
];

pub fn display_name(id: &str) -> &'static str {
    PROVIDERS
        .iter()
        .find(|(pid, _)| *pid == id)
        .map(|(_, label)| *label)
        .unwrap_or("OpenAI-compatible")
}

pub fn kind_of(id: &str) -> Kind {
    match id {
        "anthropic" => Kind::Anthropic,
        "google" => Kind::Google,
        _ => Kind::OpenAiCompatible,
    }
}

pub fn default_base(id: &str) -> &'static str {
    match id {
        "anthropic" => "https://api.anthropic.com/v1",
        "google" => "https://generativelanguage.googleapis.com/v1beta",
        "openrouter" => "https://openrouter.ai/api/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "ollama" => "http://localhost:11434/v1",
        _ => "https://api.openai.com/v1",
    }
}

pub fn needs_key(id: &str) -> bool {
    !matches!(id, "ollama" | "custom")
}

pub fn default_model(id: &str) -> &'static str {
    match id {
        "anthropic" => "claude-sonnet-4-5",
        "google" => "gemini-2.5-pro",
        "openrouter" => "anthropic/claude-sonnet-4.5",
        "groq" => "llama-3.3-70b-versatile",
        "mistral" => "mistral-large-latest",
        "ollama" => "qwen2.5-coder",
        _ => "gpt-5",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OpenAiCompatible,
    Anthropic,
    Google,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::plain(Role::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::plain(Role::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, text)
    }

    pub fn plain(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: text.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant_tools(text: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        let mut msg = Self::plain(Role::Assistant, text);
        msg.tool_calls = calls;
        msg
    }

    pub fn tool_result(
        id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let mut msg = Self::plain(Role::Tool, content);
        msg.tool_call_id = Some(id.into());
        msg.name = Some(name.into());
        msg
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub model: String,
    /// A hint from headers/body that this key is on a free tier.
    pub free_tier: bool,
    pub rate_limit_note: Option<String>,
    /// The provider stopped because of a token limit, not because it finished.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub free: bool,
    pub context: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Auth,
    RateLimit,
    /// The account is out of credits or quota. Switching models will not help.
    Credits,
    ModelUnavailable,
    Network,
    BadRequest,
    Server,
    Interrupted,
    Other,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub message: String,
    pub retry_after: Option<u64>,
    pub free_tier_signal: bool,
}

impl ProviderError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            message: message.into(),
            retry_after: None,
            free_tier_signal: false,
        }
    }

    pub fn interrupted() -> Self {
        Self::new(ErrorKind::Interrupted, "interrupted")
    }

    pub fn network(err: reqwest::Error) -> Self {
        let message = if err.is_timeout() {
            "request timed out".to_string()
        } else if err.is_connect() {
            format!("could not connect: {err}")
        } else {
            err.to_string()
        };
        Self::new(ErrorKind::Network, message)
    }

    pub fn network_io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, message)
    }

    pub fn from_body(status: u16, body: &str, retry_after: Option<u64>) -> Self {
        let parsed = extract_message(body);
        let kind = classify(status, body);
        let free = signals_free_tier(body);
        Self {
            kind,
            status: Some(status),
            message: if parsed.is_empty() {
                format!("HTTP {status}")
            } else {
                parsed
            },
            retry_after,
            free_tier_signal: free,
        }
    }

    pub fn from_stream_message(message: &str) -> Self {
        let kind = classify(400, message);
        Self {
            kind,
            status: None,
            message: if message.is_empty() {
                "provider returned an error event".into()
            } else {
                message.to_string()
            },
            retry_after: None,
            free_tier_signal: signals_free_tier(message),
        }
    }

    /// Worth retrying the same model after a pause.
    pub fn retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::RateLimit | ErrorKind::Server | ErrorKind::Network
        )
    }

    /// Worth trying a different model on the same key.
    pub fn should_failover(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::RateLimit | ErrorKind::ModelUnavailable | ErrorKind::Server
        )
    }

    /// The user has to fix the account; retrying and switching models is waste.
    pub fn needs_user_action(&self) -> bool {
        matches!(self.kind, ErrorKind::Auth | ErrorKind::Credits)
    }

    pub fn user_message(&self) -> String {
        // Provider bodies can echo the key that was sent; never surface it.
        let message = match self.kind {
            ErrorKind::Auth => format!(
                "The provider rejected the API key. Check it in /settings. ({})",
                self.message
            ),
            ErrorKind::RateLimit => {
                let wait = self
                    .retry_after
                    .map(|s| format!(" Retry after {s}s."))
                    .unwrap_or_default();
                format!("Rate limited by the provider.{wait} {}", self.message)
            }
            ErrorKind::Credits => format!(
                "This account is out of credits or quota. Add credit or use another key. ({})",
                self.message
            ),
            ErrorKind::ModelUnavailable => {
                format!("That model is not available on this key: {}", self.message)
            }
            ErrorKind::Network => format!("Network failure: {}", self.message),
            ErrorKind::Interrupted => "Interrupted.".into(),
            ErrorKind::BadRequest => format!("The provider rejected the request: {}", self.message),
            ErrorKind::Server => format!("The provider had a server error: {}", self.message),
            ErrorKind::Other => self.message.clone(),
        };
        crate::secrets::redact(&message)
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProviderError {}

fn extract_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        // OpenAI / Anthropic / Google shapes.
        if let Some(msg) = value["error"]["message"].as_str() {
            return clamp(msg, 600);
        }
        if let Some(msg) = value["error"].as_str() {
            return clamp(msg, 600);
        }
        if let Some(msg) = value["message"].as_str() {
            return clamp(msg, 600);
        }
        if let Some(msg) = value["detail"].as_str() {
            return clamp(msg, 600);
        }
        return clamp(&value.to_string(), 600);
    }
    clamp(body.trim(), 600)
}

fn clamp(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() > max {
        text.chars().take(max).collect::<String>() + "…"
    } else {
        text.to_string()
    }
}

fn classify(status: u16, body: &str) -> ErrorKind {
    let lower = body.to_ascii_lowercase();
    let mentions_model = lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("does not exist")
            || lower.contains("does not support")
            || lower.contains("unsupported")
            || lower.contains("invalid model")
            || lower.contains("no such model")
            || lower.contains("not a valid model")
            || lower.contains("model_not_found"));
    match status {
        401 | 403 => {
            if mentions_model {
                ErrorKind::ModelUnavailable
            } else {
                ErrorKind::Auth
            }
        }
        402 => ErrorKind::Credits,
        404 => ErrorKind::ModelUnavailable,
        408 => ErrorKind::Network,
        429 => ErrorKind::RateLimit,
        400 => {
            if mentions_model {
                ErrorKind::ModelUnavailable
            } else {
                ErrorKind::BadRequest
            }
        }
        500..=599 => ErrorKind::Server,
        _ => {
            if mentions_model {
                ErrorKind::ModelUnavailable
            } else {
                ErrorKind::Other
            }
        }
    }
}

fn signals_free_tier(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // `insufficient_quota` means the account is out of credit, not that the key
    // is on a free tier; treating it as the latter permanently mislabels the
    // provider and restricts its model list.
    lower.contains("free tier")
        || lower.contains("free-tier")
        || lower.contains("free_tier")
        || lower.contains("free plan")
}

#[derive(Default, Clone)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct Acc {
    text: String,
    reasoning: String,
    calls: BTreeMap<usize, PendingCall>,
    usage: Usage,
    free_tier: bool,
    rate_note: Option<String>,
    truncated: bool,
}

impl Acc {
    fn into_reply(self, model: &str) -> Reply {
        let mut tool_calls = Vec::new();
        for (index, mut call) in self.calls {
            if call.name.trim().is_empty() {
                continue;
            }
            if call.id.trim().is_empty() {
                call.id = format!("call_{index}_{}", short_id());
            }
            let arguments = if call.args.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str::<Value>(&call.args) {
                    Ok(value @ Value::Object(_)) => value,
                    Ok(other) => json!({ "value": other }),
                    Err(_) => json!({ "__raw": call.args }),
                }
            };
            tool_calls.push(ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            });
        }
        Reply {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            usage: self.usage,
            model: model.to_string(),
            free_tier: self.free_tier,
            rate_limit_note: self.rate_note,
            truncated: self.truncated,
        }
    }
}

fn short_id() -> String {
    uuid::Uuid::new_v4().to_string().chars().take(8).collect()
}

pub struct Client {
    pub provider_id: String,
    pub kind: Kind,
    pub base: String,
    key: Option<String>,
    http: HttpClient,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("provider_id", &self.provider_id)
            .field("base", &self.base)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Client {
    pub fn new(provider_id: &str, base_override: Option<&str>, key: Option<String>) -> Self {
        let base = base_override
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| default_base(provider_id).to_string());
        let http = HttpClient::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .user_agent(format!("hasty/{}", crate::VERSION))
            .build()
            .unwrap_or_else(|_| HttpClient::new());
        Self {
            provider_id: provider_id.to_string(),
            kind: kind_of(provider_id),
            base,
            key: key.filter(|k| !k.trim().is_empty()),
            http,
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    fn headers(&self) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match self.kind {
            Kind::Anthropic => {
                let key = self.key.as_deref().ok_or_else(|| {
                    ProviderError::new(ErrorKind::Auth, "no API key configured for Anthropic")
                })?;
                headers.insert(
                    "x-api-key",
                    HeaderValue::from_str(key).map_err(|_| {
                        ProviderError::new(ErrorKind::Auth, "invalid API key format")
                    })?,
                );
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
            Kind::OpenAiCompatible => {
                if let Some(key) = self.key.as_deref() {
                    headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                            ProviderError::new(ErrorKind::Auth, "invalid API key format")
                        })?,
                    );
                } else if needs_key(&self.provider_id) {
                    return Err(ProviderError::new(
                        ErrorKind::Auth,
                        format!(
                            "no API key configured for {}",
                            display_name(&self.provider_id)
                        ),
                    ));
                }
                if self.provider_id == "openrouter" {
                    headers.insert("X-Title", HeaderValue::from_static("Hasty"));
                }
            }
            Kind::Google => {
                if self.key.is_none() {
                    return Err(ProviderError::new(
                        ErrorKind::Auth,
                        "no API key configured for Google",
                    ));
                }
            }
        }
        Ok(headers)
    }

    // ---------------------------------------------------------------- models

    pub fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let headers = self.headers()?;
        let (url, extra) = match self.kind {
            Kind::OpenAiCompatible => (format!("{}/models", self.base), HeaderMap::new()),
            Kind::Anthropic => (format!("{}/models?limit=100", self.base), HeaderMap::new()),
            Kind::Google => (
                format!("{}/models?pageSize=200", self.base),
                HeaderMap::new(),
            ),
        };
        let mut all = headers.clone();
        for (k, v) in extra.iter() {
            all.insert(k, v.clone());
        }
        if self.kind == Kind::Google {
            if let Some(key) = self.key.as_deref() {
                if let Ok(value) = HeaderValue::from_str(key) {
                    all.insert("x-goog-api-key", value);
                }
            }
        }
        let response =
            self.send_retry(&|| Ok(self.http.get(&url).headers(all.clone())), &|| false)?;
        let body: Value = response.json().map_err(|e| {
            ProviderError::new(ErrorKind::Other, format!("unreadable model list: {e}"))
        })?;

        let mut models = Vec::new();
        match self.kind {
            Kind::Google => {
                for item in body["models"].as_array().cloned().unwrap_or_default() {
                    let methods = item["supportedGenerationMethods"]
                        .as_array()
                        .map(|m| m.iter().any(|v| v.as_str() == Some("generateContent")))
                        .unwrap_or(false);
                    if !methods {
                        continue;
                    }
                    let raw = item["name"].as_str().unwrap_or_default();
                    let id = raw.strip_prefix("models/").unwrap_or(raw).to_string();
                    if id.is_empty() {
                        continue;
                    }
                    models.push(ModelInfo {
                        label: item["displayName"].as_str().unwrap_or(&id).to_string(),
                        free: false,
                        context: item["inputTokenLimit"].as_u64(),
                        id,
                    });
                }
            }
            _ => {
                for item in body["data"].as_array().cloned().unwrap_or_default() {
                    let id = item["id"]
                        .as_str()
                        .or_else(|| item["name"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() || !looks_like_chat_model(&id) {
                        continue;
                    }
                    let free = item["pricing"]["prompt"]
                        .as_str()
                        .map(|p| p == "0" || p == "0.0")
                        .unwrap_or(false)
                        || self.provider_id == "ollama"
                        || id.ends_with(":free");
                    let context = item["context_length"]
                        .as_u64()
                        .or_else(|| item["top_provider"]["context_length"].as_u64());
                    let label = item["name"].as_str().unwrap_or(&id).to_string();
                    models.push(ModelInfo {
                        id,
                        label,
                        free,
                        context,
                    });
                }
            }
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models.dedup_by(|a, b| a.id == b.id);
        if models.is_empty() {
            return Err(ProviderError::new(
                ErrorKind::Other,
                "the provider returned no models",
            ));
        }
        Ok(models)
    }

    /// A cheap call proving the key works. Falls back to a tiny completion for
    /// OpenAI-compatible endpoints that do not expose `/models`.
    pub fn validate(&self) -> Result<String, ProviderError> {
        match self.list_models() {
            Ok(models) => Ok(format!("{} models available", models.len())),
            Err(err) if err.kind == ErrorKind::Auth => Err(err),
            Err(err) => {
                // A custom endpoint may not implement `/models`, but guessing a
                // model name for it is worse than reporting the real error.
                if self.kind != Kind::OpenAiCompatible || self.provider_id == "custom" {
                    return Err(err);
                }
                let messages = vec![Message::user("ping")];
                let mut on_event = |_e: StreamEvent| {};
                let reply = self.chat(
                    default_model(&self.provider_id),
                    &messages,
                    &[],
                    &|| false,
                    &mut on_event,
                )?;
                Ok(format!(
                    "endpoint reachable ({} chars)",
                    reply.text.chars().count()
                ))
            }
        }
    }

    // ------------------------------------------------------------------ chat

    pub fn chat(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        interrupt: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> Result<Reply, ProviderError> {
        // Track whether anything was rendered: retrying after a stream failed
        // mid-reply would print the partial answer a second time.
        let rendered = Cell::new(false);
        let mut tracked = |event: StreamEvent| {
            rendered.set(true);
            on_event(event);
        };
        let result = match self.kind {
            Kind::OpenAiCompatible => {
                self.chat_openai(model, messages, tools, interrupt, &mut tracked, true)
            }
            Kind::Anthropic => {
                self.chat_anthropic(model, messages, tools, interrupt, &mut tracked, true)
            }
            Kind::Google => self.chat_google(model, messages, tools, interrupt, &mut tracked, true),
        };

        match result {
            // Some compatible endpoints reject `stream`. Retry once without it
            // if the failure happened before anything was rendered.
            Err(err)
                if !rendered.get()
                    && !matches!(
                        err.kind,
                        ErrorKind::Auth | ErrorKind::Interrupted | ErrorKind::RateLimit
                    ) =>
            {
                let fallback = match self.kind {
                    Kind::OpenAiCompatible => {
                        self.chat_openai(model, messages, tools, interrupt, &mut tracked, false)
                    }
                    Kind::Anthropic => {
                        self.chat_anthropic(model, messages, tools, interrupt, &mut tracked, false)
                    }
                    Kind::Google => {
                        self.chat_google(model, messages, tools, interrupt, &mut tracked, false)
                    }
                };
                fallback.map_err(|second| {
                    if second.kind == ErrorKind::Other {
                        err
                    } else {
                        second
                    }
                })
            }
            other => other,
        }
    }

    fn chat_openai(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        interrupt: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent),
        stream: bool,
    ) -> Result<Reply, ProviderError> {
        let body = self.openai_body(model, messages, tools, stream);
        let url = format!("{}/chat/completions", self.base);
        let headers = self.headers()?;
        let response = self.send_retry(
            &|| Ok(self.http.post(&url).headers(headers.clone()).json(&body)),
            interrupt,
        )?;
        let mut acc = Acc {
            free_tier: free_tier_from_headers(response.headers()),
            rate_note: rate_note_from_headers(response.headers()),
            ..Default::default()
        };

        if !stream {
            let value: Value = response.json().map_err(|e| {
                ProviderError::new(ErrorKind::Other, format!("unreadable reply: {e}"))
            })?;
            self.absorb_openai_message(&value, &mut acc, on_event)?;
            if value["choices"]
                .get(0)
                .and_then(|c| c["finish_reason"].as_str())
                == Some("length")
            {
                acc.truncated = true;
            }
            return Ok(acc.into_reply(model));
        }

        let mut parse_err: Option<ProviderError> = None;
        let read = read_sse(response, interrupt, &mut |data| {
            let value: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => return Ok(true),
            };
            if value.get("error").is_some() {
                parse_err = Some(ProviderError::from_stream_message(&extract_message(data)));
                return Ok(false);
            }
            if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
                acc.usage = parse_usage(usage);
            }
            if let Some(choice) = value["choices"].get(0) {
                if choice["finish_reason"].as_str() == Some("length") {
                    acc.truncated = true;
                }
                let delta = &choice["delta"];
                if let Some(text) = delta["content"].as_str() {
                    if !text.is_empty() {
                        acc.text.push_str(text);
                        on_event(StreamEvent::Text(text.to_string()));
                    }
                }
                for key in ["reasoning", "reasoning_content"] {
                    if let Some(text) = delta[key].as_str() {
                        if !text.is_empty() {
                            acc.reasoning.push_str(text);
                            on_event(StreamEvent::Reasoning(text.to_string()));
                        }
                    }
                }
                if let Some(calls) = delta["tool_calls"].as_array() {
                    for call in calls {
                        let index = call["index"].as_u64().unwrap_or(0) as usize;
                        let entry = acc.calls.entry(index).or_default();
                        if let Some(id) = call["id"].as_str() {
                            if !id.is_empty() {
                                entry.id = id.to_string();
                            }
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function["name"].as_str() {
                                if entry.name.is_empty() && !name.is_empty() {
                                    entry.name = name.to_string();
                                }
                            }
                            if let Some(args) = function["arguments"].as_str() {
                                entry.args.push_str(args);
                            }
                        }
                    }
                }
            }
            Ok(true)
        });
        if let Some(err) = parse_err {
            return Err(err);
        }
        read?;
        Ok(acc.into_reply(model))
    }

    fn absorb_openai_message(
        &self,
        value: &Value,
        acc: &mut Acc,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), ProviderError> {
        if let Some(err) = value.get("error") {
            return Err(ProviderError::from_stream_message(&err.to_string()));
        }
        if let Some(usage) = value.get("usage") {
            acc.usage = parse_usage(usage);
        }
        let message = &value["choices"][0]["message"];
        if let Some(text) = message["content"].as_str() {
            acc.text.push_str(text);
            if !text.is_empty() {
                on_event(StreamEvent::Text(text.to_string()));
            }
        }
        if let Some(text) = message["reasoning"]
            .as_str()
            .or_else(|| message["reasoning_content"].as_str())
        {
            acc.reasoning.push_str(text);
            on_event(StreamEvent::Reasoning(text.to_string()));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for (index, call) in calls.iter().enumerate() {
                let entry = PendingCall {
                    id: call["id"].as_str().unwrap_or_default().to_string(),
                    name: call["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    args: match &call["function"]["arguments"] {
                        Value::String(s) => s.clone(),
                        other if other.is_null() => String::new(),
                        other => other.to_string(),
                    },
                };
                acc.calls.insert(index, entry);
            }
        }
        Ok(())
    }

    fn chat_anthropic(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        interrupt: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent),
        stream: bool,
    ) -> Result<Reply, ProviderError> {
        let body = self.anthropic_body(model, messages, tools, stream);
        let url = format!("{}/messages", self.base);
        let headers = self.headers()?;
        let response = self.send_retry(
            &|| Ok(self.http.post(&url).headers(headers.clone()).json(&body)),
            interrupt,
        )?;
        let mut acc = Acc {
            free_tier: free_tier_from_headers(response.headers()),
            rate_note: rate_note_from_headers(response.headers()),
            ..Default::default()
        };

        if !stream {
            let value: Value = response.json().map_err(|e| {
                ProviderError::new(ErrorKind::Other, format!("unreadable reply: {e}"))
            })?;
            if let Some(usage) = value.get("usage") {
                acc.usage = parse_usage(usage);
            }
            let mut call_index = 0usize;
            for block in value["content"].as_array().cloned().unwrap_or_default() {
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or_default();
                        acc.text.push_str(text);
                        if !text.is_empty() {
                            on_event(StreamEvent::Text(text.to_string()));
                        }
                    }
                    Some("thinking") => {
                        if let Some(text) = block["thinking"].as_str() {
                            acc.reasoning.push_str(text);
                            on_event(StreamEvent::Reasoning(text.to_string()));
                        }
                    }
                    Some("tool_use") => {
                        let entry = PendingCall {
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                            args: block["input"].to_string(),
                        };
                        acc.calls.insert(call_index, entry);
                        call_index += 1;
                    }
                    _ => {}
                }
            }
            if value["stop_reason"].as_str() == Some("max_tokens") {
                acc.truncated = true;
            }
            return Ok(acc.into_reply(model));
        }

        let mut parse_err: Option<ProviderError> = None;
        let read = read_sse(response, interrupt, &mut |data| {
            let value: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => return Ok(true),
            };
            match value["type"].as_str().unwrap_or_default() {
                "message_start" => {
                    if let Some(usage) = value["message"].get("usage") {
                        acc.usage = parse_usage(usage);
                    }
                }
                "content_block_start" => {
                    let index = value["index"].as_u64().unwrap_or(0) as usize;
                    let block = &value["content_block"];
                    if block["type"].as_str() == Some("tool_use") {
                        let entry = PendingCall {
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                            args: String::new(),
                        };
                        acc.calls.insert(index, entry);
                    }
                }
                "content_block_delta" => {
                    let index = value["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &value["delta"];
                    match delta["type"].as_str().unwrap_or_default() {
                        "text_delta" => {
                            if let Some(text) = delta["text"].as_str() {
                                acc.text.push_str(text);
                                on_event(StreamEvent::Text(text.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta["thinking"].as_str() {
                                acc.reasoning.push_str(text);
                                on_event(StreamEvent::Reasoning(text.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(part) = delta["partial_json"].as_str() {
                                acc.calls.entry(index).or_default().args.push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if value["delta"]["stop_reason"].as_str() == Some("max_tokens") {
                        acc.truncated = true;
                    }
                    if let Some(usage) = value.get("usage") {
                        let out = usage["output_tokens"].as_u64().unwrap_or(0);
                        if out > 0 {
                            acc.usage.output_tokens = out;
                        }
                    }
                }
                "error" => {
                    parse_err = Some(ProviderError::from_stream_message(&extract_message(data)));
                    return Ok(false);
                }
                _ => {}
            }
            Ok(true)
        });
        if let Some(err) = parse_err {
            return Err(err);
        }
        read?;
        Ok(acc.into_reply(model))
    }

    fn chat_google(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        interrupt: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent),
        stream: bool,
    ) -> Result<Reply, ProviderError> {
        let key = self.key.as_deref().ok_or_else(|| {
            ProviderError::new(ErrorKind::Auth, "no API key configured for Google")
        })?;
        let body = self.google_body(messages, tools);
        let url = if stream {
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.base,
                urlencode(model)
            )
        } else {
            format!("{}/models/{}:generateContent", self.base, urlencode(model))
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // The key belongs in a header, not in a URL that proxies and logs keep.
        headers.insert(
            "x-goog-api-key",
            HeaderValue::from_str(key)
                .map_err(|_| ProviderError::new(ErrorKind::Auth, "invalid API key format"))?,
        );
        let response = self.send_retry(
            &|| Ok(self.http.post(&url).headers(headers.clone()).json(&body)),
            interrupt,
        )?;
        let mut acc = Acc {
            free_tier: free_tier_from_headers(response.headers()),
            rate_note: rate_note_from_headers(response.headers()),
            ..Default::default()
        };

        let mut absorb = |value: &Value| -> Result<(), ProviderError> {
            if let Some(usage) = value.get("usageMetadata") {
                acc.usage.input_tokens = usage["promptTokenCount"]
                    .as_u64()
                    .unwrap_or(acc.usage.input_tokens);
                acc.usage.output_tokens = usage["candidatesTokenCount"]
                    .as_u64()
                    .unwrap_or(acc.usage.output_tokens);
            }
            let parts = value["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    if part["thought"].as_bool().unwrap_or(false) {
                        acc.reasoning.push_str(text);
                        on_event(StreamEvent::Reasoning(text.to_string()));
                    } else {
                        acc.text.push_str(text);
                        on_event(StreamEvent::Text(text.to_string()));
                    }
                }
                if part.get("functionCall").is_some() {
                    let index = acc.calls.len();
                    let entry = PendingCall {
                        id: format!("g_{}", short_id()),
                        name: part["functionCall"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        args: part["functionCall"]["args"].to_string(),
                    };
                    acc.calls.insert(index, entry);
                }
            }
            Ok(())
        };

        if !stream {
            let value: Value = response.json().map_err(|e| {
                ProviderError::new(ErrorKind::Other, format!("unreadable reply: {e}"))
            })?;
            absorb(&value)?;
            return Ok(acc.into_reply(model));
        }

        let mut parse_err: Option<ProviderError> = None;
        let read = read_sse(response, interrupt, &mut |data| {
            let value: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => return Ok(true),
            };
            if let Some(err) = value.get("error") {
                parse_err = Some(ProviderError::from_stream_message(&err.to_string()));
                return Ok(false);
            }
            if let Err(err) = absorb(&value) {
                parse_err = Some(err);
                return Ok(false);
            }
            Ok(true)
        });
        if let Some(err) = parse_err {
            return Err(err);
        }
        read?;
        Ok(acc.into_reply(model))
    }

    // --------------------------------------------------------------- payloads

    fn openai_body(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        stream: bool,
    ) -> Value {
        let msgs: Vec<Value> = messages.iter().map(openai_message).collect();
        let mut body = json!({
            "model": model,
            "messages": msgs,
            "stream": stream,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            }
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
        }
        if stream && self.provider_id == "openai" {
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }

    fn anthropic_body(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        stream: bool,
    ) -> Value {
        let mut system = String::new();
        let mut turns: Vec<Value> = Vec::new();

        let push_blocks = |turns: &mut Vec<Value>, role: &str, blocks: Vec<Value>| {
            if blocks.is_empty() {
                return;
            }
            if let Some(last) = turns.last_mut() {
                if last["role"].as_str() == Some(role) {
                    if let Some(content) = last["content"].as_array_mut() {
                        content.extend(blocks);
                        return;
                    }
                }
            }
            turns.push(json!({ "role": role, "content": blocks }));
        };

        for message in messages {
            match message.role {
                Role::System => {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&message.content);
                }
                Role::User => {
                    push_blocks(
                        &mut turns,
                        "user",
                        vec![json!({"type":"text","text": message.content})],
                    );
                }
                Role::Assistant => {
                    let mut blocks = Vec::new();
                    if !message.content.trim().is_empty() {
                        blocks.push(json!({"type":"text","text": message.content}));
                    }
                    for call in &message.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": call.arguments,
                        }));
                    }
                    push_blocks(&mut turns, "assistant", blocks);
                }
                Role::Tool => {
                    push_blocks(
                        &mut turns,
                        "user",
                        vec![json!({
                            "type": "tool_result",
                            "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                            "content": message.content,
                        })],
                    );
                }
            }
        }

        let mut body = json!({
            "model": model,
            "max_tokens": 8192,
            "messages": turns,
            "stream": stream,
        });
        if !system.is_empty() {
            // Cache the system prompt: it is the largest stable prefix and
            // re-sending it every step is the bulk of long-session cost.
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }]);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.parameters,
                        })
                    })
                    .collect(),
            );
        }
        body
    }

    fn google_body(&self, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let mut system = String::new();
        let mut contents: Vec<Value> = Vec::new();
        for message in messages {
            match message.role {
                Role::System => {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&message.content);
                }
                Role::User => contents.push(json!({
                    "role": "user",
                    "parts": [{"text": message.content}],
                })),
                Role::Assistant => {
                    let mut parts = Vec::new();
                    if !message.content.trim().is_empty() {
                        parts.push(json!({"text": message.content}));
                    }
                    for call in &message.tool_calls {
                        parts.push(json!({
                            "functionCall": { "name": call.name, "args": call.arguments }
                        }));
                    }
                    if !parts.is_empty() {
                        contents.push(json!({ "role": "model", "parts": parts }));
                    }
                }
                Role::Tool => {
                    let name = message.name.clone().unwrap_or_else(|| "tool".into());
                    let response = json!({ "result": message.content });
                    contents.push(json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": { "name": name, "response": response }
                        }],
                    }));
                }
            }
        }
        let mut body = json!({ "contents": contents });
        if !system.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }
        if !tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": tools.iter().map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })).collect::<Vec<_>>()
            }]);
        }
        body
    }

    // ------------------------------------------------------------------ http

    fn send_retry(
        &self,
        build: &dyn Fn() -> Result<reqwest::blocking::RequestBuilder, ProviderError>,
        interrupt: &dyn Fn() -> bool,
    ) -> Result<Response, ProviderError> {
        let mut attempt = 0u32;
        loop {
            if interrupt() {
                return Err(ProviderError::interrupted());
            }
            let request = build()?;
            match request.send() {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retry_after = parse_retry_after(response.headers());
                    let body = response.text().unwrap_or_default();
                    let err = ProviderError::from_body(status, &body, retry_after);
                    if err.retryable() && attempt < 2 {
                        attempt += 1;
                        let wait = err.retry_after.unwrap_or(1 << attempt).min(20);
                        if !sleep_interruptible(wait, interrupt) {
                            return Err(ProviderError::interrupted());
                        }
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => {
                    let err = ProviderError::network(err);
                    if attempt < 2 {
                        attempt += 1;
                        if !sleep_interruptible(1 << attempt, interrupt) {
                            return Err(ProviderError::interrupted());
                        }
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }
}

fn openai_message(message: &Message) -> Value {
    match message.role {
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
            "content": message.content,
        }),
        Role::Assistant => {
            if message.tool_calls.is_empty() {
                json!({ "role": "assistant", "content": message.content })
            } else {
                let calls: Vec<Value> = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                json!({
                    "role": "assistant",
                    "content": if message.content.is_empty() { Value::Null } else { json!(message.content) },
                    "tool_calls": calls,
                })
            }
        }
        Role::System => json!({ "role": "system", "content": message.content }),
        Role::User => json!({ "role": "user", "content": message.content }),
    }
}

fn read_sse(
    response: Response,
    interrupt: &dyn Fn() -> bool,
    on_data: &mut dyn FnMut(&str) -> Result<bool, ProviderError>,
) -> Result<(), ProviderError> {
    let reader = BufReader::new(response);
    let mut lines = reader.lines();
    loop {
        if interrupt() {
            return Err(ProviderError::interrupted());
        }
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(err)) => return Err(ProviderError::network_io(err.to_string())),
            None => break,
        };
        let line = line.trim_end_matches('\r');
        if line.is_empty()
            || line.starts_with(':')
            || line.starts_with("event:")
            || line.starts_with("id:")
        {
            continue;
        }
        let data = match line.strip_prefix("data:") {
            Some(rest) => rest.trim_start(),
            None => continue,
        };
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        if !on_data(data)? {
            break;
        }
    }
    Ok(())
}

fn sleep_interruptible(seconds: u64, interrupt: &dyn Fn() -> bool) -> bool {
    let mut waited = 0u64;
    while waited < seconds * 10 {
        if interrupt() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
        waited += 1;
    }
    true
}

fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    for name in [
        "retry-after",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
    ] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if let Ok(seconds) = value.trim().parse::<u64>() {
                return Some(seconds.min(120));
            }
            if let Some(seconds) = parse_go_duration(value) {
                return Some(seconds.min(120));
            }
        }
    }
    None
}

/// OpenAI sends durations like `1s`, `6m0s`, `120ms`.
fn parse_go_duration(value: &str) -> Option<u64> {
    let mut total = 0f64;
    let mut number = String::new();
    let mut found = false;
    let mut chars = value.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        let unit = ch.to_ascii_lowercase();
        if unit == 'm' && chars.peek() == Some(&'s') {
            // Milliseconds, not minutes: "20ms" is a fifth of a second.
            chars.next();
            if let Ok(n) = number.parse::<f64>() {
                total += n / 1000.0;
                found = true;
            }
            number.clear();
            continue;
        }
        let factor = match unit {
            'h' => 3600.0,
            'm' => 60.0,
            's' => 1.0,
            _ => {
                number.clear();
                continue;
            }
        };
        if let Ok(n) = number.parse::<f64>() {
            total += n * factor;
            found = true;
        }
        number.clear();
    }
    if found {
        // Round up so a sub-second retry never becomes "retry immediately".
        Some((total.ceil() as u64).max(1))
    } else {
        None
    }
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value["input_tokens"]
            .as_u64()
            .or_else(|| value["prompt_tokens"].as_u64())
            .or_else(|| value["promptTokenCount"].as_u64())
            .unwrap_or(0),
        output_tokens: value["output_tokens"]
            .as_u64()
            .or_else(|| value["completion_tokens"].as_u64())
            .or_else(|| value["candidatesTokenCount"].as_u64())
            .unwrap_or(0),
    }
}

fn free_tier_from_headers(_headers: &HeaderMap) -> bool {
    // Rate-limit headers describe the current window, not the plan. A paid key
    // with a small window would be labelled free-tier forever (and the label is
    // persisted), so only explicit provider error bodies count.
    false
}

fn rate_note_from_headers(headers: &HeaderMap) -> Option<String> {
    let requests = headers
        .get("x-ratelimit-remaining-requests")
        .and_then(|v| v.to_str().ok());
    let tokens = headers
        .get("x-ratelimit-remaining-tokens")
        .and_then(|v| v.to_str().ok());
    match (requests, tokens) {
        (Some(r), _) => Some(format!("{r} requests left in window")),
        (None, Some(t)) => Some(format!("{t} tokens left in window")),
        _ => None,
    }
}

fn looks_like_chat_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    const EXCLUDE: &[&str] = &[
        "embedding",
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "transcribe",
        "image",
        "audio",
        "rerank",
        "realtime",
        "search-preview",
        "sora",
    ];
    !EXCLUDE.iter().any(|needle| lower.contains(needle))
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' => out.push_str("%2F"),
            b':' => out.push_str("%3A"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------------ free tier

pub fn is_free_model(provider_id: &str, id: &str) -> bool {
    match provider_id {
        "ollama" => true,
        "openrouter" => id.ends_with(":free") || id.contains(":free") || id.ends_with("-free"),
        _ => false,
    }
}

pub fn has_free_models(provider_id: &str, models: &[ModelInfo]) -> bool {
    provider_id == "ollama"
        || models
            .iter()
            .any(|m| m.free || is_free_model(provider_id, &m.id))
}

/// When a key is free-tier, only free-eligible models may be used. Providers
/// that publish no free models are left untouched (nothing to restrict).
pub fn restrict_to_free(provider_id: &str, models: &[String]) -> Vec<String> {
    let infos: Vec<ModelInfo> = models
        .iter()
        .map(|id| ModelInfo {
            id: id.clone(),
            label: id.clone(),
            free: is_free_model(provider_id, id),
            context: None,
        })
        .collect();
    if !has_free_models(provider_id, &infos) {
        return models.to_vec();
    }
    models
        .iter()
        .filter(|id| is_free_model(provider_id, id))
        .cloned()
        .collect()
}

/// Models to try next when one fails. Used by subagent failover.
pub fn failover_candidates(
    provider_id: &str,
    available: &[String],
    tried: &[String],
    free_only: bool,
) -> Vec<String> {
    let pool: Vec<String> = if free_only {
        restrict_to_free(provider_id, available)
    } else {
        available.to_vec()
    };
    pool.into_iter()
        .filter(|model| !tried.iter().any(|t| t == model))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_errors() {
        assert_eq!(classify(401, "invalid api key"), ErrorKind::Auth);
        assert_eq!(
            classify(404, "model not found"),
            ErrorKind::ModelUnavailable
        );
        assert_eq!(
            classify(400, "The model `gpt-9` does not exist"),
            ErrorKind::ModelUnavailable
        );
        assert_eq!(classify(429, "rate limit"), ErrorKind::RateLimit);
        assert_eq!(classify(503, "oops"), ErrorKind::Server);
        assert_eq!(classify(400, "bad tool schema"), ErrorKind::BadRequest);
    }

    #[test]
    fn knows_groq_and_mistral() {
        assert_eq!(default_base("groq"), "https://api.groq.com/openai/v1");
        assert_eq!(default_base("mistral"), "https://api.mistral.ai/v1");
        assert_eq!(kind_of("groq"), Kind::OpenAiCompatible);
        assert_eq!(kind_of("mistral"), Kind::OpenAiCompatible);
        assert!(needs_key("groq"));
        assert!(needs_key("mistral"));
        assert!(PROVIDERS.iter().any(|(id, _)| *id == "groq"));
        assert!(PROVIDERS.iter().any(|(id, _)| *id == "mistral"));
    }

    #[test]
    fn failover_skips_tried_models() {
        let available = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let tried = vec!["a".to_string()];
        assert_eq!(
            failover_candidates("openai", &available, &tried, false),
            vec!["b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn free_tier_restricts_openrouter() {
        let models = vec![
            "meta-llama/llama-3:free".to_string(),
            "openai/gpt-5".to_string(),
        ];
        assert_eq!(
            restrict_to_free("openrouter", &models),
            vec!["meta-llama/llama-3:free".to_string()]
        );
    }

    #[test]
    fn free_tier_leaves_unknown_providers_alone() {
        let models = vec!["gpt-5".to_string()];
        assert_eq!(restrict_to_free("openai", &models), models);
    }

    #[test]
    fn parses_go_duration() {
        assert_eq!(parse_go_duration("1s"), Some(1));
        assert_eq!(parse_go_duration("6m0s"), Some(360));
        assert_eq!(parse_go_duration("20ms"), Some(1));
    }

    #[test]
    fn openai_payload_carries_tool_history() {
        let client = Client::new("openai", None, Some("sk-test".into()));
        let call = ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: json!({"path":"a.txt"}),
        };
        let messages = vec![
            Message::system("sys"),
            Message::assistant_tools("", vec![call]),
            Message::tool_result("call_1", "read_file", "ok"),
        ];
        let body = client.openai_body("gpt-5", &messages, &[], true);
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
    }

    #[test]
    fn anthropic_payload_uses_tool_blocks() {
        let client = Client::new("anthropic", None, Some("sk-test".into()));
        let call = ToolCall {
            id: "toolu_1".into(),
            name: "grep".into(),
            arguments: json!({"pattern":"x"}),
        };
        let messages = vec![
            Message::system("sys"),
            Message::assistant_tools("thinking", vec![call]),
            Message::tool_result("toolu_1", "grep", "found"),
        ];
        let body = client.anthropic_body("claude", &messages, &[], true);
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(
            body["system"][0]["cache_control"]["type"], "ephemeral",
            "the system prompt should be cached"
        );
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
    }
}
