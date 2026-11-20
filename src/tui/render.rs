//! Drawing. All interactive elements register click rectangles in `App::clicks`
//! every frame, so mouse hit-testing always matches what is on screen.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block as WidgetBlock, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{App, Block, Click, Form, Menu, PermissionModal, Phase, ToolState};
use crate::providers;
use crate::VERSION;

const COMPACT_LOGO: [&str; 3] = [
    "  ██╗  ██╗ █████╗ ███████╗████████╗██╗   ██╗",
    "  ███████║ ███████║███████╗   ██║    ╚████╔╝",
    "  ██╔══██║ ██╔══██║╚════██║   ██║     ╚██╔╝",
];

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.clicks.clear();
    let area = frame.area();
    let theme = app.theme;
    frame.render_widget(
        WidgetBlock::default().style(Style::default().bg(theme.bg)),
        area,
    );

    let input_h = input_height(app, area.width);
    let suggest_h = if app.show_slash && !app.suggestions.is_empty() {
        (app.suggestions.len().min(6) as u16) + 2
    } else {
        0
    };
    let header_h = if area.height >= 30 { 6 } else { 4 };

    let chunks = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(3),
        Constraint::Length(suggest_h),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, app, chunks[0], header_h >= 6);
    draw_transcript(frame, app, chunks[1]);
    if suggest_h > 0 {
        draw_suggestions(frame, app, chunks[2]);
    }
    draw_input(frame, app, chunks[3]);
    draw_status(frame, app, chunks[4]);

    if let Some(menu) = app.menu.clone() {
        draw_menu(frame, app, &menu, area);
    }
    if let Some(form) = app.form.clone() {
        draw_form(frame, app, &form, area);
    }
    if let Some(modal) = app.permission.clone() {
        draw_permission(frame, app, &modal, area);
    }
}

// ------------------------------------------------------------------- header

fn draw_header(frame: &mut Frame, app: &App, area: Rect, full: bool) {
    let theme = app.theme;
    let mut lines: Vec<Line> = Vec::new();
    if full {
        for raw in crate::LOGO.lines().filter(|l| !l.trim().is_empty()) {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme.accent).bg(theme.bg),
            )));
        }
    } else {
        for raw in COMPACT_LOGO {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme.accent).bg(theme.bg),
            )));
        }
    }
    let info = format!(
        "v{VERSION}  ·  {}  ·  {}  ·  {}",
        providers::display_name(&app.cfg.provider),
        if app.cfg.model.is_empty() {
            "no model"
        } else {
            &app.cfg.model
        },
        app.cfg.permission_mode.label()
    );
    let mut spans = vec![Span::styled(
        format!(" {info}"),
        Style::default().fg(theme.muted).bg(theme.bg),
    )];
    if !app.session_title.is_empty() {
        let room = area.width as usize;
        let title: String = app
            .session_title
            .chars()
            .take(room.saturating_sub(info.len() + 12))
            .collect();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            title,
            Style::default().fg(theme.accent_dim).bg(theme.bg),
        ));
    }
    lines.push(Line::from(spans));
    let block = WidgetBlock::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border).bg(theme.bg))
        .style(Style::default().bg(theme.bg));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

// --------------------------------------------------------------- transcript

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    let (lines, hits) = build_lines(app, inner.width as usize);
    let total = lines.len();
    let height = inner.height as usize;
    let max_scroll = total.saturating_sub(height);
    let scroll = app.scroll_from_bottom.min(max_scroll);
    app.scroll_from_bottom = scroll;
    let start = total.saturating_sub(height + scroll);
    let visible: Vec<Line> = lines[start.min(total)..].to_vec();

    frame.render_widget(
        Paragraph::new(Text::from(visible)).style(Style::default().bg(theme.bg)),
        inner,
    );

    for (index, target) in hits {
        if index < start {
            continue;
        }
        let row = inner.y + (index - start) as u16;
        if row < inner.y + inner.height {
            app.clicks
                .push((Rect::new(inner.x, row, inner.width, 1), target));
        }
    }
    app.transcript_area = Some(inner);
    app.clicks.push((inner, Click::Transcript));
}

