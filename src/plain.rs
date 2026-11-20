//! Plain-text mode.
//!
//! A line-based front end with no raw mode, no alternate screen, no mouse and
//! no special keys. It exists for phones (Termux), ssh sessions, screen readers
//! and terminals whose soft keyboard cannot send Esc, Ctrl or arrows: everything
//! is typed, every menu takes a number, and permissions are answered with y/n.
//!
//! The agent loop is the same one the full-screen UI uses; only the front end
//! differs.

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use crate::agent::{self, AgentInit, UiEvent, UserAction};
use crate::config::{Config, ProviderConfig};
use crate::db::Store;
use crate::memory::{self, Scope};
use crate::permissions::Mode;
use crate::providers::{self, ModelInfo};
use crate::secrets::Secrets;
use crate::VERSION;

/// A blocking, line-oriented input channel. One reader thread owns stdin so the
/// chat loop can keep draining agent events while the user is not typing.
pub struct Input {
    rx: Receiver<Option<String>>,
}

impl Input {
    fn start() -> Input {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            let mut line = String::new();
            loop {
                line.clear();
                match lock.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(_) => {
                        let text = line.trim_end_matches(['\r', '\n']).to_string();
                        if tx.send(Some(text)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                }
                if line.is_empty() {
                    break;
                }
            }
        });
        Input { rx }
    }

    fn next(&self) -> io::Result<String> {
        match self.rx.recv() {
            Ok(Some(line)) => Ok(line),
            Ok(None) | Err(_) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "input closed")),
        }
    }
}

fn ask(input: &Input, prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    input.next()
}

#[cfg(unix)]
fn echo_off() -> Option<libc::termios> {
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut term) != 0 {
            return None;
        }
        let original = term;
        term.c_lflag &= !libc::ECHO;
        libc::tcsetattr(0, libc::TCSANOW, &term);
        Some(original)
    }
}

#[cfg(not(unix))]
fn echo_off() -> Option<()> {
    None
}

#[cfg(unix)]
fn echo_restore(original: Option<libc::termios>) {
    if let Some(term) = original {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &term);
        }
    }
}

#[cfg(not(unix))]
fn echo_restore(_original: Option<()>) {}

