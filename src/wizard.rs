//! First-run setup wizard: provider, key validation, model pick, permission
//! mode, extra subagent keys and optional connectors.

use std::io;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::{AgentKey, Config, Connector};
use crate::permissions::Mode;
use crate::providers::{self, ModelInfo};
use crate::secrets::{mask, Secrets};
use crate::theme::Theme;
use crate::tui::{enter_terminal, leave_terminal};
use crate::VERSION;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Welcome,
    Provider,
    BaseUrl,
    Key,
    Models,
    Permission,
    ExtraPrompt,
    ExtraForm,
    GithubPrompt,
    GithubToken,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeKind {
    Main,
    Extra,
}

struct Wizard {
    cfg: Config,
    secrets: Secrets,
    theme: Theme,
    step: Step,
    provider_index: usize,
    provider_id: String,
    base: String,
    key_input: String,
    models: Vec<ModelInfo>,
    model_index: usize,
    permission_index: usize,
    extra_index: usize,
    extra_name: String,
    extra_role: String,
    extra_provider: String,
    extra_key: String,
    extra_model: String,
    extras: Vec<AgentKey>,
    token_input: String,
    message: Option<(bool, String)>,
    probe: Option<(ProbeKind, ProbeResult)>,
    github_probe: Option<Receiver<Result<String, String>>>,
    spinner: usize,
    cancelled: bool,
    finished: bool,
}

/// A model-list request in flight from the setup wizard.
type ProbeResult = Receiver<Result<Vec<ModelInfo>, String>>;

/// Run setup. Returns the configured pair, or `None` if the user cancelled.
pub fn run(mut cfg: Config, secrets: Secrets) -> io::Result<Option<(Config, Secrets)>> {
    // Never silently inherit a half-finished config.
    cfg.onboarded = false;
    let theme = cfg.theme_slots();
    let mut wizard = Wizard {
        provider_index: 0,
        provider_id: "openai".into(),
        base: String::new(),
        key_input: String::new(),
        models: Vec::new(),
        model_index: 0,
        permission_index: 1,
        extra_index: 0,
        extra_name: String::new(),
        extra_role: String::new(),
        extra_provider: String::new(),
        extra_key: String::new(),
        extra_model: String::new(),
        extras: Vec::new(),
        token_input: String::new(),
        message: None,
        probe: None,
        github_probe: None,
        spinner: 0,
        cancelled: false,
        finished: false,
        theme,
        step: Step::Welcome,
        cfg,
        secrets,
    };

    let mut terminal = enter_terminal()?;
    let result = loop {
        if let Err(err) = terminal.draw(|frame| draw(frame, &wizard)) {
            break Err(err);
        }
        wizard.poll_probes();
        wizard.spinner = wizard.spinner.wrapping_add(1);
        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) => wizard.on_key(key),
                Event::Paste(text) => wizard.on_paste(&text),
                _ => {}
            }
        }
        if wizard.cancelled {
            break Ok(None);
        }
        if wizard.finished {
            wizard.cfg.onboarded = true;
            wizard.cfg.secret_backend = wizard.secrets.backend().as_str().to_string();
            let _ = wizard.cfg.save();
            break Ok(Some((wizard.cfg.clone(), wizard.secrets)));
        }
    };
    leave_terminal();
    result
}

impl Wizard {
    fn body_width(&self) -> u16 {
        78
    }

