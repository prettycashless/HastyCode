//! Terminal UI: state, input handling, menus and the event loop.
//!
//! Rendering lives in [`render`]. Mouse support is real SGR mouse events from
//! crossterm; every clickable element is registered as a rectangle each frame.

pub mod render;

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::agent::{self, AgentHandle, AgentInit, Phase, SubagentStatus, UiEvent, UserAction};
use crate::commands;
use crate::config::{Config, ThemeConfig};
use crate::db::{Session, Store};
use crate::permissions::Mode;
use crate::personality;
use crate::providers::{self, Message, ModelInfo};
use crate::secrets::Secrets;
use crate::skills;
use crate::theme::Theme;
use crate::tools;
use crate::VERSION;

// --------------------------------------------------------------------- blocks

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone)]
pub enum Block {
    Logo,
    User(String),
    Assistant(String),
    Thinking {
        text: String,
        collapsed: bool,
        secs: Option<f32>,
        active: bool,
    },
    Tool {
        name: String,
        summary: String,
        state: ToolState,
        output: String,
        diff: Option<String>,
        ms: u128,
        expanded: bool,
    },
    Notice(String),
    Error(String),
}

// ---------------------------------------------------------------------- input

#[derive(Default)]
pub struct Input {
    pub buffer: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub draft: String,
}

impl Input {
    pub fn chars(&self) -> Vec<char> {
        self.buffer.chars().collect()
    }

    pub fn insert(&mut self, text: &str) {
        let mut chars = self.chars();
        let at = self.cursor.min(chars.len());
        for (offset, ch) in text.chars().enumerate() {
            chars.insert(at + offset, ch);
        }
        self.cursor = at + text.chars().count();
        self.buffer = chars.into_iter().collect();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut chars = self.chars();
        chars.remove(self.cursor - 1);
        self.cursor -= 1;
        self.buffer = chars.into_iter().collect();
    }

    pub fn delete(&mut self) {
        let mut chars = self.chars();
        if self.cursor < chars.len() {
            chars.remove(self.cursor);
            self.buffer = chars.into_iter().collect();
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars().len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars().len();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
    }

    pub fn set(&mut self, text: &str) {
        self.buffer = text.to_string();
        self.cursor = self.chars().len();
    }

    pub fn take(&mut self) -> String {
        let text = self.buffer.trim().to_string();
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        text
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = self.buffer.clone();
                let index = self.history.len() - 1;
                let text = self.history[index].clone();
                self.set(&text);
                self.history_index = Some(index);
            }
            Some(0) => {}
            Some(index) => {
                let index = index - 1;
                let text = self.history[index].clone();
                self.set(&text);
                self.history_index = Some(index);
            }
        }
    }

    pub fn history_next(&mut self) {
        match self.history_index {
            None => {}
            Some(index) if index + 1 < self.history.len() => {
                let index = index + 1;
                let text = self.history[index].clone();
                self.set(&text);
                self.history_index = Some(index);
            }
            Some(_) => {
                self.buffer = self.draft.clone();
                self.cursor = self.chars().len();
                self.history_index = None;
            }
        }
    }
}

// ---------------------------------------------------------------------- menus

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuKind {
    Slash,
    Model,
    Provider,
    Theme,
    Sessions,
    Skills,
    Settings,
    Connectors,
    Subagents,
    Help,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub detail: String,
    pub value: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub kind: MenuKind,
    pub title: String,
    pub items: Vec<MenuItem>,
    pub selected: usize,
    pub scroll: usize,
}

impl Menu {
    pub fn new(kind: MenuKind, title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            kind,
            title: title.into(),
            items,
            selected: 0,
            scroll: 0,
        }
    }

    pub fn selected_item(&self) -> Option<&MenuItem> {
        self.items.get(self.selected)
    }

    pub fn up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let mut index = self.selected;
        for _ in 0..self.items.len() {
            index = if index == 0 {
                self.items.len() - 1
            } else {
                index - 1
            };
            if self.items[index].enabled {
                self.selected = index;
                return;
            }
        }
    }

    pub fn down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let mut index = self.selected;
        for _ in 0..self.items.len() {
            index = (index + 1) % self.items.len();
            if self.items[index].enabled {
                self.selected = index;
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub label: String,
    pub value: String,
    pub masked: bool,
    pub hint: String,
}

#[derive(Debug, Clone)]
pub enum FormAction {
    ProviderKey {
        provider: String,
        base: Option<String>,
        label: String,
    },
    GithubToken,
    ThemeCustom {
        slot: String,
    },
}

#[derive(Debug, Clone)]
pub struct Form {
    pub title: String,
    pub fields: Vec<FormField>,
    pub index: usize,
    pub action: FormAction,
    pub note: Option<String>,
    pub submit_requested: bool,
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<FormField>, action: FormAction) -> Self {
        Self {
            title: title.into(),
            fields,
            index: 0,
            action,
            note: None,
            submit_requested: false,
        }
    }

    fn field_mut(&mut self) -> &mut FormField {
        let index = self.index.min(self.fields.len().saturating_sub(1));
        &mut self.fields[index]
    }
}

pub fn field(label: &str, value: &str, masked: bool, hint: &str) -> FormField {
    FormField {
        label: label.into(),
        value: value.into(),
        masked,
        hint: hint.into(),
    }
}

#[derive(Debug, Clone)]
pub struct PermissionModal {
    pub id: u64,
    pub prompt: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    SlashItem(usize),
    MenuItem(usize),
    MenuClose,
    ToggleThinking(usize),
    ToggleTool(usize),
    Allow,
    Deny,
    FormNext,
    FormCancel,
    Transcript,
}

pub struct Probe {
    rx: Receiver<Result<Vec<ModelInfo>, String>>,
    action: ProbeAction,
    key: Option<String>,
}

enum ProbeAction {
    SaveProviderKey {
        provider: String,
        base: Option<String>,
        key_name: String,
    },
    GithubToken {
        key_name: String,
    },
}

// ------------------------------------------------------------------------ app

pub struct App {
    pub cfg: Config,
    pub secrets: Secrets,
    pub store: Store,
    pub root: PathBuf,
    pub theme: Theme,
    pub agent: AgentHandle,
    pub blocks: Vec<Block>,
    pub input: Input,
    pub phase: Phase,
    pub busy: bool,
    pub session_id: String,
    pub session_title: String,
    pub usage_in: u64,
    pub usage_out: u64,
    pub model_shown: String,
    pub provider_shown: String,
    pub menu: Option<Menu>,
    pub form: Option<Form>,
    pub permission: Option<PermissionModal>,
    pub subagents: Vec<SubagentStatus>,
    pub sessions: Vec<Session>,
    pub scroll_from_bottom: usize,
    pub quit: bool,
    pub quit_armed: bool,
    pub clicks: Vec<(Rect, Click)>,
    pub menu_area: Option<Rect>,
    pub transcript_area: Option<Rect>,
    pub status_note: String,
    pub spinner: usize,
    pub probe: Option<Probe>,
    pub suggestions: Vec<commands::CommandInfo>,
    pub suggestion_index: usize,
    pub show_slash: bool,
    form_kind: Option<FormKind>,
    tool_blocks: Vec<(u64, usize)>,
    thinking_block: Option<usize>,
    last_escape: Option<std::time::Instant>,
}