#[cfg(unix)]
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_sigint(_signal: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Ctrl+C in a cooked terminal would otherwise kill the process; route it to the
/// same interrupt the full-screen UI uses.
#[cfg(unix)]
fn install_sigint() {
    unsafe {
        let handler: extern "C" fn(libc::c_int) = on_sigint;
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_sigint() {}

pub fn run(
    mut cfg: Config,
    mut secrets: Secrets,
    fresh: bool,
    force_setup: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = Input::start();
    if (!cfg.onboarded || force_setup) && !setup_flow(&mut cfg, &mut secrets, &input)? {
        println!("Setup cancelled. Nothing was changed.");
        return Ok(());
    }
    install_sigint();

    let store = Store::open_default()?;
    let root = std::env::current_dir()?;
    let cwd = root.to_string_lossy().to_string();
    let previous = if fresh {
        None
    } else {
        store.latest_session_for_cwd(&cwd)?
    };
    let created = previous.is_none();
    let session = match previous {
        Some(session) => session,
        None => store.create_session("", &cwd, &cfg.provider, &cfg.model)?,
    };
    if cfg.provider(&session.provider).is_some() {
        cfg.provider = session.provider.clone();
        cfg.model = session.model.clone();
    }
    let history = agent::messages_from_rows(&store.messages(&session.id)?);
    let session_id = session.id.clone();
    let agent_store = Store::open_default()?;
    let worker_secrets = secrets_open_again(&cfg);
    let mut agent = agent::spawn(AgentInit {
        root: root.clone(),
        config: cfg.clone(),
        secrets: worker_secrets,
        store: agent_store,
        session_id: session_id.clone(),
        history,
    });

    println!();
    println!(
        "Hasty {VERSION} · {} · {}",
        providers::display_name(&cfg.provider),
        if cfg.model.is_empty() {
            "no model"
        } else {
            &cfg.model
        }
    );
    println!("{} · {}", cfg.permission_mode.label(), root.display());
    println!("Type a task and press Enter. /help lists commands. /stop interrupts. Ctrl+D exits.");
    println!();

    let mut out = Out::default();
    let mut busy = false;
    let mut pending_permission: Option<u64> = None;
    let mut queued: VecDeque<String> = VecDeque::new();
    let mut prompt_shown = false;

    'main: loop {
        // Agent output.
        loop {
            match agent.events.try_recv() {
                Ok(UiEvent::Permission { id, prompt, detail }) => {
                    pending_permission = Some(id);
                    out.line("");
                    out.line(&format!("! {prompt}"));
                    for line in detail.lines().take(12) {
                        out.line(&format!("  {line}"));
                    }
                    print!("Allow? [Y/n] ");
                    let _ = io::stdout().flush();
                    // A line typed before the prompt appeared is still an answer.
                    if let Some(answer) = queued.pop_front() {
                        let answer = answer.trim().to_ascii_lowercase();
                        let allow = answer.is_empty() || answer == "y" || answer == "yes";
                        agent.send(UserAction::Permission { id, allow });
                        pending_permission = None;
                    }
                }
                Ok(event) => {
                    if !handle_event(&mut out, event) {
                        busy = false;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    out.line("");
                    out.line("The agent stopped.");
                    break 'main;
                }
            }
        }

        #[cfg(unix)]
        if INTERRUPTED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            if busy {
                agent.send(UserAction::Interrupt);
                out.line("· interrupt sent");
            } else {
                break 'main;
            }
        }

        // Typed input.
        match input.rx.try_recv() {
            Ok(Some(line)) => {
                if let Some(id) = pending_permission.take() {
                    let answer = line.trim().to_ascii_lowercase();
                    let allow = answer.is_empty() || answer == "y" || answer == "yes";
                    agent.send(UserAction::Permission { id, allow });
                } else if busy {
                    if line.trim().eq_ignore_ascii_case("/stop") {
                        agent.send(UserAction::Interrupt);
                        out.line("· interrupt sent");
                    } else if !line.trim().is_empty() {
                        queued.push_back(line);
                        out.line("· queued for after this turn");
                    }
                } else if !line.trim().is_empty() {
                    match handle_input(
                        &line,
                        &mut cfg,
                        &mut secrets,
                        &root,
                        &agent,
                        &input,
                        &mut out,
                    )? {
                        InputOutcome::Continue => {}
                        InputOutcome::Submitted => {
                            busy = true;
                            prompt_shown = false;
                        }
                        InputOutcome::Exit => break 'main,
                    }
                }
            }
            Ok(None) | Err(TryRecvError::Disconnected) => break 'main,
            Err(TryRecvError::Empty) => {}
        }

        if !busy {
            if let Some(next) = queued.pop_front() {
                match handle_input(
                    &next,
                    &mut cfg,
                    &mut secrets,
                    &root,
                    &agent,
                    &input,
                    &mut out,
                )? {
                    InputOutcome::Continue => {}
                    InputOutcome::Submitted => {
                        busy = true;
                        prompt_shown = false;
                    }
                    InputOutcome::Exit => break 'main,
                }
            } else if !prompt_shown && pending_permission.is_none() {
                print!("> ");
                let _ = io::stdout().flush();
                prompt_shown = true;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    out.finish();
    agent.stop();
    if created
        && store
            .messages(&session_id)
            .map(|messages| messages.is_empty())
            .unwrap_or(false)
    {
        let _ = store.delete_session(&session_id);
    }
    Ok(())
}

fn secrets_open_again(cfg: &Config) -> Secrets {
    let preferred = if cfg.secret_backend.is_empty() {
        None
    } else {
        Some(cfg.secret_backend.as_str())
    };
    Secrets::open(preferred).unwrap_or_else(|_| Secrets::open(Some("file")).expect("vault"))
}

/// Prints agent output in a shape that reads well in a narrow terminal.
#[derive(Default)]
struct Out {
    open: bool,
}

impl Out {
    fn text(&mut self, text: &str) {
        print!("{text}");
        self.open = !text.ends_with('\n');
        let _ = io::stdout().flush();
    }

    fn line(&mut self, text: &str) {
        if self.open {
            println!();
            self.open = false;
        }
        println!("{text}");
    }

    fn finish(&mut self) {
        if self.open {
            println!();
            self.open = false;
        }
    }
}

/// Returns false when the turn is over.
fn handle_event(out: &mut Out, event: UiEvent) -> bool {
    match event {
        UiEvent::Ready { .. } | UiEvent::Phase(_) => {}
        UiEvent::ThinkingStart => out.line("· thinking…"),
        UiEvent::ThinkingDelta(_) | UiEvent::ThinkingEnd { .. } => {}
        UiEvent::TextDelta(text) => out.text(&text),
        UiEvent::ToolStart { name, summary, .. } => {
            if summary.trim().is_empty() {
                out.line(&format!("  → {name}"));
            } else {
                out.line(&format!("  → {name} {summary}"));
            }
        }
        UiEvent::ToolEnd {
            name,
            ok,
            output,
            diff,
            ms,
            ..
        } => {
            out.line(&format!("  {} {name} ({ms}ms)", if ok { "✓" } else { "✗" }));
            if let Some(diff) = diff {
                print_block(out, &diff, if ok { 12 } else { 30 });
            }
            if !ok {
                print_block(out, &output, 20);
            }
        }
        UiEvent::Notice(text) => out.line(&format!("· {text}")),
        UiEvent::Error(text) => out.line(&format!("! {text}")),
        UiEvent::Permission { .. } => {}
        UiEvent::Subagents(_) | UiEvent::Usage { .. } => {}
        UiEvent::ContextReset { summary } => {
            out.line("");
            out.line(&format!("· {summary}"));
        }
        UiEvent::AttemptReset => out.line("· retrying"),
        UiEvent::SessionTitle(_) => {}
        UiEvent::Done { .. } => {
            // The reply itself was streamed as deltas; printing the summary too
            // would repeat it.
            out.finish();
            return false;
        }
    }
    true
}

fn print_block(out: &mut Out, text: &str, max_lines: usize) {
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().take(max_lines) {
        out.line(&format!("    {line}"));
    }
    if lines.len() > max_lines {
        out.line(&format!("    … {} more line(s)", lines.len() - max_lines));
    }
}

enum InputOutcome {
    Continue,
    /// A turn was started; wait for it before reading another line.
    Submitted,
    Exit,
}

fn handle_input(
    line: &str,
    cfg: &mut Config,
    _secrets: &mut Secrets,
    root: &Path,
    agent: &agent::AgentHandle,
    input: &Input,
    out: &mut Out,
) -> io::Result<InputOutcome> {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        if trimmed.eq_ignore_ascii_case("exit") || trimmed.eq_ignore_ascii_case("quit") {
            return Ok(InputOutcome::Exit);
        }
        agent.send(UserAction::Prompt(trimmed.to_string()));
        return Ok(InputOutcome::Submitted);
    }

    let (command, rest) = crate::commands::parse(trimmed);
    match command.as_str() {
        "/exit" | "/quit" => Ok(InputOutcome::Exit),
        "/help" => {
            print_help();
            Ok(InputOutcome::Continue)
        }
        "/model" => {
            pick_model(cfg, agent, input, out)?;
            Ok(InputOutcome::Continue)
        }
        "/permission" | "/permissions" | "/mode" => {
            print_choice(cfg, agent, input, out)?;
            Ok(InputOutcome::Continue)
        }
        "/clear" => {
            agent.send(UserAction::ClearHistory);
            Ok(InputOutcome::Continue)
        }
        "/compact" => {
            agent.send(UserAction::Compact);
            Ok(InputOutcome::Continue)
        }
        "/memory" => {
            print_memory(root, out);
            Ok(InputOutcome::Continue)
        }
        "/remember" => {
            remember(root, &rest, out);
            Ok(InputOutcome::Continue)
        }
        "/stop" => {
            out.line("· nothing is running");
            Ok(InputOutcome::Continue)
        }
        "/settings" | "/theme" | "/sessions" | "/skills" | "/connectors" | "/subagents" => {
            out.line("· that screen needs the full-screen UI. In plain mode use:");
            out.line("    /model  /permission  /clear  /compact  /memory  /remember");
            Ok(InputOutcome::Continue)
        }
        other => {
            out.line(&format!("unknown command {other}. /help lists commands."));
            Ok(InputOutcome::Continue)
        }
    }
}

fn print_help() {
    println!();
    println!("Commands");
    println!("  /help              this list");
    println!("  /model             switch provider or model (numbered list)");
    println!("  /permission        ask-every-time or full access");
    println!("  /clear             forget the conversation context");
    println!("  /compact           summarize the context to save tokens");
    println!("  /memory            show saved project and global notes");
    println!("  /remember <note>   save a note (prefix \"global:\" for all projects)");
    println!("  /stop              interrupt the running turn");
    println!("  /exit              leave Hasty (Ctrl+D also works)");
    println!();
    println!("Everything else is just typed as a normal task. Enter sends.");
    println!();
}

fn pick_model(
    cfg: &mut Config,
    agent: &agent::AgentHandle,
    input: &Input,
    out: &mut Out,
) -> io::Result<()> {
    if cfg.providers.is_empty() {
        out.line("· no providers configured. Run `hasty setup --plain`.");
        return Ok(());
    }
    out.line("");
    out.line("Providers");
    for (index, provider) in cfg.providers.iter().enumerate() {
        let marker = if provider.id == cfg.provider {
            "*"
        } else {
            " "
        };
        out.line(&format!(
            " {marker}{}. {} ({})",
            index + 1,
            providers::display_name(&provider.id),
            provider.id
        ));
    }
    let answer = ask(input, "Number [keep current]: ")?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(());
    }
    let index = answer.parse::<usize>().unwrap_or(0);
    if index == 0 || index > cfg.providers.len() {
        out.line("· no change");
        return Ok(());
    }
    let provider_id = cfg.providers[index - 1].id.clone();
    let models: Vec<String> = cfg
        .provider(&provider_id)
        .map(|p| p.models.clone())
        .unwrap_or_default();
    if models.is_empty() {
        out.line("· that provider has no stored models. Run `hasty setup --plain`.");
        return Ok(());
    }
    out.line("");
    out.line(&format!("{} models", providers::display_name(&provider_id)));
    for (position, model) in models.iter().enumerate().take(40) {
        out.line(&format!("  {}. {model}", position + 1));
    }
    if models.len() > 40 {
        out.line(&format!("  … {} more", models.len() - 40));
    }
    let answer = ask(input, "Number, or paste a model id: ")?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(());
    }
    let chosen = match answer.parse::<usize>() {
        Ok(number) if number >= 1 && number <= models.len() => models[number - 1].clone(),
        _ => answer.to_string(),
    };
    cfg.provider = provider_id;
    cfg.model = chosen.clone();
    cfg.save().map_err(io::Error::other)?;
    agent.send(UserAction::Config(Box::new(cfg.clone())));
    out.line(&format!("· now using {chosen}"));
    Ok(())
}

fn print_choice(
    cfg: &mut Config,
    agent: &agent::AgentHandle,
    input: &Input,
    out: &mut Out,
) -> io::Result<()> {
    out.line("");
    out.line(&format!("Permissions now: {}", cfg.permission_mode.label()));
    out.line("  1. Ask every time (recommended)");
    out.line("  2. Full access (never asks; the catastrophic deny-list still applies)");
    let answer = ask(input, "Choice [1]: ")?;
    let mode = match answer.trim() {
        "" | "1" => Mode::AskEveryTime,
        "2" => Mode::FullAccess,
        _ => {
            out.line("· no change");
            return Ok(());
        }
    };
    cfg.permission_mode = mode;
    cfg.save().map_err(io::Error::other)?;
    agent.send(UserAction::Config(Box::new(cfg.clone())));
    out.line(&format!("· permissions: {}", mode.label()));
    Ok(())
}

fn print_memory(root: &std::path::Path, out: &mut Out) {
    for (label, path) in [
        ("global", memory::global_file()),
        ("project", memory::project_file(root)),
    ] {
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                out.line("");
                out.line(&format!("{label} memory · {}", path.display()));
                print_block(out, &text, 40);
            }
            _ => out.line(&format!("· {label} memory is empty")),
        }
    }
}