fn build_lines(app: &App, width: usize) -> (Vec<Line<'static>>, Vec<(usize, Click)>) {
    let theme = app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, Click)> = Vec::new();

    for (index, block) in app.blocks.iter().enumerate() {
        match block {
            Block::Logo => {}
            Block::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " ❯ ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        first_line(text),
                        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
                    ),
                ]));
                for line in wrap_text(text, width.saturating_sub(3)).into_iter().skip(1) {
                    lines.push(Line::from(vec![
                        Span::raw("   "),
                        Span::styled(line, Style::default().fg(theme.user)),
                    ]));
                }
                lines.push(Line::from(""));
            }
            Block::Assistant(text) => {
                for line in wrap_text(text, width.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(line, Style::default().fg(theme.fg)),
                    ]));
                }
                lines.push(Line::from(""));
            }
            Block::Thinking {
                text,
                collapsed,
                secs,
                active,
            } => {
                let arrow = if *collapsed { "▸" } else { "▾" };
                let timing = match secs {
                    Some(secs) => format!(" · {secs:.1}s"),
                    None if *active => " · thinking".to_string(),
                    None => String::new(),
                };
                let summary = if *collapsed {
                    format!(" · {}", thinking_summary(text, width.saturating_sub(28)))
                } else {
                    String::new()
                };
                let color = if *active {
                    theme.accent
                } else {
                    theme.accent_dim
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {arrow} Thinking{timing}"),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(summary, Style::default().fg(theme.muted)),
                ]));
                hits.push((lines.len() - 1, Click::ToggleThinking(index)));
                if !*collapsed {
                    for line in wrap_text(text, width.saturating_sub(4)) {
                        lines.push(Line::from(vec![
                            Span::raw("   "),
                            Span::styled(line, Style::default().fg(theme.muted)),
                        ]));
                    }
                    if text.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            "   (no reasoning stream from this model)",
                            Style::default().fg(theme.accent_dim),
                        )));
                    }
                }
                lines.push(Line::from(""));
            }
            Block::Tool {
                name,
                summary,
                state,
                output,
                diff,
                ms,
                expanded,
            } => {
                let (icon, color) = match state {
                    ToolState::Running => (SPINNER[app.spinner % SPINNER.len()], theme.accent),
                    ToolState::Ok => ("✓", theme.ok),
                    ToolState::Failed => ("✗", theme.err),
                };
                let timing = if *ms > 0 {
                    format!(" · {}ms", ms)
                } else {
                    String::new()
                };
                let head = format!(" {icon} {name} ");
                let available = width.saturating_sub(head.width() + timing.len() + 2);
                lines.push(Line::from(vec![
                    Span::styled(
                        head,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(clip(summary, available), Style::default().fg(theme.fg)),
                    Span::styled(timing, Style::default().fg(theme.accent_dim)),
                ]));
                hits.push((lines.len() - 1, Click::ToggleTool(index)));

                let max_output = if *expanded { 400 } else { 8 };
                let max_diff = if *expanded { 400 } else { 24 };
                if let Some(diff) = diff {
                    for (i, line) in diff.lines().enumerate() {
                        if i >= max_diff {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "    … {} more diff lines (click to expand)",
                                    diff.lines().count() - max_diff
                                ),
                                Style::default().fg(theme.muted),
                            )));
                            break;
                        }
                        let color = match line.chars().next() {
                            Some('+') => theme.diff_add,
                            Some('-') => theme.diff_del,
                            Some('@') => theme.accent_dim,
                            _ => theme.muted,
                        };
                        lines.push(Line::from(Span::styled(
                            format!("    {line}"),
                            Style::default().fg(color),
                        )));
                    }
                }
                if !output.trim().is_empty() {
                    let mut shown = 0;
                    for line in output.lines() {
                        if shown >= max_output {
                            let remaining = output.lines().count() - max_output;
                            lines.push(Line::from(Span::styled(
                                format!("    … {remaining} more lines (click to expand)"),
                                Style::default().fg(theme.accent_dim),
                            )));
                            break;
                        }
                        for wrapped in wrap_text(line, width.saturating_sub(6)) {
                            lines.push(Line::from(vec![
                                Span::raw("    "),
                                Span::styled(wrapped, Style::default().fg(theme.muted)),
                            ]));
                            shown += 1;
                        }
                    }
                }
                lines.push(Line::from(""));
            }
            Block::Notice(text) => {
                for (i, line) in wrap_text(text, width.saturating_sub(4))
                    .into_iter()
                    .enumerate()
                {
                    lines.push(Line::from(vec![
                        Span::styled(
                            if i == 0 { " · " } else { "   " },
                            Style::default().fg(theme.accent_dim),
                        ),
                        Span::styled(line, Style::default().fg(theme.muted)),
                    ]));
                }
            }
            Block::Error(text) => {
                for (i, line) in wrap_text(text, width.saturating_sub(4))
                    .into_iter()
                    .enumerate()
                {
                    lines.push(Line::from(vec![
                        Span::styled(
                            if i == 0 { " ✗ " } else { "   " },
                            Style::default().fg(theme.err),
                        ),
                        Span::styled(line, Style::default().fg(theme.err)),
                    ]));
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Send a task. Hasty plans, uses tools, and verifies before answering.",
            Style::default().fg(theme.muted),
        )));
    }
    (lines, hits)
}