    fn poll_probes(&mut self) {
        if let Some((kind, rx)) = &self.probe {
            match rx.try_recv() {
                Ok(result) => {
                    let kind = *kind;
                    self.probe = None;
                    match (kind, result) {
                        (ProbeKind::Main, Ok(models)) => {
                            if models.is_empty() {
                                self.message =
                                    Some((false, "The provider returned no models.".into()));
                                self.step = Step::Key;
                            } else {
                                self.models = models;
                                self.model_index = 0;
                                self.message = None;
                                self.step = Step::Models;
                            }
                        }
                        (ProbeKind::Extra, Ok(models)) => {
                            if models.is_empty() {
                                self.message =
                                    Some((false, "No models returned for that key.".into()));
                                self.extra_index = 3;
                            } else {
                                self.models = models;
                                self.model_index = 0;
                                self.message = None;
                                self.step = Step::ExtraForm;
                                self.extra_index = 4;
                            }
                        }
                        (ProbeKind::Main, Err(err)) => {
                            self.message = Some((false, format!("Key rejected: {err}")));
                            self.step = Step::Key;
                        }
                        (ProbeKind::Extra, Err(err)) => {
                            self.message = Some((false, format!("Key rejected: {err}")));
                            self.extra_index = 3;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.probe = None;
                    self.message = Some((false, "Validation crashed. Try again.".into()));
                }
            }
        }
        if let Some(rx) = &self.github_probe {
            match rx.try_recv() {
                Ok(Ok(login)) => {
                    self.github_probe = None;
                    let key_name = "connector:github".to_string();
                    if let Err(err) = self.secrets.set(&key_name, &self.token_input) {
                        self.message = Some((false, format!("Could not store the token: {err}")));
                        return;
                    }
                    self.cfg.connectors.retain(|c| c.name != "github");
                    self.cfg.connectors.push(Connector {
                        name: "github".into(),
                        kind: "token".into(),
                        enabled: true,
                        token_ref: Some(key_name),
                        detail: Some(format!("signed in as {login}")),
                    });
                    self.message = Some((true, format!("GitHub connected as {login}.")));
                    self.step = Step::Done;
                }
                Ok(Err(err)) => {
                    self.github_probe = None;
                    self.message = Some((false, format!("GitHub rejected the token: {err}")));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.github_probe = None;
                    self.message = Some((false, "GitHub check crashed.".into()));
                }
            }
        }
    }

    fn provider_list() -> Vec<&'static str> {
        providers::PROVIDERS.iter().map(|(id, _)| *id).collect()
    }

    fn start_probe(
        &mut self,
        kind: ProbeKind,
        provider: &str,
        base: Option<String>,
        key: Option<String>,
    ) {
        let (tx, rx) = mpsc::channel();
        let provider = provider.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(crate::agent::probe(&provider, base.as_deref(), key));
        });
        self.probe = Some((kind, rx));
        self.message = None;
    }

    fn on_paste(&mut self, text: &str) {
        match self.step {
            Step::Key => self.key_input.push_str(text.trim()),
            Step::BaseUrl => self.base.push_str(text.trim()),
            Step::ExtraForm if self.extra_index == 3 => self.extra_key.push_str(text.trim()),
            Step::ExtraForm if self.extra_index == 4 => self.extra_model.push_str(text.trim()),
            Step::ExtraForm if self.extra_index == 0 => self.extra_name.push_str(text.trim()),
            Step::ExtraForm if self.extra_index == 1 => self.extra_role.push_str(text.trim()),
            Step::GithubToken => self.token_input.push_str(text.trim()),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.cancelled = true;
            return;
        }
        if key.code == KeyCode::Esc {
            // Back one step where it makes sense, otherwise cancel.
            self.message = None;
            self.step = match self.step {
                Step::Welcome => {
                    self.cancelled = true;
                    Step::Welcome
                }
                Step::Provider => Step::Welcome,
                Step::BaseUrl | Step::Key => Step::Provider,
                Step::Models => Step::Key,
                Step::Permission => Step::Models,
                Step::ExtraPrompt => Step::Permission,
                Step::ExtraForm if self.extra_index == 0 => Step::ExtraPrompt,
                Step::ExtraForm => {
                    self.extra_index -= 1;
                    Step::ExtraForm
                }
                Step::GithubPrompt => Step::ExtraPrompt,
                Step::GithubToken => Step::GithubPrompt,
                Step::Done => Step::Done,
            };
            return;
        }

        if self.probe.is_some() || self.github_probe.is_some() {
            return;
        }

        match self.step.clone() {
            Step::Welcome => {
                if key.code == KeyCode::Enter {
                    self.step = Step::Provider;
                }
            }
            Step::Provider => {
                let list = Self::provider_list();
                match key.code {
                    KeyCode::Up => {
                        self.provider_index = if self.provider_index == 0 {
                            list.len() - 1
                        } else {
                            self.provider_index - 1
                        }
                    }
                    KeyCode::Down => self.provider_index = (self.provider_index + 1) % list.len(),
                    KeyCode::Enter => {
                        self.provider_id = list[self.provider_index].to_string();
                        self.base = providers::default_base(&self.provider_id).to_string();
                        self.key_input.clear();
                        self.message = None;
                        if self.provider_id == "custom" {
                            self.step = Step::BaseUrl;
                        } else if self.provider_id == "ollama" {
                            self.start_probe(
                                ProbeKind::Main,
                                "ollama",
                                Some(self.base.clone()),
                                None,
                            );
                        } else {
                            self.step = Step::Key;
                        }
                    }
                    _ => {}
                }
            }
            Step::BaseUrl => match key.code {
                KeyCode::Enter => {
                    if !self.base.starts_with("http") {
                        self.message =
                            Some((false, "Base URL must start with http:// or https://".into()));
                    } else {
                        self.step = Step::Key;
                    }
                }
                KeyCode::Backspace => {
                    self.base.pop();
                }
                KeyCode::Char(ch) => self.base.push(ch),
                _ => {}
            },
            Step::Key => match key.code {
                KeyCode::Enter => {
                    if self.key_input.trim().is_empty() {
                        self.message = Some((false, "Paste the API key first.".into()));
                    } else {
                        let key = self.key_input.trim().to_string();
                        self.start_probe(
                            ProbeKind::Main,
                            &self.provider_id.clone(),
                            Some(self.base.clone()),
                            Some(key),
                        );
                    }
                }
                KeyCode::Backspace => {
                    self.key_input.pop();
                }
                KeyCode::Char(ch) => self.key_input.push(ch),
                _ => {}
            },
            Step::Models => {
                let count = self.models.len();
                match key.code {
                    KeyCode::Up => {
                        self.model_index = if self.model_index == 0 {
                            count.saturating_sub(1)
                        } else {
                            self.model_index - 1
                        }
                    }
                    KeyCode::Down => self.model_index = (self.model_index + 1) % count.max(1),
                    KeyCode::Enter => {
                        if let Some(model) = self.models.get(self.model_index) {
                            self.cfg.model = model.id.clone();
                            self.cfg.provider = self.provider_id.clone();
                            self.cfg.providers.retain(|p| p.id != self.provider_id);
                            self.cfg.providers.push(crate::config::ProviderConfig {
                                id: self.provider_id.clone(),
                                base_url: Some(self.base.clone()),
                                key_ref: if providers::needs_key(&self.provider_id) {
                                    Some(format!("provider:{}", self.provider_id))
                                } else {
                                    None
                                },
                                models: self.models.iter().map(|m| m.id.clone()).collect(),
                                free_tier: self.provider_id == "ollama"
                                    || (self.models.iter().all(|m| m.free)
                                        && !self.models.is_empty()),
                            });
                            if providers::needs_key(&self.provider_id) {
                                if let Err(err) = self.secrets.set(
                                    &format!("provider:{}", self.provider_id),
                                    self.key_input.trim(),
                                ) {
                                    self.message =
                                        Some((false, format!("Could not store the key: {err}")));
                                    return;
                                }
                            }
                            self.step = Step::Permission;
                        }
                    }
                    _ => {}
                }
            }
            Step::Permission => match key.code {
                KeyCode::Up | KeyCode::Down => {
                    self.permission_index = 1 - self.permission_index.min(1);
                }
                KeyCode::Enter => {
                    self.cfg.permission_mode = if self.permission_index == 0 {
                        Mode::FullAccess
                    } else {
                        Mode::AskEveryTime
                    };
                    self.step = Step::ExtraPrompt;
                }
                _ => {}
            },
            Step::ExtraPrompt => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.extra_name.clear();
                    self.extra_role.clear();
                    self.extra_key.clear();
                    self.extra_model.clear();
                    self.extra_provider = if self.cfg.provider.is_empty() {
                        "openai".into()
                    } else {
                        self.cfg.provider.clone()
                    };
                    self.extra_index = 0;
                    self.message = None;
                    self.step = Step::ExtraForm;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
                    self.step = Step::GithubPrompt;
                }
                _ => {}
            },
            Step::ExtraForm => {
                // Provider picker is a list; everything else is text.
                if self.extra_index == 2 {
                    let list: Vec<String> =
                        self.cfg.providers.iter().map(|p| p.id.clone()).collect();
                    let list = if list.is_empty() {
                        vec!["openai".to_string()]
                    } else {
                        list
                    };
                    let mut index = list
                        .iter()
                        .position(|p| p == &self.extra_provider)
                        .unwrap_or(0);
                    match key.code {
                        KeyCode::Up => {
                            index = if index == 0 {
                                list.len() - 1
                            } else {
                                index - 1
                            };
                            self.extra_provider = list[index].clone();
                        }
                        KeyCode::Down => {
                            index = (index + 1) % list.len();
                            self.extra_provider = list[index].clone();
                        }
                        KeyCode::Enter => {
                            self.extra_index = 3;
                            self.message = None;
                        }
                        _ => {}
                    }
                    return;
                }
                match key.code {
                    KeyCode::Enter => {
                        self.advance_extra();
                    }
                    KeyCode::Backspace => {
                        self.extra_field_mut().pop();
                    }
                    KeyCode::Char(ch) => self.extra_field_mut().push(ch),
                    _ => {}
                }
            }
            Step::GithubPrompt => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.token_input.clear();
                    self.message = None;
                    self.step = Step::GithubToken;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
                    self.step = Step::Done;
                }
                _ => {}
            },
            Step::GithubToken => match key.code {
                KeyCode::Enter => {
                    let token = self.token_input.trim().to_string();
                    if token.is_empty() {
                        self.message =
                            Some((false, "Paste a token, or press n above to skip.".into()));
                    } else {
                        let (tx, rx) = mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(check_github(&token));
                        });
                        self.github_probe = Some(rx);
                        self.message = None;
                    }
                }
                KeyCode::Backspace => {
                    self.token_input.pop();
                }
                KeyCode::Char(ch) => self.token_input.push(ch),
                _ => {}
            },
            Step::Done => {
                if key.code == KeyCode::Enter {
                    self.finished = true;
                }
            }
        }
    }

    fn extra_field_mut(&mut self) -> &mut String {
        match self.extra_index {
            0 => &mut self.extra_name,
            1 => &mut self.extra_role,
            3 => &mut self.extra_key,
            _ => &mut self.extra_model,
        }
    }

    fn advance_extra(&mut self) {
        match self.extra_index {
            0 => {
                if self.extra_name.trim().is_empty() {
                    self.message = Some((false, "Give the subagent a name.".into()));
                    return;
                }
                self.extra_name = self.extra_name.trim().to_lowercase().replace(' ', "-");
                self.extra_index = 1;
            }
            1 => {
                if self.extra_role.trim().is_empty() {
                    self.extra_role = "specialist".into();
                }
                self.extra_index = 2;
            }
            3 => {
                if self.extra_key.trim().is_empty() {
                    self.message = Some((false, "Paste the key for this subagent.".into()));
                    return;
                }
                let provider = self.extra_provider.clone();
                let base = self
                    .cfg
                    .provider(&provider)
                    .and_then(|p| p.base_url.clone())
                    .or_else(|| Some(providers::default_base(&provider).to_string()));
                let key = self.extra_key.trim().to_string();
                self.start_probe(ProbeKind::Extra, &provider, base, Some(key));
            }
            4 => {
                if self.extra_model.trim().is_empty() {
                    self.message = Some((false, "Pick a model (type its name).".into()));
                    return;
                }
                let name = self.extra_name.clone();
                let key_name = format!("subagent:{name}");
                if let Err(err) = self.secrets.set(&key_name, self.extra_key.trim()) {
                    self.message = Some((false, format!("Could not store the key: {err}")));
                    return;
                }
                self.extras.push(AgentKey {
                    name,
                    role: self.extra_role.clone(),
                    provider: self.extra_provider.clone(),
                    key_ref: key_name,
                    model: self.extra_model.trim().to_string(),
                    models: self.models.iter().map(|m| m.id.clone()).collect(),
                });
                self.cfg.subagent_keys = self.extras.clone();
                self.message = Some((true, "Subagent key added.".into()));
                self.step = Step::ExtraPrompt;
            }
            _ => {}
        }
    }
}