fn remember(root: &std::path::Path, note: &str, out: &mut Out) {
    let (scope, text) = match note
        .strip_prefix("global:")
        .or_else(|| note.strip_prefix("global :"))
    {
        Some(rest) => (Scope::Global, rest.trim()),
        None => (Scope::Project, note.trim()),
    };
    if text.is_empty() {
        out.line("· usage: /remember <note>   or   /remember global: <note>");
        return;
    }
    match memory::remember(root, text, scope) {
        Ok((_, memory::Saved::New)) => out.line(&format!("· saved to {} memory", scope.label())),
        Ok((_, memory::Saved::Duplicate)) => out.line("· already in memory"),
        Err(err) => out.line(&format!("! {err}")),
    }
}

// ------------------------------------------------------------------- setup

/// Text setup: numbered choices, keys typed or pasted, nothing echoed.
pub fn setup_flow(cfg: &mut Config, secrets: &mut Secrets, input: &Input) -> io::Result<bool> {
    cfg.onboarded = false;
    println!();
    println!("Hasty {VERSION} — text setup (works with any keyboard)");
    println!("Nothing leaves this machine except requests to the provider you pick.");

    let (provider_id, base, key, models) = loop {
        println!();
        println!("Choose a provider");
        for (index, (id, label)) in providers::PROVIDERS.iter().enumerate() {
            println!("  {}. {label} ({id})", index + 1);
        }
        let answer = ask(input, "Number [1]: ")?;
        let index = answer
            .trim()
            .parse::<usize>()
            .unwrap_or(1)
            .saturating_sub(1);
        if index >= providers::PROVIDERS.len() {
            println!("Pick a number from the list.");
            continue;
        }
        let id = providers::PROVIDERS[index].0;
        let mut base = providers::default_base(id).to_string();
        if id == "custom" {
            let value = ask(input, "Base URL (https://…/v1): ")?;
            if !value.trim().starts_with("http") {
                println!("That does not look like an http(s) URL.");
                continue;
            }
            base = value.trim().trim_end_matches('/').to_string();
        }

        let key = if providers::needs_key(id) {
            println!("Paste the API key and press Enter. It is not echoed.");
            let original = echo_off();
            let value = ask(input, "API key: ");
            echo_restore(original);
            println!();
            match value {
                Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
                _ => {
                    println!("No key entered.");
                    continue;
                }
            }
        } else {
            String::new()
        };

        println!("Checking the key and fetching models…");
        let key_arg = if key.is_empty() {
            None
        } else {
            Some(key.clone())
        };
        let models = match agent::probe(id, Some(&base), key_arg) {
            Ok(models) if !models.is_empty() => models,
            Ok(_) => {
                println!("The provider returned no models.");
                Vec::new()
            }
            Err(err) => {
                println!("Failed: {err}");
                println!("Check the key, or pick another provider.");
                continue;
            }
        };
        break (id.to_string(), base, key, models);
    };

    let model = choose_model(input, &models)?;

    let mode = loop {
        println!();
        println!("Permissions");
        println!("  1. Ask every time (recommended)");
        println!("  2. Full access (never asks; the catastrophic deny-list still applies)");
        let answer = ask(input, "Number [1]: ")?;
        match answer.trim() {
            "" | "1" => break Mode::AskEveryTime,
            "2" => break Mode::FullAccess,
            _ => println!("Pick 1 or 2."),
        }
    };

    let model_ids: Vec<String> = if models.is_empty() {
        vec![model.clone()]
    } else {
        models.iter().map(|m| m.id.clone()).collect()
    };
    cfg.provider = provider_id.clone();
    cfg.model = model.clone();
    cfg.providers.retain(|p| p.id != provider_id);
    cfg.providers.push(ProviderConfig {
        id: provider_id.clone(),
        base_url: Some(base),
        key_ref: if providers::needs_key(&provider_id) {
            Some(format!("provider:{provider_id}"))
        } else {
            None
        },
        models: model_ids,
        free_tier: provider_id == "ollama",
    });
    if providers::needs_key(&provider_id) && !key.is_empty() {
        secrets
            .set(&format!("provider:{provider_id}"), &key)
            .map_err(io::Error::other)?;
    }
    cfg.permission_mode = mode;
    cfg.onboarded = true;
    cfg.save().map_err(io::Error::other)?;

    println!();
    println!("Ready.");
    println!("  Provider: {}", providers::display_name(&cfg.provider));
    println!("  Model: {}", cfg.model);
    println!("  Permissions: {}", cfg.permission_mode.label());
    println!("  Keys: {} vault", secrets.backend().as_str());
    println!("  Subagent keys and connectors can be added later in the full UI.");
    println!();
    Ok(true)
}

fn choose_model(input: &Input, models: &[ModelInfo]) -> io::Result<String> {
    if models.is_empty() {
        let value = ask(input, "Model id: ")?;
        let value = value.trim().to_string();
        return if value.is_empty() {
            Ok(String::new())
        } else {
            Ok(value)
        };
    }
    println!();
    println!("Models");
    for (index, model) in models.iter().enumerate().take(40) {
        let free = if model.free { " · free" } else { "" };
        println!("  {}. {}{free}", index + 1, model.id);
    }
    if models.len() > 40 {
        println!("  … {} more (paste an id to use one)", models.len() - 40);
    }
    let answer = ask(input, "Number [1], or paste a model id: ")?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(models[0].id.clone());
    }
    if let Ok(number) = answer.parse::<usize>() {
        if number >= 1 && number <= models.len() {
            return Ok(models[number - 1].id.clone());
        }
    }
    Ok(answer.to_string())
}

/// `hasty setup --plain` from outside a chat session.
pub fn run_setup_only(
    mut cfg: Config,
    mut secrets: Secrets,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = Input::start();
    if setup_flow(&mut cfg, &mut secrets, &input)? {
        println!("Run `hasty --plain` to start.");
    }
    Ok(())
}