fn thinking_summary(text: &str, width: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "waiting for reasoning…".into();
    }
    clip(&flat, width.max(20))
}

// -------------------------------------------------------------- suggestions

fn draw_suggestions(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.panel))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let start = if app.suggestion_index >= inner.height as usize {
        app.suggestion_index + 1 - inner.height as usize
    } else {
        0
    };
    let mut lines: Vec<Line> = Vec::new();
    for (index, command) in app.suggestions.iter().enumerate().skip(start) {
        if lines.len() >= inner.height as usize {
            break;
        }
        let selected = index == app.suggestion_index;
        let style = if selected {
            Style::default()
                .fg(theme.fg)
                .bg(theme.accent_dim)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg).bg(theme.panel)
        };
        let detail_style = if selected {
            Style::default().fg(theme.fg).bg(theme.accent_dim)
        } else {
            Style::default().fg(theme.muted).bg(theme.panel)
        };
        let label = format!(" {} {}", command.name, command.args);
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<28}"), style),
            Span::styled(command.description, detail_style),
        ]));
        let row = inner.y + (index - start) as u16;
        app.clicks.push((
            Rect::new(inner.x, row, inner.width, 1),
            Click::SlashItem(index),
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// -------------------------------------------------------------------- input

fn input_height(app: &App, width: u16) -> u16 {
    let usable = width.saturating_sub(4).max(10) as usize;
    let lines = wrap_input(&app.input.buffer, usable).len();
    (lines as u16 + 2).clamp(3, 9)
}

fn wrap_input(buffer: &str, width: usize) -> Vec<String> {
    let mut out = wrap_text(buffer, width);
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn draw_input(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.panel))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(
            " message ",
            Style::default().fg(theme.accent_dim).bg(theme.panel),
        ))
        .title_bottom(Span::styled(
            " Enter send · Alt+Enter newline · / commands ",
            Style::default().fg(theme.muted).bg(theme.panel),
        ));
    let inner = block.inner(area);
    if inner.width < 4 || inner.height == 0 {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(block, area);

    let usable = inner.width.saturating_sub(2) as usize;
    let visual = wrap_input(&app.input.buffer, usable);
    let cursor = cursor_position(&app.input.buffer, app.input.cursor, usable);
    let mut lines: Vec<Line> = Vec::new();
    for (index, text) in visual.iter().enumerate() {
        let prefix = if index == 0 { " ❯ " } else { "   " };
        let mut spans = vec![Span::styled(
            prefix.to_string(),
            Style::default().fg(theme.accent).bg(theme.panel),
        )];
        if index == cursor.0 {
            let chars: Vec<char> = text.chars().collect();
            let col = cursor.1.min(chars.len());
            let before: String = chars[..col].iter().collect();
            let mut rest: String = chars[col..].iter().collect();
            let cursor_char = if rest.is_empty() {
                " ".to_string()
            } else {
                rest.chars()
                    .next()
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            };
            if !rest.is_empty() {
                rest = rest.chars().skip(1).collect();
            }
            spans.push(Span::styled(
                before,
                Style::default().fg(theme.fg).bg(theme.panel),
            ));
            spans.push(Span::styled(
                cursor_char,
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                rest,
                Style::default().fg(theme.fg).bg(theme.panel),
            ));
        } else {
            spans.push(Span::styled(
                text.clone(),
                Style::default().fg(theme.fg).bg(theme.panel),
            ));
        }
        lines.push(Line::from(spans));
    }
    if app.input.buffer.is_empty() {
        lines = vec![Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(theme.accent).bg(theme.panel)),
            Span::styled(
                "describe a task…",
                Style::default().fg(theme.muted).bg(theme.panel),
            ),
            Span::styled(" ", Style::default().bg(theme.accent)),
        ])];
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    let row = inner.y + cursor.0 as u16;
    let col = inner.x + 3 + cursor.1 as u16;
    if row < area.y + area.height.saturating_sub(1) {
        frame.set_cursor_position(Position::new(col.min(area.x + area.width - 2), row));
    }
}