impl App {
    pub fn new(
        cfg: Config,
        secrets: Secrets,
        store: Store,
        root: PathBuf,
        session_id: String,
        history: Vec<Message>,
        session_title: String,
    ) -> io::Result<Self> {
        let theme = cfg.theme_slots();
        let provider_shown = providers::display_name(&cfg.provider).to_string();
        let model_shown = cfg.model.clone();
        let agent = agent::spawn(AgentInit {
            root: root.clone(),
            config: cfg.clone(),
            secrets: secrets_open_again(&cfg),
            store: Store::open_default().map_err(io::Error::other)?,
            session_id: session_id.clone(),
            history,
        });
        let mut blocks = vec![Block::Logo];
        let mode = cfg.permission_mode;
        blocks.push(Block::Notice(format!(
            "{} · {}. Type a task, or /help for commands.",
            providers::display_name(&cfg.provider),
            mode.label()
        )));
        if mode == Mode::FullAccess {
            blocks.push(Block::Notice(
                "Full access is on. Hasty can modify and delete anything here; the catastrophic deny-list still applies.".into(),
            ));
        }
        Ok(Self {
            cfg,
            secrets,
            store,
            root,
            theme,
            agent,
            blocks,
            input: Input::default(),
            phase: Phase::Idle,
            busy: false,
            session_id,
            session_title,
            usage_in: 0,
            usage_out: 0,
            model_shown,
            provider_shown,
            menu: None,
            form: None,
            permission: None,
            subagents: Vec::new(),
            sessions: Vec::new(),
            scroll_from_bottom: 0,
            quit: false,
            quit_armed: false,
            clicks: Vec::new(),
            menu_area: None,
            transcript_area: None,
            status_note: String::new(),
            spinner: 0,
            probe: None,
            suggestions: Vec::new(),
            suggestion_index: 0,
            show_slash: false,
            form_kind: None,
            tool_blocks: Vec::new(),
            thinking_block: None,
            last_escape: None,
        })
    }

    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
        if self.blocks.len() > 400 {
            // Keep the logo and trim the oldest conversation block, keeping the
            // stored block indices in sync so later updates hit the right tool.
            self.blocks.remove(1);
            self.tool_blocks = self
                .tool_blocks
                .iter()
                .filter_map(|(id, index)| match *index {
                    0 | 1 => None,
                    other => Some((*id, other - 1)),
                })
                .collect();
            self.thinking_block = self.thinking_block.and_then(|index| index.checked_sub(1));
        }
        self.follow();
    }

    /// Drop the visible transcript without leaving stale block indices behind.
    pub fn clear_transcript(&mut self) {
        self.blocks.truncate(1);
        self.tool_blocks.clear();
        self.thinking_block = None;
    }

    pub fn follow(&mut self) {
        self.scroll_from_bottom = 0;
    }

    fn sync_config(&mut self) {
        let _ = self.cfg.save();
        self.agent
            .send(UserAction::Config(Box::new(self.cfg.clone())));
        self.theme = self.cfg.theme_slots();
        self.provider_shown = providers::display_name(&self.cfg.provider).to_string();
        self.model_shown = self.cfg.model.clone();
    }

    fn save_secrets(&mut self) {
        self.cfg.secret_backend = self.secrets.backend().as_str().to_string();
        let _ = self.cfg.save();
    }

    // ------------------------------------------------------------- agent events

    pub fn on_agent_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Ready { session_id } => self.session_id = session_id,
            UiEvent::Phase(phase) => {
                self.phase = phase;
                self.busy = phase != Phase::Idle;
            }
            UiEvent::ThinkingStart => {
                self.push(Block::Thinking {
                    text: String::new(),
                    collapsed: true,
                    secs: None,
                    active: true,
                });
                self.thinking_block = Some(self.blocks.len() - 1);
            }
            UiEvent::ThinkingDelta(text) => {
                let index = match self.thinking_block {
                    Some(index) if index < self.blocks.len() => index,
                    _ => {
                        self.push(Block::Thinking {
                            text: String::new(),
                            collapsed: true,
                            secs: None,
                            active: true,
                        });
                        let index = self.blocks.len() - 1;
                        self.thinking_block = Some(index);
                        index
                    }
                };
                if let Block::Thinking { text: body, .. } = &mut self.blocks[index] {
                    body.push_str(&text);
                }
                self.follow();
            }
            UiEvent::ThinkingEnd { secs } => {
                if let Some(index) = self.thinking_block.take() {
                    if let Some(Block::Thinking {
                        secs: stamp,
                        active,
                        ..
                    }) = self.blocks.get_mut(index)
                    {
                        *stamp = Some(secs);
                        *active = false;
                    }
                }
            }
            UiEvent::TextDelta(text) => {
                match self.blocks.last_mut() {
                    Some(Block::Assistant(body)) => body.push_str(&text),
                    _ => self.push(Block::Assistant(text)),
                }
                self.follow();
            }
            UiEvent::ToolStart { id, name, summary } => {
                self.push(Block::Tool {
                    name,
                    summary,
                    state: ToolState::Running,
                    output: String::new(),
                    diff: None,
                    ms: 0,
                    expanded: false,
                });
                let index = self.blocks.len() - 1;
                self.tool_blocks.push((id, index));
            }
            UiEvent::ToolEnd {
                id,
                name,
                ok,
                output,
                diff,
                ms,
            } => {
                let index = self
                    .tool_blocks
                    .iter()
                    .rev()
                    .find(|(tool_id, _)| *tool_id == id)
                    .map(|(_, index)| *index);
                if let Some(Block::Tool {
                    state,
                    output: body,
                    diff: stored,
                    ms: duration,
                    name: stored_name,
                    ..
                }) = index.and_then(|index| self.blocks.get_mut(index))
                {
                    *state = if ok { ToolState::Ok } else { ToolState::Failed };
                    *body = output;
                    *stored = diff;
                    *duration = ms;
                    *stored_name = name;
                }
                self.follow();
            }
            UiEvent::Notice(text) => self.push(Block::Notice(text)),
            UiEvent::Error(text) => self.push(Block::Error(text)),
            UiEvent::Permission { id, prompt, detail } => {
                self.permission = Some(PermissionModal { id, prompt, detail });
            }
            UiEvent::Subagents(list) => self.subagents = list,
            UiEvent::Usage {
                input,
                output,
                model,
            } => {
                self.usage_in += input;
                self.usage_out += output;
                if !model.is_empty() {
                    self.model_shown = model;
                }
            }
            UiEvent::ContextReset { summary } => {
                self.clear_transcript();
                self.push(Block::Notice(summary));
                self.follow();
            }
            UiEvent::AttemptReset => {
                // Discard text streamed by the attempt that just failed; the
                // retry will stream its own answer from the start.
                if matches!(self.blocks.last(), Some(Block::Assistant(_))) {
                    self.blocks.pop();
                }
                self.thinking_block = None;
                self.follow();
            }
            UiEvent::SessionTitle(title) => self.session_title = title,
            UiEvent::Done { summary } => {
                self.phase = Phase::Idle;
                self.busy = false;
                if let Some(summary) = summary {
                    let already = matches!(self.blocks.last(), Some(Block::Assistant(text)) if !text.trim().is_empty());
                    if !already && !summary.trim().is_empty() {
                        self.push(Block::Assistant(summary));
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------------- keys

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.quit_armed = false;

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.busy {
                self.agent.send(UserAction::Interrupt);
                self.status_note = "interrupt sent".into();
            } else {
                self.quit = true;
            }
            return;
        }

        if let Some(modal) = self.permission.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.answer_permission(true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.answer_permission(false)
                }
                _ => {}
            }
            let _ = modal;
            return;
        }

        if let Some(form) = &mut self.form {
            match key.code {
                KeyCode::Esc => self.form = None,
                KeyCode::Enter => {
                    if form.index + 1 < form.fields.len() {
                        form.index += 1;
                    } else {
                        form.submit_requested = true;
                    }
                }
                KeyCode::Tab => {
                    form.index = (form.index + 1) % form.fields.len();
                }
                KeyCode::BackTab => {
                    form.index = if form.index == 0 {
                        form.fields.len() - 1
                    } else {
                        form.index - 1
                    };
                }
                KeyCode::Char(ch) => form.field_mut().value.push(ch),
                KeyCode::Backspace => {
                    form.field_mut().value.pop();
                }
                KeyCode::Up => {
                    form.index = form.index.saturating_sub(1);
                }
                KeyCode::Down => {
                    form.index = (form.index + 1).min(form.fields.len().saturating_sub(1));
                }
                _ => {}
            }
            if self
                .form
                .as_ref()
                .map(|form| form.submit_requested)
                .unwrap_or(false)
            {
                self.submit_form();
            }
            return;
        }

        if let Some(menu) = &mut self.menu {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.menu = None,
                KeyCode::Up | KeyCode::Char('k') => menu.up(),
                KeyCode::Down | KeyCode::Char('j') => menu.down(),
                KeyCode::PageUp => {
                    menu.selected = menu.selected.saturating_sub(8);
                }
                KeyCode::PageDown => {
                    menu.selected = (menu.selected + 8).min(menu.items.len().saturating_sub(1));
                }
                KeyCode::Enter => self.menu_select(),
                KeyCode::Char('d') if menu.kind == MenuKind::Sessions => {
                    self.delete_selected_session()
                }
                _ => {}
            }
            return;
        }

        if key.code == KeyCode::Esc {
            if self.busy {
                self.agent.send(UserAction::Interrupt);
                self.status_note = "interrupt sent".into();
                return;
            }
            if self.show_slash && !self.suggestions.is_empty() {
                self.show_slash = false;
                return;
            }
            if !self.input.buffer.is_empty() {
                self.input.clear();
                return;
            }
            let now = std::time::Instant::now();
            if self
                .last_escape
                .map(|at| now.duration_since(at) < Duration::from_millis(1200))
                .unwrap_or(false)
            {
                self.quit = true;
            } else {
                self.status_note = "esc again to exit".into();
                self.last_escape = Some(now);
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('t') => {
                    self.toggle_last_thinking();
                    return;
                }
                KeyCode::Char('o') => {
                    for block in self.blocks.iter_mut() {
                        if let Block::Thinking { collapsed, .. } = block {
                            *collapsed = !*collapsed;
                        }
                    }
                    return;
                }
                KeyCode::Char('j') => {
                    self.input.insert("\n");
                    return;
                }
                KeyCode::Char('l') => {
                    self.clear_transcript();
                    return;
                }
                KeyCode::Char('p') => {
                    self.open_slash();
                    return;
                }
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Enter {
            self.input.insert("\n");
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if self.show_slash && !self.suggestions.is_empty() {
                    self.accept_suggestion();
                } else {
                    self.submit();
                }
            }
            KeyCode::Tab => {
                if self.show_slash && !self.suggestions.is_empty() {
                    self.accept_suggestion();
                }
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.update_suggestions();
            }
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up => {
                if self.show_slash && !self.suggestions.is_empty() {
                    self.suggestion_index = if self.suggestion_index == 0 {
                        self.suggestions.len() - 1
                    } else {
                        self.suggestion_index - 1
                    };
                } else {
                    self.input.history_prev();
                }
            }
            KeyCode::Down => {
                if self.show_slash && !self.suggestions.is_empty() {
                    self.suggestion_index = (self.suggestion_index + 1) % self.suggestions.len();
                } else {
                    self.input.history_next();
                }
            }
            KeyCode::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
            }
            KeyCode::Char(ch) => {
                if !key.modifiers.contains(KeyModifiers::ALT) {
                    self.input.insert(&ch.to_string());
                    self.update_suggestions();
                }
            }
            KeyCode::F(1) => self.open_slash(),
            _ => {}
        }
    }

    pub fn on_paste(&mut self, text: String) {
        if self.permission.is_some() || self.form.is_some() || self.menu.is_some() {
            return;
        }
        let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert(&cleaned);
        self.update_suggestions();
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(3);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let point = Rect::new(mouse.column, mouse.row, 1, 1);
                let mut hit = None;
                for (rect, target) in self.clicks.iter().rev() {
                    if intersects(*rect, point) {
                        hit = Some(target.clone());
                        break;
                    }
                }
                match hit {
                    Some(Click::SlashItem(index)) => {
                        self.suggestion_index = index;
                        self.accept_suggestion();
                    }
                    Some(Click::MenuItem(index)) => {
                        if let Some(menu) = &mut self.menu {
                            if index < menu.items.len() {
                                menu.selected = index;
                                self.menu_select();
                            }
                        }
                    }
                    Some(Click::MenuClose) => self.menu = None,
                    Some(Click::ToggleThinking(index)) => {
                        if let Some(Block::Thinking { collapsed, .. }) = self.blocks.get_mut(index)
                        {
                            *collapsed = !*collapsed;
                        }
                    }
                    Some(Click::ToggleTool(index)) => self.toggle_tool_expansion(index),
                    Some(Click::Allow) => self.answer_permission(true),
                    Some(Click::Deny) => self.answer_permission(false),
                    Some(Click::FormNext) => self.submit_form(),
                    Some(Click::FormCancel) => self.form = None,
                    Some(Click::Transcript) | None => {}
                }
            }
            _ => {}
        }
    }

    fn toggle_last_thinking(&mut self) {
        for index in (0..self.blocks.len()).rev() {
            if let Block::Thinking { collapsed, .. } = &mut self.blocks[index] {
                *collapsed = !*collapsed;
                return;
            }
        }
    }

    fn toggle_tool_expansion(&mut self, index: usize) {
        if let Some(Block::Tool { expanded, .. }) = self.blocks.get_mut(index) {
            *expanded = !*expanded;
        }
    }

    // ------------------------------------------------------------------ submit

    fn update_suggestions(&mut self) {
        let buffer = self.input.buffer.clone();
        if buffer.starts_with('/') && !buffer.contains(' ') && !buffer.contains('\n') {
            self.suggestions = commands::matching(&buffer);
            self.show_slash = true;
            self.suggestion_index = self
                .suggestion_index
                .min(self.suggestions.len().saturating_sub(1));
        } else {
            self.show_slash = false;
            self.suggestions.clear();
        }
    }

    fn open_slash(&mut self) {
        self.input.set("/");
        self.suggestion_index = 0;
        self.update_suggestions();
    }

    fn accept_suggestion(&mut self) {
        if self.suggestions.is_empty() {
            return;
        }
        let index = self.suggestion_index.min(self.suggestions.len() - 1);
        let command = self.suggestions[index].name.to_string();
        self.input.set(&format!("{command} "));
        self.show_slash = false;
        self.suggestions.clear();
        if !command.is_empty() {
            let needs_arg = matches!(command.as_str(), "/theme" | "/skills");
            if !needs_arg {
                self.input.set(&command);
                self.submit();
            }
        }
    }

    fn submit(&mut self) {
        let text = self.input.take();
        if text.is_empty() {
            return;
        }
        self.show_slash = false;
        self.suggestions.clear();
        self.status_note.clear();
        if text.starts_with('/') {
            self.handle_command(&text);
        } else {
            self.push(Block::User(text.clone()));
            self.thinking_block = None;
            self.tool_blocks.clear();
            self.agent.send(UserAction::Prompt(text));
            self.phase = Phase::Working;
            self.busy = true;
            self.follow();
        }
    }

    // ---------------------------------------------------------------- commands

    pub fn handle_command(&mut self, line: &str) {
        let (command, rest) = commands::parse(line);
        match command.as_str() {
            "/exit" | "/quit" => self.quit = true,
            "/help" => self.open_help(),
            "/model" => self.open_model_menu(),
            "/theme" => self.theme_command(&rest),
            "/settings" => self.open_settings(),
            "/subagents" => self.open_subagents(),
            "/skills" => self.skills_command(&rest),
            "/connectors" => self.open_connectors(),
            "/sessions" => {
                if rest == "export" {
                    match commands::export_session(&self.store, None, Some(&self.session_id)) {
                        Ok(path) => {
                            self.push(Block::Notice(format!("Exported to {}", path.display())))
                        }
                        Err(err) => self.push(Block::Error(err)),
                    }
                } else {
                    self.open_sessions();
                }
            }
            "/clear" => {
                self.clear_transcript();
                self.agent.send(UserAction::ClearHistory);
                self.push(Block::Notice(
                    "Context cleared, including this session's saved messages.".into(),
                ));
            }
            "/memory" => {
                let mut shown = false;
                for (label, path) in crate::memory::files(&self.root) {
                    let text = crate::memory::read(&path);
                    if text.trim().is_empty() {
                        continue;
                    }
                    shown = true;
                    self.push(Block::Notice(format!(
                        "{label} memory — {}\n{}",
                        path.display(),
                        text.trim()
                    )));
                }
                if !shown {
                    self.push(Block::Notice(format!(
                        "No memory saved yet. Use /remember <note>; files live in {}.",
                        crate::memory::memory_dir().display()
                    )));
                }
            }
            "/remember" => {
                let trimmed = rest.trim();
                let (scope, note) = match trimmed.strip_prefix("global:") {
                    Some(note) => (crate::memory::Scope::Global, note.trim()),
                    None => (crate::memory::Scope::Project, trimmed),
                };
                if note.is_empty() {
                    self.push(Block::Error(
                        "Usage: /remember <note>  or  /remember global: <note>".into(),
                    ));
                } else {
                    match crate::memory::remember(&self.root, note, scope) {
                        Ok((path, crate::memory::Saved::New)) => self.push(Block::Notice(format!(
                            "Saved to {} memory ({}).",
                            scope.label(),
                            path.display()
                        ))),
                        Ok((path, crate::memory::Saved::Duplicate)) => self.push(Block::Notice(
                            format!("Already in {} memory ({}).", scope.label(), path.display()),
                        )),
                        Err(err) => self.push(Block::Error(err)),
                    }
                }
            }
            "/compact" => {
                self.agent.send(UserAction::Compact);
                self.push(Block::Notice("Compacting context…".into()));
            }
            "/init" => self.run_init(),
            other => self.push(Block::Error(format!(
                "Unknown command `{other}`. Try /help."
            ))),
        }
    }

    fn run_init(&mut self) {
        let path = personality::project_notes_path(&self.root);
        let notes = commands::generate_project_notes(&self.root);
        match std::fs::write(&path, notes) {
            Ok(_) => self.push(Block::Notice(format!(
                "Wrote {}. Edit it to describe conventions; Hasty reads it every turn.",
                path.display()
            ))),
            Err(err) => self.push(Block::Error(format!(
                "Could not write {}: {err}",
                path.display()
            ))),
        }
    }

    fn theme_command(&mut self, rest: &str) {
        if rest.is_empty() {
            let items = Theme::preset_names()
                .iter()
                .map(|name| MenuItem {
                    label: name.to_string(),
                    detail: if *name == self.cfg.theme.name {
                        "current".into()
                    } else {
                        String::new()
                    },
                    value: name.to_string(),
                    enabled: true,
                })
                .collect();
            self.menu = Some(Menu::new(MenuKind::Theme, "Theme", items));
            return;
        }
        let (action, arg) = commands::parse(rest);
        if action == "set" {
            let mut parts = arg.split_whitespace();
            let slot = parts.next().unwrap_or_default().to_string();
            let value = parts.next().unwrap_or_default().to_string();
            if slot.is_empty() || value.is_empty() {
                self.push(Block::Error("Usage: /theme set <slot> <#rrggbb>".into()));
                return;
            }
            if crate::theme::parse(&value).is_none() {
                self.push(Block::Error(format!("`{value}` is not a color")));
                return;
            }
            if !crate::theme::SLOTS.contains(&slot.as_str()) {
                self.push(Block::Error(format!(
                    "Unknown slot `{slot}`. Slots: {}",
                    crate::theme::SLOTS.join(", ")
                )));
                return;
            }
            self.cfg.theme.custom.insert(slot.clone(), value.clone());
            self.sync_config();
            self.push(Block::Notice(format!("Set {slot} to {value}.")));
            return;
        }
        if Theme::preset(&action).is_none() {
            self.push(Block::Error(format!(
                "Unknown theme `{action}`. Options: {}",
                Theme::preset_names().join(", ")
            )));
            return;
        }
        self.cfg.theme = ThemeConfig {
            name: action.clone(),
            custom: Default::default(),
        };
        self.sync_config();
        self.push(Block::Notice(format!("Theme: {action}")));
    }

    fn skills_command(&mut self, rest: &str) {
        let (action, arg) = commands::parse(rest);
        match action.as_str() {
            "" => self.open_skills(),
            "install" => {
                if arg.is_empty() {
                    self.push(Block::Notice(format!(
                        "Usage: /skills install <github-url>. Suggested: {}",
                        skills::DEFAULT_SUGGESTION
                    )));
                    return;
                }
                self.push(Block::Notice(format!("Installing {arg}…")));
                let token = self.github_token();
                match skills::install(&arg, token.as_deref()) {
                    Ok(entries) => {
                        let (_, slug) = skills::parse_source(&arg).unwrap_or_default();
                        if !slug.is_empty() && !self.cfg.skill_repos.contains(&slug) {
                            self.cfg.skill_repos.push(slug.clone());
                        }
                        for entry in &entries {
                            if !self.cfg.enabled_skills.contains(&entry.name) {
                                self.cfg.enabled_skills.push(entry.name.clone());
                            }
                        }
                        self.sync_config();
                        self.push(Block::Notice(format!(
                            "Installed {slug}: {} skills enabled. They load on demand.",
                            entries.len()
                        )));
                        self.open_skills();
                    }
                    Err(err) => self.push(Block::Error(err)),
                }
            }
            "remove" => {
                if arg.is_empty() {
                    self.push(Block::Notice("Usage: /skills remove <owner/repo>".into()));
                    return;
                }
                match skills::remove_repo(&arg) {
                    Ok(_) => {
                        self.cfg.skill_repos.retain(|r| r != &arg);
                        self.sync_config();
                        self.push(Block::Notice(format!("Removed {arg}.")));
                    }
                    Err(err) => self.push(Block::Error(err)),
                }
            }
            other => self.push(Block::Error(format!("Unknown /skills option `{other}`"))),
        }
    }

    fn github_token(&self) -> Option<String> {
        self.cfg
            .connectors
            .iter()
            .find(|c| c.name == "github")
            .and_then(|c| c.token_ref.as_ref())
            .and_then(|reference| self.secrets.get(reference))
    }

    // ------------------------------------------------------------------- menus

    fn open_help(&mut self) {
        let items = vec![
            MenuItem {
                label: "Enter".into(),
                detail: "send · in menus, select".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Alt+Enter / Ctrl+J".into(),
                detail: "newline in the input".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Up / Down".into(),
                detail: "input history · menu navigation".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Tab".into(),
                detail: "complete the highlighted slash command".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Esc / Ctrl+C".into(),
                detail: "interrupt the agent · Esc twice exits".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Ctrl+T".into(),
                detail: "expand/collapse the last thinking block".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Ctrl+O".into(),
                detail: "expand/collapse every thinking block".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "Mouse".into(),
                detail: "click menus, thinking headers and tool blocks; wheel scrolls".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "PgUp / PgDn".into(),
                detail: "scroll the transcript".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/model".into(),
                detail: "switch provider and model".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/settings".into(),
                detail: "permissions, keys, connectors, paths".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/skills".into(),
                detail: "install and enable skills".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/subagents".into(),
                detail: "subagent status and extra keys".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/sessions".into(),
                detail: "browse, resume, delete, export".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/sessions export".into(),
                detail: "write a JSON export".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/compact".into(),
                detail: "summarize context".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/memory".into(),
                detail: "show saved project and global memory".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/remember".into(),
                detail: "save a durable note (global: <note> for everywhere)".into(),
                value: String::new(),
                enabled: false,
            },
            MenuItem {
                label: "/init".into(),
                detail: "write project notes (HASTY.md)".into(),
                value: String::new(),
                enabled: false,
            },
        ];
        self.menu = Some(Menu::new(MenuKind::Help, "Help", items));
    }

    fn open_model_menu(&mut self) {
        let mut items = Vec::new();
        for provider in &self.cfg.providers {
            for model in &provider.models {
                items.push(MenuItem {
                    label: format!("{} · {model}", providers::display_name(&provider.id)),
                    detail: if provider.free_tier {
                        "free tier".into()
                    } else if self.cfg.provider == provider.id && self.cfg.model == *model {
                        "current".into()
                    } else {
                        String::new()
                    },
                    value: format!("{}\u{1f}{model}", provider.id),
                    enabled: true,
                });
            }
        }
        items.push(MenuItem {
            label: "Add or replace a provider key…".into(),
            detail: "opens provider selection".into(),
            value: "__add_provider".into(),
            enabled: true,
        });
        if items.len() == 1 {
            items.insert(
                0,
                MenuItem {
                    label: "No providers configured yet".into(),
                    detail: "use the item below to add one".into(),
                    value: String::new(),
                    enabled: false,
                },
            );
        }
        self.menu = Some(Menu::new(MenuKind::Model, "Model", items));
    }

    fn open_provider_menu(&mut self) {
        let items = providers::PROVIDERS
            .iter()
            .map(|(id, label)| MenuItem {
                label: label.to_string(),
                detail: if self.cfg.provider(id).is_some() {
                    "configured".into()
                } else {
                    String::new()
                },
                value: id.to_string(),
                enabled: true,
            })
            .collect();
        self.menu = Some(Menu::new(MenuKind::Provider, "Provider", items));
    }

    fn open_settings(&mut self) {
        let mode_detail = format!("current: {}", self.cfg.permission_mode.label());
        let items = vec![
            MenuItem {
                label: "Permission mode: Ask every time".into(),
                detail: mode_detail.clone(),
                value: "mode:ask".into(),
                enabled: true,
            },
            MenuItem {
                label: "Permission mode: Full access".into(),
                detail: "runs tools without prompts".into(),
                value: "mode:full".into(),
                enabled: true,
            },
            MenuItem {
                label: "Add or replace a provider key".into(),
                detail: format!("{} provider(s) configured", self.cfg.providers.len()),
                value: "key:add".into(),
                enabled: true,
            },
            MenuItem {
                label: "Add a subagent key".into(),
                detail: format!("{} extra key(s)", self.cfg.subagent_keys.len()),
                value: "subagents".into(),
                enabled: true,
            },
            MenuItem {
                label: "Connectors".into(),
                detail: format!("{} configured", self.cfg.connectors.len()),
                value: "connectors".into(),
                enabled: true,
            },
            MenuItem {
                label: "Edit personality".into(),
                detail: personality::path().display().to_string(),
                value: "persona".into(),
                enabled: true,
            },
            MenuItem {
                label: "Storage".into(),
                detail: format!("SQLite at {}", crate::paths::db_file().display()),
                value: "storage".into(),
                enabled: true,
            },
            MenuItem {
                label: "Toggle reads outside the project".into(),
                detail: if self.cfg.allow_outside_project_reads {
                    "reads allowed (ask mode still confirms sensitive paths)".into()
                } else {
                    "reads outside the project ask first".into()
                },
                value: "outside_reads".into(),
                enabled: true,
            },
            MenuItem {
                label: "Secret backend".into(),
                detail: format!(
                    "{} · {}",
                    self.secrets.backend().as_str(),
                    crate::paths::vault_file().display()
                ),
                value: "backend".into(),
                enabled: true,
            },
        ];
        self.menu = Some(Menu::new(MenuKind::Settings, "Settings", items));
    }

    fn open_subagents(&mut self) {
        let mut items = Vec::new();
        for status in &self.subagents {
            items.push(MenuItem {
                label: format!("{} · {}", status.name, status.status),
                detail: format!(
                    "{} · {} · {}ms · {}",
                    status.provider, status.model, status.elapsed_ms, status.health
                ),
                value: String::new(),
                enabled: false,
            });
        }
        for key in &self.cfg.subagent_keys {
            items.push(MenuItem {
                label: format!("key: {}", key.name),
                detail: format!("{} · {} · {}", key.provider, key.model, key.role),
                value: format!("view:{}", key.name),
                enabled: false,
            });
        }
        items.push(MenuItem {
            label: "Add a subagent key…".into(),
            detail: "provider, key, model and role label".into(),
            value: "__add".into(),
            enabled: true,
        });
        items.push(MenuItem {
            label: "Remove a subagent key…".into(),
            detail: "select which one on the next screen".into(),
            value: "__remove".into(),
            enabled: !self.cfg.subagent_keys.is_empty(),
        });
        self.menu = Some(Menu::new(MenuKind::Subagents, "Subagents", items));
    }

    fn open_connectors(&mut self) {
        let github = self
            .cfg
            .connectors
            .iter()
            .find(|c| c.name == "github")
            .map(|c| c.enabled)
            .unwrap_or(false);
        let items = vec![
            MenuItem {
                label: "GitHub".into(),
                detail: if github {
                    "token stored · used for skill installs".into()
                } else {
                    "not connected · add a personal access token".into()
                },
                value: "github".into(),
                enabled: true,
            },
            MenuItem {
                label: "Device login".into(),
                detail: "needs a GitHub OAuth client id (not bundled); use a token instead".into(),
                value: "github_device".into(),
                enabled: false,
            },
        ];
        self.menu = Some(Menu::new(MenuKind::Connectors, "Connectors", items));
    }

    fn open_sessions(&mut self) {
        self.sessions = self.store.list_sessions(100).unwrap_or_default();
        let items = self
            .sessions
            .iter()
            .map(|session| MenuItem {
                label: if session.title.is_empty() {
                    format!("session {}", &session.id[..8.min(session.id.len())])
                } else {
                    session.title.clone()
                },
                detail: format!(
                    "{} · {} · {} messages",
                    session.model,
                    relative_time(session.updated_at),
                    self.store
                        .messages(&session.id)
                        .map(|m| m.len())
                        .unwrap_or(0)
                ),
                value: session.id.clone(),
                enabled: true,
            })
            .collect::<Vec<_>>();
        let mut items = items;
        if items.is_empty() {
            items.push(MenuItem {
                label: "No saved sessions yet".into(),
                detail: String::new(),
                value: String::new(),
                enabled: false,
            });
        }
        self.menu = Some(Menu::new(
            MenuKind::Sessions,
            "Sessions · Enter resume · d delete",
            items,
        ));
    }

    fn open_skills(&mut self) {
        let installed = skills::installed();
        let mut items = Vec::new();
        for skill in &installed {
            let enabled =
                self.cfg.enabled_skills.is_empty() || self.cfg.enabled_skills.contains(&skill.name);
            items.push(MenuItem {
                label: format!("{} {}", if enabled { "●" } else { "○" }, skill.name),
                detail: format!("{} · {}", skill.repo, skill.description),
                value: format!("toggle:{}", skill.name),
                enabled: true,
            });
        }
        if installed.is_empty() {
            items.push(MenuItem {
                label: "No skills installed".into(),
                detail: "pick a suggested repo below or use /skills install <url>".into(),
                value: String::new(),
                enabled: false,
            });
        }
        for (slug, url, why) in skills::suggestions() {
            let installed_already = self.cfg.skill_repos.iter().any(|r| r == slug);
            items.push(MenuItem {
                label: format!("Install {slug}"),
                detail: format!("{why} · {url}"),
                value: format!("install:{url}"),
                enabled: !installed_already,
            });
        }
        for repo in &self.cfg.skill_repos {
            items.push(MenuItem {
                label: format!("Remove {repo}"),
                detail: "deletes the local clone".into(),
                value: format!("remove:{repo}"),
                enabled: true,
            });
        }
        self.menu = Some(Menu::new(MenuKind::Skills, "Skills", items));
    }

    fn menu_select(&mut self) {
        let Some(menu) = self.menu.clone() else {
            return;
        };
        let Some(item) = menu.selected_item().cloned() else {
            return;
        };
        self.menu = None;
        match menu.kind {
            MenuKind::Help => {}
            MenuKind::Slash => self.handle_command(&item.value),
            MenuKind::Model => {
                if item.value == "__add_provider" {
                    self.open_provider_menu();
                } else if let Some((provider, model)) = item.value.split_once('\u{1f}') {
                    self.cfg.provider = provider.to_string();
                    self.cfg.model = model.to_string();
                    self.sync_config();
                    self.push(Block::Notice(format!(
                        "Model: {} · {model}",
                        providers::display_name(provider)
                    )));
                    let _ = self
                        .store
                        .touch_session(&self.session_id, None, Some(&self.cfg.model));
                }
            }
            MenuKind::Provider => {
                let provider = item.value.clone();
                let base = if provider == "custom" {
                    Some("http://localhost:8000/v1".to_string())
                } else {
                    None
                };
                self.form = Some(Form::new(
                    format!("{} API key", providers::display_name(&provider)),
                    vec![
                        field("API key", "", true, "stored locally, never logged"),
                        field(
                            "Base URL",
                            base.as_deref().unwrap_or(providers::default_base(&provider)),
                            false,
                            "OpenAI-compatible endpoint root",
                        ),
                    ],
                    FormAction::ProviderKey {
                        provider: provider.clone(),
                        base,
                        label: providers::display_name(&provider).to_string(),
                    },
                ));
                self.form_kind = None;
            }
            MenuKind::Theme => {
                self.cfg.theme = ThemeConfig {
                    name: item.value.clone(),
                    custom: Default::default(),
                };
                self.sync_config();
                self.push(Block::Notice(format!("Theme: {}", item.value)));
            }
            MenuKind::Sessions => {
                if item.value.is_empty() {
                    return;
                }
                self.resume_session(&item.value);
            }
            MenuKind::Skills => {
                if let Some(name) = item.value.strip_prefix("toggle:") {
                    if self.cfg.enabled_skills.is_empty() {
                        self.cfg.enabled_skills = skills::installed()
                            .into_iter()
                            .map(|s| s.name)
                            .filter(|n| n != name)
                            .collect();
                    } else if let Some(pos) = self.cfg.enabled_skills.iter().position(|n| n == name) {
                        self.cfg.enabled_skills.remove(pos);
                    } else {
                        self.cfg.enabled_skills.push(name.to_string());
                    }
                    self.sync_config();
                    self.open_skills();
                } else if let Some(url) = item.value.strip_prefix("install:") {
                    self.handle_command(&format!("/skills install {url}"));
                } else if let Some(repo) = item.value.strip_prefix("remove:") {
                    self.handle_command(&format!("/skills remove {repo}"));
                }
            }
            MenuKind::Settings => match item.value.as_str() {
                "mode:ask" => {
                    self.cfg.permission_mode = Mode::AskEveryTime;
                    self.sync_config();
                    self.push(Block::Notice("Permission mode: Ask every time.".into()));
                }
                "mode:full" => {
                    self.cfg.permission_mode = Mode::FullAccess;
                    self.sync_config();
                    self.push(Block::Notice(
                        "Permission mode: Full access. Commands run without asking. Catastrophic commands remain blocked.".into(),
                    ));
                }
                "key:add" => self.open_provider_menu(),
                "subagents" => self.open_subagents(),
                "connectors" => self.open_connectors(),
                "persona" => self.push(Block::Notice(format!(
                    "Edit {} in any editor. Changes apply to the next message.",
                    personality::path().display()
                ))),
                "storage" => self.push(Block::Notice(format!(
                    "Sessions, messages and tool calls live in {} (SQLite). Use /sessions export for JSON.",
                    crate::paths::db_file().display()
                ))),
                "outside_reads" => {
                    self.cfg.allow_outside_project_reads = !self.cfg.allow_outside_project_reads;
                    self.sync_config();
                    self.open_settings();
                }
                "backend" => self.push(Block::Notice(format!(
                    "Secrets backend: {}. Vault: {} (AES-256-GCM, 0600).",
                    self.secrets.backend().as_str(),
                    crate::paths::vault_file().display()
                ))),
                _ => {}
            },
            MenuKind::Connectors => {
                if item.value == "github" {
                    self.form = Some(Form::new(
                        "GitHub token",
                        vec![field(
                            "Personal access token",
                            "",
                            true,
                            "needs repo read access; used for skill installs",
                        )],
                        FormAction::GithubToken,
                    ));
                }
            }
            MenuKind::Subagents => match item.value.as_str() {
                "__add" => self.open_add_subagent_form(),
                "__remove" => {
                    let items = self
                        .cfg
                        .subagent_keys
                        .iter()
                        .map(|key| MenuItem {
                            label: key.name.clone(),
                            detail: format!("{} · {}", key.provider, key.model),
                            value: format!("delkey:{}", key.name),
                            enabled: true,
                        })
                        .collect();
                    self.menu = Some(Menu::new(MenuKind::Subagents, "Remove subagent key", items));
                }
                value if value.starts_with("delkey:") => {
                    let name = value.trim_start_matches("delkey:").to_string();
                    self.cfg.subagent_keys.retain(|k| k.name != name);
                    self.sync_config();
                    self.push(Block::Notice(format!("Removed subagent key {name}.")));
                }
                _ => {}
            },
        }
    }

    fn open_add_subagent_form(&mut self) {
        let provider = if self.cfg.provider.is_empty() {
            "openai".to_string()
        } else {
            self.cfg.provider.clone()
        };
        let base = self
            .cfg
            .provider(&provider)
            .and_then(|p| p.base_url.clone());
        self.form = Some(Form::new(
            "Add subagent key",
            vec![
                field("Name", "", false, "e.g. reviewer"),
                field("Role", "", false, "e.g. security review"),
                field(
                    "Provider",
                    &provider,
                    false,
                    "openai, anthropic, google, openrouter, ollama, custom",
                ),
                field("API key", "", true, "its own key, stored separately"),
                field("Model", "", false, "e.g. gpt-5"),
            ],
            FormAction::ProviderKey {
                provider,
                base,
                label: "subagent".into(),
            },
        ));
        self.form_kind = Some(FormKind::Subagent);
    }

    fn submit_form(&mut self) {
        let Some(form) = self.form.clone() else {
            return;
        };
        let values: Vec<String> = form
            .fields
            .iter()
            .map(|f| f.value.trim().to_string())
            .collect();
        match &form.action {
            FormAction::ProviderKey {
                provider,
                base,
                label,
            } => {
                if self.form_kind == Some(FormKind::Subagent) {
                    let (name, role, provider, key, model) = (
                        values[0].clone(),
                        values[1].clone(),
                        values[2].clone(),
                        values[3].clone(),
                        values[4].clone(),
                    );
                    if name.is_empty() || key.is_empty() || model.is_empty() {
                        self.form_note("Name, key and model are required.");
                        return;
                    }
                    let key_name = format!("subagent:{name}");
                    match self.secrets.set(&key_name, &key) {
                        Ok(_) => {
                            self.cfg.subagent_keys.retain(|k| k.name != name);
                            self.cfg.subagent_keys.push(crate::config::AgentKey {
                                name: name.clone(),
                                role,
                                provider,
                                key_ref: key_name,
                                model,
                                models: Vec::new(),
                            });
                            self.save_secrets();
                            self.sync_config();
                            self.form = None;
                            self.form_kind = None;
                            self.push(Block::Notice(format!("Added subagent key `{name}`.")));
                        }
                        Err(err) => self.form_note(&format!("Could not store the key: {err}")),
                    }
                    return;
                }

                let key = values.first().cloned().unwrap_or_default();
                let base_url = values.get(1).cloned().filter(|v| !v.is_empty());
                if providers::needs_key(provider) && key.is_empty() {
                    self.form_note("An API key is required for this provider.");
                    return;
                }
                let base_opt = base_url.clone().or_else(|| base.clone());
                self.start_probe(
                    ProbeAction::SaveProviderKey {
                        provider: provider.clone(),
                        base: base_opt.clone(),
                        key_name: format!("provider:{provider}"),
                    },
                    provider.clone(),
                    base_opt,
                    if key.is_empty() { None } else { Some(key) },
                    format!("{label} key"),
                );
            }
            FormAction::GithubToken => {
                let token = values.first().cloned().unwrap_or_default();
                if token.is_empty() {
                    self.form_note("Paste a token, or Esc to cancel.");
                    return;
                }
                self.start_probe(
                    ProbeAction::GithubToken {
                        key_name: "connector:github".into(),
                    },
                    "github".into(),
                    None,
                    Some(token),
                    "GitHub token".into(),
                );
            }
            FormAction::ThemeCustom { slot } => {
                let value = values.first().cloned().unwrap_or_default();
                if crate::theme::parse(&value).is_none() {
                    self.form_note("Use #rrggbb or a named color.");
                    return;
                }
                self.cfg.theme.custom.insert(slot.clone(), value);
                self.sync_config();
                self.form = None;
            }
        }
    }

    fn form_note(&mut self, note: &str) {
        if let Some(form) = &mut self.form {
            form.note = Some(note.to_string());
        }
    }

    fn start_probe(
        &mut self,
        action: ProbeAction,
        provider: String,
        base: Option<String>,
        key: Option<String>,
        label: String,
    ) {
        let key_clone = key.clone();
        let provider_clone = provider.clone();
        let base_clone = base.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(agent::probe(
                &provider_clone,
                base_clone.as_deref(),
                key_clone,
            ));
        });
        self.probe = Some(Probe { rx, action, key });
        self.status_note = format!("checking {label}…");
        self.form = None;
    }

    fn poll_probe(&mut self) {
        let Some(probe) = &self.probe else { return };
        let result = match probe.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.probe = None;
                self.status_note = "check failed".into();
                return;
            }
        };
        let (action, pending_key) = match self.probe.take() {
            Some(probe) => (probe.action, probe.key),
            None => return,
        };
        match (result, action) {
            (
                Ok(models),
                ProbeAction::SaveProviderKey {
                    provider,
                    base,
                    key_name,
                },
            ) => {
                if let Some(key) = pending_key.clone() {
                    if let Err(err) = self.secrets.set(&key_name, &key) {
                        self.push(Block::Error(format!("Could not store the key: {err}")));
                        return;
                    }
                }
                let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
                if ids.is_empty() {
                    self.push(Block::Error("The provider returned no models.".into()));
                    return;
                }
                let free = models.iter().any(|m| m.free)
                    && models.iter().all(|m| m.free)
                    && !provider.is_empty();
                self.cfg.providers.retain(|p| p.id != provider);
                self.cfg.providers.push(crate::config::ProviderConfig {
                    id: provider.clone(),
                    base_url: base.clone(),
                    key_ref: Some(key_name.clone()),
                    models: ids.clone(),
                    free_tier: free,
                });
                if self.cfg.provider.is_empty() || self.cfg.provider == provider {
                    self.cfg.provider = provider.clone();
                    if !ids.contains(&self.cfg.model) {
                        self.cfg.model = ids[0].clone();
                    }
                }
                self.save_secrets();
                self.sync_config();
                self.push(Block::Notice(format!(
                    "{} ready: {} models. Main model: {}",
                    providers::display_name(&provider),
                    ids.len(),
                    self.cfg.model
                )));
                self.open_model_menu();
            }
            (Ok(_), ProbeAction::GithubToken { key_name }) => {
                if let Some(token) = pending_key.clone() {
                    if let Err(err) = self.secrets.set(&key_name, &token) {
                        self.push(Block::Error(format!("Could not store the token: {err}")));
                        return;
                    }
                }
                self.cfg.connectors.retain(|c| c.name != "github");
                self.cfg.connectors.push(crate::config::Connector {
                    name: "github".into(),
                    kind: "token".into(),
                    enabled: true,
                    token_ref: Some(key_name),
                    detail: Some("personal access token".into()),
                });
                self.save_secrets();
                self.sync_config();
                self.push(Block::Notice("GitHub connected.".into()));
            }
            (Err(err), ProbeAction::SaveProviderKey { provider, .. }) => {
                self.push(Block::Error(format!(
                    "{} could not be validated: {err}",
                    providers::display_name(&provider)
                )));
            }
            (Err(err), ProbeAction::GithubToken { .. }) => {
                self.push(Block::Error(format!("GitHub rejected the token: {err}")));
            }
        }
        self.status_note.clear();
    }

    fn answer_permission(&mut self, allow: bool) {
        let Some(modal) = self.permission.take() else {
            return;
        };
        let _ = self.agent.actions.send(UserAction::Permission {
            id: modal.id,
            allow,
        });
        self.push(Block::Notice(if allow {
            "Allowed.".into()
        } else {
            "Denied.".into()
        }));
    }

    pub fn resume_session(&mut self, id: &str) {
        let rows = match self.store.messages(id) {
            Ok(rows) => rows,
            Err(err) => {
                self.push(Block::Error(format!("Could not load session: {err}")));
                return;
            }
        };
        let history = agent::messages_from_rows(&rows);
        let session = self.store.session(id).ok().flatten();
        self.session_id = id.to_string();
        self.session_title = session
            .as_ref()
            .map(|s| s.title.clone())
            .unwrap_or_default();
        if let Some(session) = &session {
            if self.cfg.provider(&session.provider).is_some() {
                self.cfg.provider = session.provider.clone();
                self.cfg.model = session.model.clone();
                self.sync_config();
            }
        }
        self.agent.send(UserAction::LoadHistory(Box::new(history)));
        self.blocks.truncate(1);
        if let Some(session) = &session {
            self.usage_in = session.input_tokens;
            self.usage_out = session.output_tokens;
            let dir = std::path::PathBuf::from(&session.cwd);
            if !session.cwd.is_empty() && dir.is_dir() && dir != self.root {
                self.root = dir.clone();
                self.agent.send(UserAction::SetRoot(dir));
                self.push(Block::Notice(format!(
                    "Working directory: {}",
                    self.root.display()
                )));
            }
        }
        let mut added = 0;
        for row in &rows {
            match row.role.as_str() {
                "user" => {
                    self.push(Block::User(row.content.clone()));
                    added += 1;
                }
                "assistant" if !row.content.trim().is_empty() => {
                    self.push(Block::Assistant(row.content.clone()));
                    added += 1;
                }
                _ => {}
            }
        }
        self.push(Block::Notice(format!(
            "Resumed session ({}).",
            if added == 0 {
                "empty".to_string()
            } else {
                format!("{added} messages")
            }
        )));
    }

    fn delete_selected_session(&mut self) {
        let id = self
            .menu
            .as_ref()
            .and_then(|menu| menu.selected_item().map(|item| item.value.clone()))
            .unwrap_or_default();
        let title = self
            .menu
            .as_ref()
            .and_then(|menu| menu.selected_item().map(|item| item.label.clone()))
            .unwrap_or_default();
        if id.is_empty() {
            return;
        }
        match self.store.delete_session(&id) {
            Ok(_) => {
                self.push(Block::Notice(format!("Deleted session “{title}”.")));
                self.open_sessions();
            }
            Err(err) => self.push(Block::Error(format!("Delete failed: {err}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormKind {
    Subagent,
}

fn intersects(a: Rect, b: Rect) -> bool {
    b.x >= a.x
        && b.x < a.x.saturating_add(a.width)
        && b.y >= a.y
        && b.y < a.y.saturating_add(a.height)
}

fn relative_time(secs: u64) -> String {
    let now = crate::config::now_secs();
    let delta = now.saturating_sub(secs);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86400),
    }
}

fn secrets_open_again(cfg: &Config) -> Secrets {
    Secrets::open(Some(&cfg.secret_backend))
        .unwrap_or_else(|_| Secrets::open(Some("file")).expect("vault must be creatable"))
}

// ---------------------------------------------------------------------- setup

pub fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

pub fn leave_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Run the chat UI until the user exits.
pub fn run(mut app: App) -> io::Result<()> {
    let mut terminal = enter_terminal()?;
    let result = loop {
        if let Err(err) = terminal.draw(|frame| render::draw(frame, &mut app)) {
            break Err(err);
        }
        while let Ok(event) = app.agent.events.try_recv() {
            app.on_agent_event(event);
        }
        app.poll_probe();
        app.spinner = app.spinner.wrapping_add(1);
        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Paste(text) => app.on_paste(text),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    app.agent.stop();
    leave_terminal();
    result
}

/// Build tool-name hints for the empty state.
pub fn tool_hint() -> String {
    tool_names_joined()
}

fn tool_names_joined() -> String {
    tools::specs()
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

impl App {
    pub fn version_line(&self) -> String {
        format!(
            "v{VERSION} · {} · {}",
            providers::display_name(&self.cfg.provider),
            if self.cfg.model.is_empty() {
                "no model"
            } else {
                &self.cfg.model
            }
        )
    }
}