fn check_github(token: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent(format!("hasty/{VERSION}"))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| format!("network error: {e}"))?;
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    Ok(body["login"].as_str().unwrap_or("unknown").to_string())
}

// ------------------------------------------------------------------- drawing

fn draw(frame: &mut Frame, wizard: &Wizard) {
    let theme = wizard.theme;
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);
    let logo_h = if area.height > 26 { 7 } else { 4 };
    let chunks = Layout::vertical([
        Constraint::Length(logo_h),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .split(area);

    draw_logo(frame, wizard, chunks[0], logo_h > 4);

    let width = area.width.saturating_sub(6).min(wizard.body_width());
    let body = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        chunks[1].y,
        width,
        chunks[1].height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.panel))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(
            format!(" setup · {} ", step_label(&wizard.step)),
            Style::default().fg(theme.accent).bg(theme.panel),
        ));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    let mut lines: Vec<Line> = Vec::new();
    let push = |lines: &mut Vec<Line>, text: &str, style: Style| {
        lines.push(Line::from(Span::styled(text.to_string(), style)));
    };
    let body_style = Style::default().fg(theme.fg).bg(theme.panel);
    let dim = Style::default().fg(theme.muted).bg(theme.panel);
    let accent = Style::default()
        .fg(theme.accent)
        .bg(theme.panel)
        .add_modifier(Modifier::BOLD);

    match &wizard.step {
        Step::Welcome => {
            push(&mut lines, "Setup takes about a minute.", body_style);
            lines.push(Line::from(""));
            push(
                &mut lines,
                "Hasty runs entirely on this machine. Keys are stored in the OS",
                dim,
            );
            push(
                &mut lines,
                "keychain when available, otherwise an encrypted file (0600).",
                dim,
            );
            lines.push(Line::from(""));
            push(&mut lines, "Warning", accent);
            push(
                &mut lines,
                "Hasty can read, modify and delete anything you can, run any",
                body_style,
            );
            push(
                &mut lines,
                "command, install software and open apps. Use it at your own risk.",
                body_style,
            );
            push(
                &mut lines,
                "Prefer \"Ask every time\" and a VM/container for untrusted projects.",
                dim,
            );
        }
        Step::Provider => {
            push(&mut lines, "Choose a provider:", body_style);
            lines.push(Line::from(""));
            for (index, (id, label)) in providers::PROVIDERS.iter().enumerate() {
                let selected = index == wizard.provider_index;
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(theme.accent).bg(theme.panel),
                    ),
                    Span::styled(
                        format!("{label:<28}"),
                        if selected {
                            Style::default()
                                .fg(theme.fg)
                                .bg(theme.accent_dim)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            body_style
                        },
                    ),
                    Span::styled(
                        providers::default_base(id),
                        if selected {
                            Style::default().fg(theme.fg).bg(theme.accent_dim)
                        } else {
                            dim
                        },
                    ),
                ]));
            }
        }
        Step::BaseUrl => {
            push(
                &mut lines,
                "Base URL for the OpenAI-compatible endpoint:",
                body_style,
            );
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ❯ ", accent),
                Span::styled(wizard.base.clone(), body_style),
                Span::styled("▏", Style::default().fg(theme.accent).bg(theme.panel)),
            ]));
            lines.push(Line::from(""));
            push(&mut lines, "Example: http://localhost:8000/v1", dim);
        }
        Step::Key => {
            push(
                &mut lines,
                &format!(
                    "Paste your {} API key:",
                    providers::display_name(&wizard.provider_id)
                ),
                body_style,
            );
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ❯ ", accent),
                Span::styled(
                    if wizard.key_input.is_empty() {
                        "paste key…".to_string()
                    } else {
                        mask(&wizard.key_input)
                    },
                    if wizard.key_input.is_empty() {
                        dim
                    } else {
                        body_style
                    },
                ),
            ]));
            lines.push(Line::from(""));
            push(
                &mut lines,
                "The key is validated with a test call and never printed.",
                dim,
            );
            push(
                &mut lines,
                &format!("Endpoint: {}", wizard.base),
                Style::default().fg(theme.accent_dim).bg(theme.panel),
            );
        }
        Step::Models => {
            push(&mut lines, "Choose the main model:", body_style);
            lines.push(Line::from(""));
            let height = inner.height.saturating_sub(4) as usize;
            let start = if wizard.model_index >= height {
                wizard.model_index + 1 - height
            } else {
                0
            };
            for (index, model) in wizard.models.iter().enumerate().skip(start).take(height) {
                let selected = index == wizard.model_index;
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(theme.accent).bg(theme.panel),
                    ),
                    Span::styled(
                        format!("{:<40}", model.id),
                        if selected {
                            Style::default()
                                .fg(theme.fg)
                                .bg(theme.accent_dim)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            body_style
                        },
                    ),
                    Span::styled(
                        if model.free { "free" } else { "" },
                        if selected {
                            Style::default().fg(theme.fg).bg(theme.accent_dim)
                        } else {
                            dim
                        },
                    ),
                ]));
            }
        }
        Step::Permission => {
            push(
                &mut lines,
                "How much should Hasty do without asking?",
                body_style,
            );
            lines.push(Line::from(""));
            let options = [
                ("Full access", Mode::FullAccess.describe()),
                (
                    "Ask every time (recommended)",
                    Mode::AskEveryTime.describe(),
                ),
            ];
            for (index, (label, description)) in options.iter().enumerate() {
                let selected = index == wizard.permission_index;
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(theme.accent).bg(theme.panel),
                    ),
                    Span::styled(
                        format!("{label:<30}"),
                        if selected {
                            Style::default()
                                .fg(theme.fg)
                                .bg(theme.accent_dim)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            body_style
                        },
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(
                        *description,
                        if selected {
                            Style::default().fg(theme.fg).bg(theme.panel)
                        } else {
                            dim
                        },
                    ),
                ]));
            }
            lines.push(Line::from(""));
            push(
                &mut lines,
                "Catastrophic commands (rm -rf /, mkfs, dd to a disk) are blocked in both modes.",
                dim,
            );
        }
        Step::ExtraPrompt => {
            push(&mut lines, "Add extra keys for subagents?", body_style);
            lines.push(Line::from(""));
            push(
                &mut lines,
                "Subagents run in parallel with their own key and model, e.g. a",
                dim,
            );
            push(
                &mut lines,
                "reviewer on a cheaper model. Optional. Editable later.",
                dim,
            );
            lines.push(Line::from(""));
            if wizard.extras.is_empty() {
                push(&mut lines, "No extra keys added yet.", dim);
            }
            for extra in &wizard.extras {
                push(
                    &mut lines,
                    &format!(" • {} — {} · {}", extra.name, extra.provider, extra.model),
                    body_style,
                );
            }
            lines.push(Line::from(""));
            push(&mut lines, "y: add another    n / Enter: continue", accent);
        }
        Step::ExtraForm => {
            push(
                &mut lines,
                &format!("Extra key · step {} of 5", wizard.extra_index + 1),
                body_style,
            );
            lines.push(Line::from(""));
            let fields = [
                ("Name", &wizard.extra_name, "e.g. reviewer"),
                ("Role", &wizard.extra_role, "e.g. security review"),
                ("Provider", &wizard.extra_provider, ""),
                ("API key", &wizard.extra_key, "paste key…"),
                ("Model", &wizard.extra_model, "e.g. gpt-5-mini"),
            ];
            for (index, (label, value, hint)) in fields.iter().enumerate() {
                let active = index == wizard.extra_index;
                let display = if index == 3 {
                    if value.is_empty() {
                        hint.to_string()
                    } else {
                        mask(value)
                    }
                } else if value.is_empty() {
                    hint.to_string()
                } else {
                    value.to_string()
                };
                if index == 2 && active {
                    push(
                        &mut lines,
                        &format!(" {}: {} (↑/↓ to change, Enter next)", label, value),
                        accent,
                    );
                    continue;
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {} {label}: ", if active { "›" } else { " " }),
                        if active { accent } else { dim },
                    ),
                    Span::styled(
                        display,
                        if active && value.is_empty() {
                            dim
                        } else {
                            body_style
                        },
                    ),
                ]));
            }
        }
        Step::GithubPrompt => {
            push(&mut lines, "Connect GitHub?", body_style);
            lines.push(Line::from(""));
            push(
                &mut lines,
                "A token lets Hasty clone skill repositories and read issues.",
                dim,
            );
            push(
                &mut lines,
                "Optional. You can add it later in /connectors.",
                dim,
            );
            lines.push(Line::from(""));
            push(&mut lines, "y: paste a token    n / Enter: skip", accent);
        }
        Step::GithubToken => {
            push(&mut lines, "GitHub personal access token:", body_style);
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ❯ ", accent),
                Span::styled(
                    if wizard.token_input.is_empty() {
                        "ghp_…".to_string()
                    } else {
                        mask(&wizard.token_input)
                    },
                    body_style,
                ),
            ]));
            lines.push(Line::from(""));
            push(
                &mut lines,
                "Needs read access to repositories. Stored like an API key.",
                dim,
            );
        }
        Step::Done => {
            push(&mut lines, "Ready.", accent);
            lines.push(Line::from(""));
            push(
                &mut lines,
                &format!(
                    "Provider: {}",
                    providers::display_name(&wizard.cfg.provider)
                ),
                body_style,
            );
            push(
                &mut lines,
                &format!("Model: {}", wizard.cfg.model),
                body_style,
            );
            push(
                &mut lines,
                &format!("Permission mode: {}", wizard.cfg.permission_mode.label()),
                body_style,
            );
            push(
                &mut lines,
                &format!(
                    "Subagent keys: {}    Connectors: {}",
                    wizard.cfg.subagent_keys.len(),
                    wizard.cfg.connectors.len()
                ),
                dim,
            );
            push(
                &mut lines,
                &format!("Secrets: {}", wizard.secrets.backend().as_str()),
                dim,
            );
            lines.push(Line::from(""));
            push(&mut lines, "Press Enter to start.", accent);
        }
    }

    if let Some((ok, message)) = &wizard.message {
        lines.push(Line::from(""));
        let color = if *ok { theme.ok } else { theme.err };
        for line in wrap_lines(message, inner.width.saturating_sub(2) as usize) {
            lines.push(Line::from(vec![
                Span::styled(
                    if *ok { " ✓ " } else { " ✗ " },
                    Style::default().fg(color).bg(theme.panel),
                ),
                Span::styled(line, Style::default().fg(color).bg(theme.panel)),
            ]));
        }
    }
    if wizard.probe.is_some() || wizard.github_probe.is_some() {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {} validating…", frames[wizard.spinner % frames.len()]),
            accent,
        )));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(theme.panel))
            .wrap(Wrap { trim: false }),
        inner,
    );

    let footer = Line::from(vec![
        Span::styled(
            " ↑↓ choose · Enter confirm · Esc back · Ctrl+C quit ",
            Style::default().fg(theme.muted).bg(theme.bg),
        ),
        Span::styled(
            format!("  hasty {VERSION} "),
            Style::default().fg(theme.accent_dim).bg(theme.bg),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().bg(theme.bg)),
        chunks[2],
    );
}