/// Map a char cursor offset onto (visual line, column).
fn cursor_position(buffer: &str, cursor: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let mut line = 0usize;
    let mut col = 0usize;
    for (index, ch) in buffer.chars().enumerate() {
        if index == cursor {
            return (line, col);
        }
        if ch == '\n' {
            line += 1;
            col = 0;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > width {
            line += 1;
            col = 0;
        }
        col += w;
    }
    (line, col)
}

// ------------------------------------------------------------------- status

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let (dot, label) = match app.phase {
        Phase::Idle => ("●", "ready".to_string()),
        Phase::Thinking => (SPINNER[app.spinner % SPINNER.len()], "thinking".to_string()),
        Phase::Working => (SPINNER[app.spinner % SPINNER.len()], "working".to_string()),
    };
    let color = match app.phase {
        Phase::Idle => theme.ok,
        _ => theme.accent,
    };
    let mut spans = vec![
        Span::styled(format!(" {dot} "), Style::default().fg(color).bg(theme.bg)),
        Span::styled(
            format!("{label}  "),
            Style::default().fg(theme.muted).bg(theme.bg),
        ),
    ];
    if app.usage_in + app.usage_out > 0 {
        spans.push(Span::styled(
            format!("{}↑ {}↓  ", app.usage_in, app.usage_out),
            Style::default().fg(theme.accent_dim).bg(theme.bg),
        ));
    }
    if !app.subagents.is_empty() {
        let running = app
            .subagents
            .iter()
            .filter(|s| s.status == "running" || s.status == "queued")
            .count();
        if running > 0 {
            spans.push(Span::styled(
                format!("{running} subagent(s)  "),
                Style::default().fg(theme.warn).bg(theme.bg),
            ));
        }
    }
    if !app.status_note.is_empty() {
        spans.push(Span::styled(
            app.status_note.clone(),
            Style::default().fg(theme.warn).bg(theme.bg),
        ));
    }
    spans.push(Span::styled(
        format!("  {} ", app.cfg.permission_mode.label()),
        Style::default().fg(theme.accent_dim).bg(theme.bg),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

// --------------------------------------------------------------------- menu

fn draw_menu(frame: &mut Frame, app: &mut App, menu: &Menu, area: Rect) {
    let theme = app.theme;
    let width = area.width.saturating_sub(8).clamp(30, 96);
    let desired = (menu.items.len() as u16 + 2)
        .min(area.height.saturating_sub(4))
        .max(3);
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(desired)) / 2,
        width,
        desired,
    );
    frame.render_widget(Clear, rect);
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_dim).bg(theme.panel))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(
            format!(" {} ", menu.title),
            Style::default()
                .fg(theme.accent)
                .bg(theme.panel)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " ↑↓ move · Enter select · Esc close ",
            Style::default().fg(theme.muted).bg(theme.panel),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    app.menu_area = Some(rect);

    let list_h = inner.height as usize;
    let scroll = if menu.selected >= list_h {
        menu.selected + 1 - list_h
    } else {
        0
    };
    let mut lines: Vec<Line> = Vec::new();
    for (index, item) in menu.items.iter().enumerate().skip(scroll).take(list_h) {
        let selected = index == menu.selected;
        let base = if selected {
            Style::default()
                .fg(theme.fg)
                .bg(theme.accent_dim)
                .add_modifier(Modifier::BOLD)
        } else if item.enabled {
            Style::default().fg(theme.fg).bg(theme.panel)
        } else {
            Style::default().fg(theme.muted).bg(theme.panel)
        };
        let detail_style = if selected {
            Style::default().fg(theme.fg).bg(theme.accent_dim)
        } else {
            Style::default().fg(theme.muted).bg(theme.panel)
        };
        let marker = if selected { "❯ " } else { "  " };
        let label_width = inner.width.saturating_sub(2) as usize;
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{}", item.label), base),
            Span::styled(
                if item.detail.is_empty() {
                    String::new()
                } else {
                    format!(
                        "  {}",
                        clip(
                            &item.detail,
                            label_width.saturating_sub(item.label.width() + 2).max(8)
                        )
                    )
                },
                detail_style,
            ),
        ]));
        let row = inner.y + (index - scroll) as u16;
        app.clicks.push((
            Rect::new(inner.x, row, inner.width, 1),
            Click::MenuItem(index),
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// --------------------------------------------------------------------- form

fn draw_form(frame: &mut Frame, app: &mut App, form: &Form, area: Rect) {
    let theme = app.theme;
    let width = area.width.saturating_sub(12).clamp(40, 78);
    let height = (form.fields.len() as u16 * 3 + 6).min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_dim).bg(theme.panel))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(
            format!(" {} ", form.title),
            Style::default()
                .fg(theme.accent)
                .bg(theme.panel)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.index;
        let marker = if focused { "❯ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker}{}", field.label),
                Style::default()
                    .fg(if focused { theme.accent } else { theme.muted })
                    .bg(theme.panel)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if field.masked {
                    let masked = if field.value.is_empty() {
                        String::new()
                    } else {
                        "•".repeat(field.value.chars().count().min(32))
                    };
                    format!("  {masked}")
                } else if field.value.is_empty() {
                    format!("  {}", field.hint)
                } else {
                    format!("  {}", field.value)
                },
                Style::default()
                    .fg(if field.value.is_empty() {
                        theme.muted
                    } else {
                        theme.fg
                    })
                    .bg(theme.panel),
            ),
        ]));
        if focused && !field.hint.is_empty() && !field.value.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("     {}", field.hint),
                Style::default().fg(theme.accent_dim).bg(theme.panel),
            )));
        }
        lines.push(Line::from(""));
    }
    if let Some(note) = &form.note {
        lines.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default().fg(theme.err).bg(theme.panel),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![Span::styled(
        "  [ Enter/Tab next · Enter on last field saves · Esc cancel ]",
        Style::default().fg(theme.muted).bg(theme.panel),
    )]));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );

    let next_row = inner.y + inner.height.saturating_sub(1);
    app.clicks.push((
        Rect::new(inner.x, next_row, inner.width, 1),
        Click::FormNext,
    ));
    app.clicks.push((
        Rect::new(rect.x + rect.width.saturating_sub(6), rect.y, 6, 1),
        Click::FormCancel,
    ));
}

