//! The agent loop.
//!
//! A worker thread owns the conversation, the provider client and the tools.
//! The UI talks to it with [`UserAction`] and receives [`UiEvent`]s. Every tool
//! call passes through the single permission gate before it runs.

pub mod subagents;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::{MessageRow, Store, SubagentRow};
use crate::permissions::{Action, Gate, Outcome};
use crate::personality;
use crate::providers::{self, Client, Message, StreamEvent, ToolCall};
use crate::secrets::Secrets;
use crate::skills;
use crate::tools::{self, ToolContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Thinking,
    Working,
}

#[derive(Debug, Clone)]
pub struct SubagentStatus {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub health: String,
    pub task: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Ready {
        session_id: String,
    },
    Phase(Phase),
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingEnd {
        secs: f32,
    },
    TextDelta(String),
    ToolStart {
        id: u64,
        name: String,
        summary: String,
    },
    ToolEnd {
        id: u64,
        name: String,
        ok: bool,
        output: String,
        diff: Option<String>,
        ms: u128,
    },
    Notice(String),
    Error(String),
    Permission {
        id: u64,
        prompt: String,
        detail: String,
    },
    Subagents(Vec<SubagentStatus>),
    Usage {
        input: u64,
        output: u64,
        model: String,
    },
    ContextReset {
        summary: String,
    },
    /// A model attempt failed after streaming; the visible partial reply should
    /// be discarded before the retry streams.
    AttemptReset,
    Done {
        summary: Option<String>,
    },
    SessionTitle(String),
}

#[derive(Debug)]
pub enum UserAction {
    Prompt(String),
    Interrupt,
    Permission {
        id: u64,
        allow: bool,
    },
    Config(Box<Config>),
    ClearHistory,
    LoadHistory(Box<Vec<Message>>),
    /// Resume switched to a session that ran in another directory.
    SetRoot(PathBuf),
    Compact,
    Shutdown,
}

pub struct AgentHandle {
    pub events: Receiver<UiEvent>,
    pub actions: Sender<UserAction>,
    pub worker: Option<std::thread::JoinHandle<()>>,
}

impl AgentHandle {
    pub fn send(&self, action: UserAction) {
        let _ = self.actions.send(action);
    }