fn draw_logo(frame: &mut Frame, wizard: &Wizard, area: Rect, full: bool) {
    let theme = wizard.theme;
    let mut lines: Vec<Line> = Vec::new();
    if full {
        for raw in crate::LOGO.lines().filter(|line| !line.trim().is_empty()) {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme.accent).bg(theme.bg),
            )));
        }
    } else {
        for raw in [
            "  ██╗  ██╗ █████╗ ███████╗████████╗██╗   ██╗",
            "  ███████║ ███████║███████╗   ██║    ╚████╔╝",
            "  ██╔══██║ ██╔══██║╚════██║   ██║     ╚██╔╝",
        ] {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme.accent).bg(theme.bg),
            )));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn step_label(step: &Step) -> &'static str {
    match step {
        Step::Welcome => "welcome",
        Step::Provider => "1/6 provider",
        Step::BaseUrl => "1/6 endpoint",
        Step::Key => "2/6 api key",
        Step::Models => "3/6 model",
        Step::Permission => "4/6 permissions",
        Step::ExtraPrompt | Step::ExtraForm => "5/6 subagents",
        Step::GithubPrompt | Step::GithubToken => "6/6 connectors",
        Step::Done => "done",
    }
}

fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.chars().count() + word.chars().count() + 1 > width && !current.is_empty() {
            out.push(current.clone());
            current.clear();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