// --------------------------------------------------------------- permission

fn draw_permission(frame: &mut Frame, app: &mut App, modal: &PermissionModal, area: Rect) {
    let theme = app.theme;
    let width = area.width.saturating_sub(10).clamp(40, 84);
    let height = 9.min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn).bg(theme.panel))
        .style(Style::default().bg(theme.panel))
        .title(Span::styled(
            " Permission required ",
            Style::default()
                .fg(theme.warn)
                .bg(theme.panel)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    for line in wrap_text(&modal.prompt, inner.width.saturating_sub(2) as usize) {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(line, Style::default().fg(theme.fg).bg(theme.panel)),
        ]));
    }
    lines.push(Line::from(""));
    for line in wrap_text(&modal.detail, inner.width.saturating_sub(2) as usize) {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                line,
                Style::default()
                    .fg(theme.err)
                    .bg(theme.panel)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "[ Allow  y / Enter ]",
            Style::default()
                .fg(theme.bg)
                .bg(theme.ok)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            "[ Deny  n / Esc ]",
            Style::default()
                .fg(theme.bg)
                .bg(theme.err)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    let button_row = inner.y + inner.height.saturating_sub(2);
    app.clicks
        .push((Rect::new(inner.x + 1, button_row, 21, 1), Click::Allow));
    app.clicks
        .push((Rect::new(inner.x + 25, button_row, 19, 1), Click::Deny));
}

// ------------------------------------------------------------------ helpers

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0usize;
        let mut last_space: Option<usize> = None;
        for ch in paragraph.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if line_w + w > width && !line.is_empty() {
                if let Some(pos) = last_space {
                    let tail = line.split_off(pos + 1);
                    out.push(line.trim_end().to_string());
                    line = tail;
                    line_w = line.width();
                    if line_w == 0 {
                        last_space = None;
                    }
                } else {
                    out.push(std::mem::take(&mut line));
                    line_w = 0;
                }
            }
            line.push(ch);
            line_w += w;
            if ch == ' ' {
                last_space = Some(line.len() - 1);
            }
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn clip(text: &str, max: usize) -> String {
    let max = max.max(4);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[..max - 1].iter().collect::<String>() + "…"
}