    pub fn stop(&mut self) {
        let _ = self.actions.send(UserAction::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

pub struct AgentInit {
    pub root: PathBuf,
    pub config: Config,
    pub secrets: Secrets,
    pub store: Store,
    pub session_id: String,
    pub history: Vec<Message>,
}

pub fn spawn(init: AgentInit) -> AgentHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (action_tx, action_rx) = mpsc::channel();
    let worker_events = event_tx.clone();
    let handle = std::thread::spawn(move || {
        let mut worker = Worker {
            root: init.root,
            cfg: init.config,
            secrets: init.secrets,
            store: init.store,
            session_id: init.session_id.clone(),
            history: init.history,
            events: worker_events,
            actions: action_rx,
            interrupt: Arc::new(AtomicBool::new(false)),
            pending: VecDeque::new(),
            next_id: 1,
            subagents: Vec::new(),
            titled: false,
            session_model: None,
            recent_calls: std::collections::HashMap::new(),
            subagent_started: std::collections::HashMap::new(),
        };
        let _ = worker.events.send(UiEvent::Ready {
            session_id: init.session_id,
        });
        worker.run();
    });
    AgentHandle {
        events: event_rx,
        actions: action_tx,
        worker: Some(handle),
    }
}

struct Worker {
    root: PathBuf,
    cfg: Config,
    secrets: Secrets,
    store: Store,
    session_id: String,
    history: Vec<Message>,
    events: Sender<UiEvent>,
    actions: Receiver<UserAction>,
    interrupt: Arc<AtomicBool>,
    pending: VecDeque<UserAction>,
    next_id: u64,
    subagents: Vec<SubagentStatus>,
    titled: bool,
    /// The model chosen after a failover this session, so the next turn does
    /// not walk back into the model that just failed.
    session_model: Option<String>,
    /// Exact tool calls seen this turn, for the repeat guard.
    recent_calls: std::collections::HashMap<String, u32>,
    /// Subagent id -> (start instant, start seconds) for honest durations.
    subagent_started: std::collections::HashMap<String, (Instant, u64)>,
}

impl Worker {
    fn emit(&self, event: UiEvent) {
        let _ = self.events.send(event);
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn run(&mut self) {
        loop {
            let action = match self.pending.pop_front() {
                Some(action) => action,
                None => match self.actions.recv() {
                    Ok(action) => action,
                    Err(_) => break,
                },
            };
            match action {
                UserAction::Prompt(text) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    self.turn(text);
                }
                UserAction::Interrupt => {
                    self.interrupt.store(true, Ordering::SeqCst);
                }
                UserAction::Permission { .. } => {}
                UserAction::Config(cfg) => {
                    self.cfg = *cfg;
                    self.session_model = None;
                }
                UserAction::ClearHistory => {
                    self.session_model = None;
                    self.history.clear();
                    // Clearing is durable: the stored rows go too, so resuming
                    // this session cannot restore the context just cleared.
                    let _ = self.store.clear_messages(&self.session_id);
                    self.emit(UiEvent::ContextReset {
                        summary: "Context cleared.".into(),
                    });
                }
                UserAction::SetRoot(path) => {
                    if path.is_dir() {
                        self.root = path;
                    }
                }
                UserAction::LoadHistory(messages) => {
                    self.session_model = None;
                    self.history = *messages;
                    self.emit(UiEvent::ContextReset {
                        summary: "Session resumed.".into(),
                    });
                }
                UserAction::Compact => self.compact(),
                UserAction::Shutdown => break,
            }
        }
    }

    // ------------------------------------------------------------------ turn

    fn turn(&mut self, prompt: String) {
        self.interrupt.store(false, Ordering::SeqCst);
        self.recent_calls.clear();
        self.history.push(Message::user(prompt.clone()));
        let _ = self
            .store
            .add_message(&self.session_id, "user", &prompt, "");

        let title: String = prompt
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        if !self.titled {
            let _ = self
                .store
                .touch_session(&self.session_id, Some(&title), None);
            self.emit(UiEvent::SessionTitle(title));
            self.titled = true;
        }

        let max = self.cfg.max_iterations.max(1);
        let mut finished = false;
        for _iteration in 0..max {
            if self.interrupt.load(Ordering::SeqCst) {
                self.interrupt.store(false, Ordering::SeqCst);
                self.emit(UiEvent::Notice(
                    "Interrupted. Send a new message to continue.".into(),
                ));
                self.emit(UiEvent::Done { summary: None });
                return;
            }
            self.emit(UiEvent::Phase(Phase::Working));

            let reply = match self.model_step() {
                Ok(reply) => reply,
                Err(reason) => {
                    if reason == "__interrupted__" {
                        self.interrupt.store(false, Ordering::SeqCst);
                        self.emit(UiEvent::Notice("Interrupted.".into()));
                    } else {
                        self.emit(UiEvent::Error(reason));
                        self.emit(UiEvent::Done { summary: None });
                    }
                    return;
                }
            };

            let has_text = !reply.text.trim().is_empty();
            let calls = reply.tool_calls.clone();
            if has_text || !calls.is_empty() {
                self.history
                    .push(Message::assistant_tools(reply.text.clone(), calls.clone()));
                let calls_json = serde_json::to_string(&calls).unwrap_or_default();
                let _ = self.store.add_message_full(
                    &self.session_id,
                    "assistant",
                    &reply.text,
                    &reply.reasoning,
                    &calls_json,
                    "",
                    "",
                );
            }

            if calls.is_empty() {
                self.emit(UiEvent::Done {
                    summary: if has_text {
                        Some(reply.text.clone())
                    } else {
                        None
                    },
                });
                return;
            }

            // Every tool_call in the assistant message must get a matching
            // tool_result, even when the turn stops early. Otherwise the next
            // request is rejected by OpenAI and Anthropic as malformed.
            let mut stop_reason: Option<&'static str> = None;
            for call in &calls {
                if let Some(reason) = stop_reason {
                    self.skip_tool(call, reason);
                    continue;
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    stop_reason = Some("interrupted");
                    self.skip_tool(call, "interrupted");
                    continue;
                }
                if call.name == "finish" {
                    let summary = call.arguments["summary"]
                        .as_str()
                        .unwrap_or("done")
                        .to_string();
                    stop_reason = Some("finished");
                    self.skip_tool(call, "finished");
                    self.emit(UiEvent::Done {
                        summary: Some(summary),
                    });
                    finished = true;
                    continue;
                }
                let result = self.run_tool(call);
                let _ = self
                    .store
                    .add_tool_result(&self.session_id, &call.id, &call.name, &result);
                self.history.push(Message::tool_result(
                    call.id.clone(),
                    call.name.clone(),
                    result,
                ));
            }
            if finished {
                return;
            }
            if stop_reason == Some("interrupted") {
                self.interrupt.store(false, Ordering::SeqCst);
                self.emit(UiEvent::Notice("Interrupted.".into()));
                self.emit(UiEvent::Done { summary: None });
                return;
            }
        }

        self.emit(UiEvent::Notice(format!(
            "Stopped after {max} steps without finishing. Tell me how to proceed."
        )));
        self.emit(UiEvent::Done { summary: None });
    }

    fn skip_tool(&mut self, call: &ToolCall, why: &str) {
        let text = format!("Not run: {why}.");
        let _ = self
            .store
            .add_tool_result(&self.session_id, &call.id, &call.name, &text);
        self.history.push(Message::tool_result(
            call.id.clone(),
            call.name.clone(),
            text,
        ));
    }

    fn model_step(&mut self) -> Result<providers::Reply, String> {
        let provider_id = self.cfg.provider.clone();
        if provider_id.is_empty() {
            return Err("No provider configured. Run /model to pick one.".into());
        }
        let provider =
            self.cfg.provider(&provider_id).cloned().ok_or_else(|| {
                format!("Provider `{provider_id}` is not configured. Run /model.")
            })?;

        let mut models: Vec<String> = if provider.models.is_empty() {
            vec![self.cfg.model.clone()]
        } else {
            provider.models.clone()
        };
        if models.iter().all(|m| m.is_empty()) {
            return Err("No model selected. Run /model.".into());
        }
        let mut current = self
            .session_model
            .clone()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| self.cfg.model.clone());
        if current.is_empty() {
            current = models[0].clone();
        }
        if !models.contains(&current) {
            models.insert(0, current.clone());
        }
        if provider.free_tier {
            let allowed = providers::restrict_to_free(&provider_id, &models);
            if !allowed.is_empty() && !allowed.contains(&current) {
                self.emit(UiEvent::Notice(format!(
                    "This key is free tier: switching to `{}`.",
                    allowed[0]
                )));
                current = allowed[0].clone();
            }
        }

        let key = provider
            .key_ref
            .as_ref()
            .and_then(|reference| self.secrets.get(reference));
        if providers::needs_key(&provider_id) && key.is_none() {
            return Err(format!(
                "No API key for {}. Run /settings to add one.",
                providers::display_name(&provider_id)
            ));
        }

        let system = personality::system_prompt(
            &self.cfg,
            &self.root,
            personality::read_project_notes(&self.root).as_deref(),
            &skills::index(&self.cfg.enabled_skills, &self.root),
        );
        let mut messages = Vec::with_capacity(self.history.len() + 1);
        messages.push(Message::system(system));
        messages.extend(self.history.iter().cloned());
        // `/compact` already writes its summary into the history; injecting the
        // stored copy again would send it twice.
        let already_compacted = self
            .history
            .first()
            .map(|message| {
                message.role == providers::Role::User
                    && message.content.starts_with("Earlier in this session:")
            })
            .unwrap_or(false);
        if !already_compacted {
            if let Some(summary) = self
                .store
                .session(&self.session_id)
                .ok()
                .flatten()
                .map(|s| s.summary)
                .filter(|s| !s.trim().is_empty())
            {
                messages.insert(
                    1,
                    Message::system(format!(
                        "Summary of earlier work in this session:\n{summary}"
                    )),
                );
            }
        }

        let tools = tools::specs();
        let mut tried: Vec<String> = Vec::new();

        loop {
            let client = Client::new(&provider_id, provider.base_url.as_deref(), key.clone());
            let events = self.events.clone();
            let interrupt = self.interrupt.clone();
            let started = Instant::now();
            let mut thinking_started = false;
            let mut on_event = move |event: StreamEvent| match event {
                StreamEvent::Text(text) => {
                    let _ = events.send(UiEvent::TextDelta(text));
                }
                StreamEvent::Reasoning(text) => {
                    if !thinking_started {
                        thinking_started = true;
                        let _ = events.send(UiEvent::ThinkingStart);
                    }
                    let _ = events.send(UiEvent::ThinkingDelta(text));
                }
            };

            let result = client.chat(
                &current,
                &messages,
                &tools,
                &|| interrupt.load(Ordering::SeqCst),
                &mut on_event,
            );

            if thinking_started {
                self.emit(UiEvent::ThinkingEnd {
                    secs: started.elapsed().as_secs_f32(),
                });
            }

            match result {
                Ok(reply) => {
                    if !tried.is_empty() {
                        // Remember the working model for later turns and save it on
                        // the session, so a resume starts from the model that worked.
                        self.session_model = Some(current.clone());
                        let _ = self
                            .store
                            .touch_session(&self.session_id, None, Some(&current));
                    }
                    if reply.truncated {
                        self.emit(UiEvent::Notice(format!(
                            "`{current}` hit its output limit, so the reply may be cut off."
                        )));
                    }
                    if reply.free_tier && !provider.free_tier {
                        self.mark_free_tier(&provider_id);
                        self.emit(UiEvent::Notice(
                            "Provider reports a free-tier key. Only free-eligible models will be used; expect tight rate limits.".into(),
                        ));
                    }
                    if self.cfg.model != current && reply.tool_calls.is_empty() {
                        // Failover changed the model; remember it for this session.
                        self.emit(UiEvent::Notice(format!(
                            "Using `{current}` for the rest of this turn."
                        )));
                    }
                    let _ = self.store.add_usage(
                        &self.session_id,
                        reply.usage.input_tokens,
                        reply.usage.output_tokens,
                    );
                    self.emit(UiEvent::Usage {
                        input: reply.usage.input_tokens,
                        output: reply.usage.output_tokens,
                        model: reply.model.clone(),
                    });
                    return Ok(reply);
                }
                Err(err) => {
                    if err.kind == providers::ErrorKind::Interrupted {
                        return Err("__interrupted__".into());
                    }
                    if err.free_tier_signal && !provider.free_tier {
                        self.mark_free_tier(&provider_id);
                    }
                    let candidates = providers::failover_candidates(
                        &provider_id,
                        &models,
                        &tried,
                        provider.free_tier,
                    )
                    .into_iter()
                    .filter(|model| model != &current)
                    .collect::<Vec<_>>();
                    if err.should_failover() {
                        if let Some(next) = candidates.first().cloned() {
                            tried.push(current.clone());
                            self.emit(UiEvent::AttemptReset);
                            self.emit(UiEvent::Notice(format!(
                                "`{current}` failed ({}). Switching to `{next}`.",
                                err.kind_label()
                            )));
                            current = next;
                            continue;
                        }
                    }
                    return Err(err.user_message());
                }
            }
        }
    }

    fn mark_free_tier(&mut self, provider_id: &str) {
        if let Some(provider) = self.cfg.provider_mut(provider_id) {
            provider.free_tier = true;
        }
        let _ = self.cfg.save();
    }

    // ------------------------------------------------------------------ tools

    fn run_tool(&mut self, call: &ToolCall) -> String {
        let id = self.id();
        let ctx = ToolContext::new(self.root.clone());
        let summary = crate::secrets::redact(&tools::summarize(&call.name, &call.arguments));
        self.emit(UiEvent::ToolStart {
            id,
            name: call.name.clone(),
            summary: summary.clone(),
        });

        if call.arguments.get("__raw").is_some() {
            let message =
                "The tool arguments were not valid JSON, so nothing ran. Retry with valid JSON."
                    .to_string();
            self.emit(UiEvent::ToolEnd {
                id,
                name: call.name.clone(),
                ok: false,
                output: message.clone(),
                diff: None,
                ms: 0,
            });
            return message;
        }
        let signature = format!("{}:{}", call.name, call.arguments);
        let count = self.recent_calls.entry(signature).or_insert(0);
        *count += 1;
        if *count > 2 {
            let message = format!(
                "You already called `{}` with these exact arguments twice this turn. Read the earlier result, change the approach, or finish.",
                call.name
            );
            self.emit(UiEvent::ToolEnd {
                id,
                name: call.name.clone(),
                ok: false,
                output: message.clone(),
                diff: None,
                ms: 0,
            });
            return message;
        }

        if call.name == "spawn_subagent" {
            let result = self.run_subagent(call);
            self.emit(UiEvent::ToolEnd {
                id,
                name: call.name.clone(),
                ok: true,
                output: result.chars().take(2000).collect(),
                diff: None,
                ms: 0,
            });
            return result;
        }

        let actions = tools::actions_for(&call.name, &call.arguments, &ctx);
        let gate = Gate::new(self.cfg.permission_mode, self.root.clone());
        for action in &actions {
            match gate.check(action) {
                Outcome::Allow => {}
                Outcome::Deny(reason) => {
                    let message = format!("blocked by Hasty: {reason}");
                    self.emit(UiEvent::ToolEnd {
                        id,
                        name: call.name.clone(),
                        ok: false,
                        output: message.clone(),
                        diff: None,
                        ms: 0,
                    });
                    let _ = self.store.add_tool_call(
                        &self.session_id,
                        &call.name,
                        &call.arguments.to_string(),
                        &message,
                        "denied",
                        0,
                    );
                    return message;
                }
                Outcome::Confirm(reason) => {
                    let mut detail = action.label();
                    if let Some(preview) = tools::preview(&ctx, &call.name, &call.arguments) {
                        detail.push_str("\n\n");
                        detail.push_str(&preview);
                    }
                    if !self.ask_permission(action, &reason, &detail) {
                        let message = format!(
                            "The user declined: {}. Explain what you need or find another way.",
                            action.label()
                        );
                        self.emit(UiEvent::ToolEnd {
                            id,
                            name: call.name.clone(),
                            ok: false,
                            output: message.clone(),
                            diff: None,
                            ms: 0,
                        });
                        let _ = self.store.add_tool_call(
                            &self.session_id,
                            &call.name,
                            &call.arguments.to_string(),
                            &message,
                            "declined",
                            0,
                        );
                        return message;
                    }
                }
            }
        }

        let started = Instant::now();
        let outcome = tools::execute(&ctx, &call.name, &call.arguments, &self.interrupt);
        let ms = started.elapsed().as_millis();
        // Tool output can contain whatever the command printed, including a key.
        // Redact it before it is shown, stored, or sent to the provider.
        let output = crate::secrets::redact(&outcome.output);
        self.emit(UiEvent::ToolEnd {
            id,
            name: call.name.clone(),
            ok: outcome.ok,
            output: output.chars().take(6000).collect(),
            diff: outcome.diff.clone(),
            ms,
        });
        let _ = self.store.add_tool_call(
            &self.session_id,
            &call.name,
            &call.arguments.to_string(),
            &output,
            if outcome.ok { "ok" } else { "error" },
            ms as u64,
        );
        let mut text = output;
        if let Some(diff) = outcome.diff {
            text.push_str("\n--- diff ---\n");
            text.push_str(&diff);
        }
        fit_for_model(text)
    }

    fn ask_permission(&mut self, action: &Action, reason: &str, detail: &str) -> bool {
        let id = self.id();
        self.emit(UiEvent::Permission {
            id,
            prompt: reason.to_string(),
            detail: detail.to_string(),
        });
        let _ = action;
        loop {
            match self.actions.recv_timeout(Duration::from_secs(900)) {
                Ok(UserAction::Permission { id: rid, allow }) if rid == id => return allow,
                Ok(UserAction::Interrupt) => {
                    self.interrupt.store(true, Ordering::SeqCst);
                    return false;
                }
                Ok(UserAction::Shutdown) => {
                    self.pending.push_back(UserAction::Shutdown);
                    return false;
                }
                Ok(other) => self.pending.push_back(other),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.emit(UiEvent::Notice(
                        "The permission prompt timed out after 15 minutes; treated as declined."
                            .into(),
                    ));
                    return false;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn run_subagent(&mut self, call: &ToolCall) -> String {
        let name = call.arguments["name"]
            .as_str()
            .unwrap_or("subagent")
            .to_string();
        let task = call.arguments["task"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let requested = call.arguments["model"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if task.trim().is_empty() {
            return "spawn_subagent requires a `task`".into();
        }

        // Pick a configured key: explicit label, else round-robin.
        let keys = self.cfg.subagent_keys.clone();
        let chosen = keys
            .iter()
            .find(|key| key.name.eq_ignore_ascii_case(&requested) || key.model == requested)
            .or_else(|| keys.get(self.subagents.len() % keys.len().max(1)));

        let (provider_id, key_ref, model, models, role_label) = match chosen {
            Some(key) => {
                let provider = self.cfg.provider(&key.provider).cloned();
                (
                    key.provider.clone(),
                    Some(key.key_ref.clone()),
                    key.model.clone(),
                    if key.models.is_empty() {
                        provider.map(|p| p.models).unwrap_or_default()
                    } else {
                        key.models.clone()
                    },
                    if key.role.is_empty() {
                        "specialist".to_string()
                    } else {
                        key.role.clone()
                    },
                )
            }
            None => {
                let provider = match self.cfg.active_provider().cloned() {
                    Some(provider) => provider,
                    None => return "No provider configured for subagents.".into(),
                };
                (
                    self.cfg.provider.clone(),
                    provider.key_ref.clone(),
                    self.cfg.model.clone(),
                    provider.models.clone(),
                    "generalist".to_string(),
                )
            }
        };

        let key = key_ref.and_then(|reference| self.secrets.get(&reference));
        let base_url = self
            .cfg
            .provider(&provider_id)
            .and_then(|p| p.base_url.clone());
        if providers::needs_key(&provider_id) && key.is_none() {
            return format!("No API key for subagent `{name}`.");
        }

        let id = uuid::Uuid::new_v4().to_string();
        let spec = subagents::SubagentSpec {
            id: id.clone(),
            name: name.clone(),
            role: role_label,
            provider: provider_id.clone(),
            base_url,
            key,
            model: model.clone(),
            models,
            task: task.clone(),
            free_only: self
                .cfg
                .provider(&provider_id)
                .map(|p| p.free_tier)
                .unwrap_or(false),
        };

        let (tx, rx) = mpsc::channel();
        let root = self.root.clone();
        let mode = self.cfg.permission_mode;
        let interrupt = self.interrupt.clone();
        let handle = std::thread::spawn(move || {
            subagents::run(spec, root, mode, tx, interrupt);
        });

        self.subagents.push(SubagentStatus {
            id: id.clone(),
            name: name.clone(),
            provider: provider_id,
            model: model.clone(),
            status: "running".into(),
            health: "ok".into(),
            task: task.clone(),
            elapsed_ms: 0,
        });
        self.subagent_started
            .insert(id.clone(), (Instant::now(), crate::config::now_secs()));
        self.emit_subagents();

        let mut report = String::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(update) => {
                    if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == update.id) {
                        existing.status = update.status.as_str().to_string();
                        existing.health = update.health.clone();
                        existing.model = update.model.clone();
                        if let Some((started, _)) = self.subagent_started.get(&update.id) {
                            existing.elapsed_ms = started.elapsed().as_millis();
                        }
                    }
                    let _ = self.store.upsert_subagent(&SubagentRow {
                        id: update.id.clone(),
                        session_id: self.session_id.clone(),
                        name: update.name.clone(),
                        provider: update.provider.clone(),
                        model: update.model.clone(),
                        status: update.status.as_str().to_string(),
                        task: update.task.clone(),
                        result: update.result.clone(),
                        health: update.health.clone(),
                        started_at: self
                            .subagent_started
                            .get(&update.id)
                            .map(|(_, secs)| *secs)
                            .unwrap_or_else(crate::config::now_secs),
                        ended_at: if matches!(
                            update.status,
                            subagents::Status::Done | subagents::Status::Failed
                        ) {
                            Some(crate::config::now_secs())
                        } else {
                            None
                        },
                    });
                    self.emit_subagents();
                    if matches!(
                        update.status,
                        subagents::Status::Done | subagents::Status::Failed
                    ) {
                        report = update.result.clone();
                        break;
                    }
                    if !update.result.is_empty() {
                        report = update.result.clone();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if handle.is_finished() {
                        // Drain a final queued update, then stop.
                        if let Ok(update) = rx.recv_timeout(Duration::from_millis(200)) {
                            report = update.result.clone();
                        }
                        break;
                    }
                    if self.interrupt.load(Ordering::SeqCst) {
                        report = "interrupted".into();
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = handle.join();
        if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == id) {
            if existing.status == "running" {
                existing.status = "done".into();
            }
            if let Some((started, _)) = self.subagent_started.get(&id) {
                existing.elapsed_ms = started.elapsed().as_millis();
            }
        }
        self.emit_subagents();
        format!(
            "Subagent `{name}` report:\n{}",
            if report.is_empty() {
                "(no output)"
            } else {
                &report
            }
        )
    }

    fn emit_subagents(&self) {
        self.emit(UiEvent::Subagents(self.subagents.clone()));
    }

    // ------------------------------------------------------------- compaction

    fn compact(&mut self) {
        if self.history.len() < 4 {
            self.emit(UiEvent::Notice("Not enough context to compact yet.".into()));
            return;
        }
        let provider_id = self.cfg.provider.clone();
        let Some(provider) = self.cfg.provider(&provider_id).cloned() else {
            self.emit(UiEvent::Error("No provider configured.".into()));
            return;
        };
        let key = provider
            .key_ref
            .as_ref()
            .and_then(|reference| self.secrets.get(reference));
        let client = Client::new(&provider_id, provider.base_url.as_deref(), key);
        // Compact with the model that answered last, not the configured one:
        // after a failover the configured model may be the one that failed.
        let model = self
            .session_model
            .clone()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| self.cfg.model.clone());
        if model.is_empty() {
            self.emit(UiEvent::Error("No model selected. Run /model.".into()));
            return;
        }

        let mut transcript = String::new();
        for message in &self.history {
            let role = match message.role {
                providers::Role::User => "User",
                providers::Role::Assistant => "Hasty",
                providers::Role::Tool => "Tool",
                providers::Role::System => continue,
            };
            transcript.push_str(&format!("{role}: {}\n", truncate(&message.content, 3000)));
        }
        let prompt = format!(
            "Summarize this coding session for future context. Keep: user goals and constraints, \
             decisions made, files touched, commands and results, what remains. Be specific and \
             under 500 words.\n\n{transcript}"
        );
        let messages = vec![
            Message::system("You compress context without losing technical facts."),
            Message::user(prompt),
        ];
        let mut sink = |_event: StreamEvent| {};
        match client.chat(&model, &messages, &[], &|| false, &mut sink) {
            Ok(reply) if !reply.text.trim().is_empty() => {
                let summary = reply.text.trim().to_string();
                let _ = self.store.set_summary(&self.session_id, &summary);
                // Never split between an assistant tool_call and its result.
                let split = compact_split(&self.history, 2);
                let keep = self.history.split_off(split);
                self.history = vec![Message::user(format!(
                    "Earlier in this session:\n{summary}\n\nContinue from here."
                ))];
                self.history.extend(keep);
                self.emit(UiEvent::ContextReset {
                    summary: summary.clone(),
                });
            }
            Ok(_) => self.emit(UiEvent::Error("Compaction returned nothing.".into())),
            Err(err) => self.emit(UiEvent::Error(format!(
                "Compaction failed: {}",
                err.user_message()
            ))),
        }
    }
}

/// Convert persisted rows back into chat messages for a resumed session.
///
/// Tool calls and their results are rebuilt as real `assistant(tool_calls)` /
/// `tool` pairs; older databases that only stored text still work.
pub fn messages_from_rows(rows: &[MessageRow]) -> Vec<Message> {
    let mut out = Vec::new();
    for row in rows {
        match row.role.as_str() {
            "user" => out.push(Message::user(row.content.clone())),
            "assistant" => {
                let calls: Vec<ToolCall> = if row.tool_calls.trim().is_empty() {
                    Vec::new()
                } else {
                    serde_json::from_str(&row.tool_calls).unwrap_or_default()
                };
                if calls.is_empty() {
                    if !row.content.trim().is_empty() {
                        out.push(Message::assistant(row.content.clone()));
                    }
                } else {
                    out.push(Message::assistant_tools(row.content.clone(), calls));
                }
            }
            "tool" => {
                let id = if row.tool_call_id.trim().is_empty() {
                    "unknown".to_string()
                } else {
                    row.tool_call_id.clone()
                };
                out.push(Message::tool_result(
                    id,
                    row.name.clone(),
                    truncate(&row.content, 8000),
                ));
            }
            _ => {}
        }
    }
    out
}

/// Index to split history at, keeping `keep` trailing messages without
/// orphaning a tool result that belongs to the message before it.
fn compact_split(history: &[Message], keep: usize) -> usize {
    let mut index = history.len().saturating_sub(keep);
    // Start at a user turn: never orphan a tool result and never open a
    // conversation with an assistant message.
    while index > 0 && history[index].role != providers::Role::User {
        index -= 1;
    }
    index
}

/// Keep a tool result useful without letting one command eat the context.
fn fit_for_model(text: String) -> String {
    const MAX: usize = 12_000;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX {
        return text;
    }
    let head = MAX / 2;
    let tail = MAX.saturating_sub(head + 40);
    let mut out: String = chars[..head].iter().collect();
    out.push_str(&format!(
        "\n… ({} characters elided; output was too large) …\n",
        chars.len() - head - tail
    ));
    out.extend(chars[chars.len() - tail..].iter());
    out
}

fn truncate(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[..max].iter().collect::<String>() + "…"
}

impl providers::ProviderError {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            providers::ErrorKind::Auth => "auth",
            providers::ErrorKind::RateLimit => "rate limit",
            providers::ErrorKind::Credits => "out of credits",
            providers::ErrorKind::ModelUnavailable => "model unavailable",
            providers::ErrorKind::Network => "network",
            providers::ErrorKind::BadRequest => "bad request",
            providers::ErrorKind::Server => "server error",
            providers::ErrorKind::Interrupted => "interrupted",
            providers::ErrorKind::Other => "error",
        }
    }
}

/// Used by the wizard to validate a key and list models.
pub fn probe(
    provider_id: &str,
    base_url: Option<&str>,
    key: Option<String>,
) -> Result<Vec<providers::ModelInfo>, String> {
    let client = Client::new(provider_id, base_url, key);
    client.list_models().map_err(|e| e.user_message())
}

/// Shared tool-spec helper so the UI can describe capabilities.
pub fn tool_names() -> Vec<String> {
    tools::specs().into_iter().map(|s| s.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn compact_split_starts_at_a_user_turn() {
        let history = vec![
            Message::user("first"),
            Message::assistant_tools("working", vec![call("a", "read_file")]),
            Message::tool_result("a", "read_file", "contents"),
            Message::user("second"),
        ];
        let index = compact_split(&history, 2);
        assert_eq!(history[index].role, providers::Role::User);
        assert_eq!(index, 0);
    }

    #[test]
    fn compact_split_keeps_a_clean_tail() {
        let history = vec![
            Message::user("first"),
            Message::assistant("done"),
            Message::user("second"),
            Message::assistant_tools("working", vec![call("b", "glob")]),
            Message::tool_result("b", "glob", "files"),
        ];
        let index = compact_split(&history, 2);
        assert_eq!(history[index].role, providers::Role::User);
    }
}
