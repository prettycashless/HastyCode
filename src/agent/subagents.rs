//! Subagents: independent agent loops with their own model, running in parallel.
//!
//! They cannot prompt the user, so they only get tools the gate allows without
//! confirmation, plus command execution in Full access mode. If a model fails
//! they fail over to another model on the same key and report the switch.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::permissions::{Action, Gate, Mode, Outcome};
use crate::providers::{self, Client, Message, StreamEvent, ToolSpec};
use crate::tools::{self, ToolContext};

#[derive(Debug, Clone)]
pub struct SubagentSpec {
    pub id: String,
    pub name: String,
    pub role: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub key: Option<String>,
    pub model: String,
    pub models: Vec<String>,
    pub task: String,
    pub free_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Running,
    Done,
    Failed,
    Switched,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Switched => "switched",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubagentUpdate {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub status: Status,
    pub health: String,
    pub task: String,
    pub result: String,
}

impl SubagentUpdate {
    pub fn new(spec: &SubagentSpec) -> Self {
        Self {
            id: spec.id.clone(),
            name: spec.name.clone(),
            provider: spec.provider.clone(),
            model: spec.model.clone(),
            status: Status::Queued,
            health: "ok".into(),
            task: spec.task.clone(),
            result: String::new(),
        }
    }
}

/// Run one subagent to completion, streaming status updates.
pub fn run(
    spec: SubagentSpec,
    root: PathBuf,
    mode: Mode,
    tx: Sender<SubagentUpdate>,
    interrupt: Arc<AtomicBool>,
) {
    let mut update = SubagentUpdate::new(&spec);
    update.status = Status::Running;
    let _ = tx.send(update.clone());

    let ctx = ToolContext::new(root.clone());
    let gate = Gate::new(mode, root.clone());
    let specs = subagent_tools(mode);
    let available = spec.models.clone();

    let system = format!(
        "You are {}, a focused subagent of Hasty working on one task.\n\
         Role: {}\n\
         You cannot ask questions and cannot access the user's chat. Work autonomously.\n\
         Use tools to inspect the code; never guess. Verify your claims with commands when allowed.\n\
         Reply with a concise report: what you did, evidence (commands/output), and remaining risks.\n\
         If something is unverifiable, say so plainly.",
        spec.name, spec.role
    );
    let mut messages = vec![Message::system(system), Message::user(spec.task.clone())];

    let mut model = spec.model.clone();
    let mut tried: Vec<String> = Vec::new();
    let mut switches: Vec<String> = Vec::new();
    let mut report = String::new();
    let mut failure: Option<String> = None;

    for _attempt in 0..10 {
        if interrupt.load(Ordering::SeqCst) {
            failure = Some("interrupted".into());
            break;
        }
        let client = Client::new(&spec.provider, spec.base_url.as_deref(), spec.key.clone());
        let mut text = String::new();
        let mut on_event = |event: StreamEvent| {
            if let StreamEvent::Text(chunk) = event {
                text.push_str(&chunk);
            }
        };
        match client.chat(
            &model,
            &messages,
            &specs,
            &|| interrupt.load(Ordering::SeqCst),
            &mut on_event,
        ) {
            Ok(reply) => {
                report = reply.text.clone();
                if reply.tool_calls.is_empty() {
                    if !report.trim().is_empty() {
                        messages.push(Message::assistant(report.clone()));
                    }
                    break;
                }
                // One assistant turn carries both the text and the tool calls;
                // pushing the text separately duplicated it in every request.
                messages.push(Message::assistant_tools(
                    reply.text,
                    reply.tool_calls.clone(),
                ));
                let mut stop = false;
                for call in &reply.tool_calls {
                    if interrupt.load(Ordering::SeqCst) {
                        failure = Some("interrupted".into());
                        stop = true;
                        break;
                    }
                    let result = execute(&ctx, &gate, mode, &call.name, &call.arguments);
                    messages.push(Message::tool_result(
                        call.id.clone(),
                        call.name.clone(),
                        result.clone(),
                    ));
                }
                if stop {
                    break;
                }
            }
            Err(err) => {
                if err.kind == providers::ErrorKind::Interrupted {
                    failure = Some("interrupted".into());
                    break;
                }
                let next = providers::failover_candidates(
                    &spec.provider,
                    &available,
                    &tried,
                    spec.free_only,
                );
                update.health = format!("{} failed: {}", model, err.kind_label());
                if let Some(candidate) = next.into_iter().find(|m| m != &model) {
                    tried.push(model.clone());
                    switches.push(format!("{} → {}", model, candidate));
                    update.status = Status::Switched;
                    update.model = candidate.clone();
                    update.result = format!("model switched: {}", switches.join(", "));
                    let _ = tx.send(update.clone());
                    model = candidate;
                    continue;
                }
                failure = Some(err.user_message());
                break;
            }
        }
    }

    let mut final_text = report.trim().to_string();
    if !switches.is_empty() {
        final_text.push_str(&format!("\n\n[model failover: {}]", switches.join(", ")));
    }
    if let Some(err) = failure {
        final_text.push_str(&format!("\n\n[failed: {err}]"));
        update.status = Status::Failed;
    } else if final_text.is_empty() {
        final_text = "(no report produced)".into();
    } else {
        update.status = Status::Done;
    }
    update.model = model;
    update.result = clamp(&final_text, 8000);
    update.health = if update.status == Status::Failed {
        "failed".into()
    } else if !switches.is_empty() {
        "recovered".into()
    } else {
        "ok".into()
    };
    let _ = tx.send(update);
}

fn execute(ctx: &ToolContext, gate: &Gate, mode: Mode, name: &str, args: &Value) -> String {
    if name == "spawn_subagent" {
        return "subagents cannot spawn subagents".into();
    }
    // Tools granted to subagents, with confirmation treated as refusal.
    let actions = tools::actions_for(name, args, ctx);
    for action in &actions {
        match gate.check(action) {
            Outcome::Allow => {}
            Outcome::Deny(reason) => return format!("blocked: {reason}"),
            Outcome::Confirm(reason) => {
                let permitted = match action {
                    Action::ReadFile { path } => {
                        gate.inside_project(path) && mode == Mode::FullAccess
                    }
                    Action::WriteFile { path } | Action::EditFile { path } => {
                        gate.inside_project(path) && mode == Mode::FullAccess
                    }
                    _ => false,
                };
                if !permitted {
                    return format!("not permitted for subagents: {reason}");
                }
            }
        }
    }
    if matches!(name, "web_fetch" | "open_app" | "install_package") {
        return "tool not available to subagents".into();
    }
    if name == "run_command" && mode != Mode::FullAccess {
        return "command execution requires Full access mode".into();
    }
    if name == "git" && mode != Mode::FullAccess {
        return "git requires Full access mode".into();
    }
    let flag = AtomicBool::new(false);
    let outcome = tools::execute(ctx, name, args, &flag);
    let mut text = outcome.output;
    if let Some(diff) = outcome.diff {
        text.push_str("\n--- diff ---\n");
        text.push_str(&diff);
    }
    clamp(&text, 12_000)
}

fn subagent_tools(mode: Mode) -> Vec<ToolSpec> {
    tools::specs()
        .into_iter()
        .filter(|spec| {
            let allowed = match spec.name.as_str() {
                "read_file" | "list_dir" | "glob" | "grep" | "use_skill" | "remember" => true,
                "write_file" | "edit_file" => mode == Mode::FullAccess,
                "run_command" | "git" => mode == Mode::FullAccess,
                _ => false,
            };
            allowed
        })
        .collect()
}

fn clamp(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[..max].iter().collect::<String>() + "\n… (truncated)"
}
