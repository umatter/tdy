//! Drawing the workbench frame. Reads [`Workbench`], changes nothing.
//!
//! Layout (design doc §6/§7, task brief): a one-line header, a body, a
//! one-line status; the body splits into the file browser (hidden below 60
//! columns) and a right column of main pane over console. `zoom` makes the
//! console take the whole right column. Every drawing decision here reads
//! `Workbench`'s already-computed state — nothing in this module decides
//! anything, which is what lets `tests/wb_render.rs` assert on drawn text
//! without a terminal.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use tdy::console::EntryStatus;

use crate::workbench::{Context, Focus, Workbench};

const DIM: Color = Color::DarkGray;
/// Below this many columns the file browser has nowhere to go; the console
/// (where typing still works) keeps the space instead.
const MIN_WIDTH_FOR_BROWSER: u16 = 60;
const BROWSER_WIDTH: u16 = 26;

pub fn draw(f: &mut Frame, w: &mut Workbench) {
    let [header, body, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
            .areas(f.area());

    draw_header(f, header, w);
    draw_body(f, body, w);
    draw_status(f, status, w);
}

fn draw_header(f: &mut Frame, area: Rect, w: &Workbench) {
    let line = format!(" tdy — {}", w.browser.title());
    f.render_widget(Paragraph::new(Span::styled(line, Style::new().bold())), area);
}

fn draw_body(f: &mut Frame, area: Rect, w: &mut Workbench) {
    if area.width < MIN_WIDTH_FOR_BROWSER {
        draw_right(f, area, w);
        return;
    }
    let [browser, right] =
        Layout::horizontal([Constraint::Length(BROWSER_WIDTH), Constraint::Fill(1)]).areas(area);
    draw_browser(f, browser, w);
    draw_right(f, right, w);
}

fn draw_right(f: &mut Frame, area: Rect, w: &mut Workbench) {
    if w.zoom {
        draw_console(f, area, w);
        return;
    }
    let [main, console] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(w.console_rows + 2),
    ])
    .areas(area);
    draw_main(f, main, w);
    draw_console(f, console, w);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(DIM)
    };
    Block::bordered().title(title).border_style(style)
}

/// Exactly `render_listing`'s vocabulary (`src/console/mod.rs`) — the
/// browser row and the text console must never disagree about what a file
/// is.
fn entry_status_text(status: &EntryStatus) -> String {
    match status {
        EntryStatus::None => String::new(),
        EntryStatus::Sniffed { confidence: Some(c), method } => format!("sniffed {c:.2} ({method})"),
        EntryStatus::Sniffed { confidence: None, method } => format!("sniffed ({method})"),
        EntryStatus::Stale => "stale".into(),
        EntryStatus::NoLock => "target, no lock".into(),
        EntryStatus::Locked => "target, locked".into(),
        EntryStatus::Drift(n) => format!("target, drift ({n})"),
    }
}

fn draw_browser(f: &mut Frame, area: Rect, w: &Workbench) {
    let focused = w.focus == Focus::Browser;
    let block = pane_block(" files ".to_string(), focused);
    // The "▸ " highlight symbol reserves its own two columns on every row,
    // selected or not (ratatui's `List` shifts all row content right by
    // its width) — the text layout has to account for that or the longest
    // status strings (`target, no lock`) get clipped at the pane edge.
    let inner_width = block.inner(area).width.saturating_sub(2) as usize;

    if let Some(err) = &w.browser.error {
        f.render_widget(Paragraph::new(err.as_str()).block(block), area);
        return;
    }

    let items: Vec<ListItem> = w
        .browser
        .entries
        .iter()
        .map(|e| ListItem::new(browser_row(&e.name, entry_status_text(&e.status), inner_width)))
        .collect();

    let mut state = ListState::default().with_selected(Some(w.browser.selected));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(&list, area, &mut state);
}

/// Name left, status right, within `width` columns (the pane's inner
/// width) — `render_listing`'s two columns, laid out for a fixed-width
/// pane instead of a name-aligned block of text.
fn browser_row(name: &str, status: String, width: usize) -> Line<'static> {
    if status.is_empty() {
        return Line::raw(name.to_string());
    }
    let status_w = status.chars().count();
    let avail_for_name = width.saturating_sub(status_w + 1).max(1);
    let name = truncate(name, avail_for_name);
    let pad = width.saturating_sub(name.chars().count() + status_w).max(1);
    Line::raw(format!("{name}{}{status}", " ".repeat(pad)))
}

fn context_title(ctx: &Context) -> String {
    match ctx {
        Context::Empty => "main".to_string(),
        Context::File { path, .. } => path.display().to_string(),
        Context::Query(_) => "query".to_string(),
    }
}

fn draw_main(f: &mut Frame, area: Rect, w: &Workbench) {
    let focused = w.focus == Focus::Main;
    let title = format!(" {} ", context_title(&w.context));
    let block = pane_block(title, focused);

    match &w.context {
        Context::Empty => {
            let lines = vec![
                Line::raw("select a file on the left, or type `.help`"),
                Line::raw(w.browser.root().display().to_string()),
                Line::raw("`tdy ui <target>` opens the classic review flow"),
            ];
            f.render_widget(Paragraph::new(lines).block(block), area);
        }
        // The full two-column "what tdy sees / what tdy makes of it" view
        // is Task 4's; this task renders the raw lines only, so the
        // context switches without a blank pane while that view lands.
        Context::File { raw, .. } => {
            let lines: Vec<Line> = if raw.lines.is_empty() {
                vec![Line::styled("reading…", Style::new().fg(DIM))]
            } else {
                raw.lines.iter().map(|l| Line::raw(l.clone())).collect()
            };
            f.render_widget(Paragraph::new(lines).block(block), area);
        }
        Context::Query(t) => {
            let lines: Vec<Line> = t
                .rows
                .iter()
                .map(|r| Line::raw(r.join("  ")))
                .collect();
            f.render_widget(Paragraph::new(lines).block(block), area);
        }
    }
}

fn draw_console(f: &mut Frame, area: Rect, w: &Workbench) {
    let focused = w.focus == Focus::Console;
    let block = pane_block(" console ".to_string(), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for cell in &w.scrollback {
        lines.push(Line::styled(format!("tdy> {}", cell.echo), Style::new().fg(DIM)));
        for l in cell.text.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }

    let content_rows = inner.height.saturating_sub(1) as usize;
    let total = lines.len();
    let end = total.saturating_sub(w.scroll);
    let start = end.saturating_sub(content_rows);
    let mut visible: Vec<Line> = lines[start..end].to_vec();

    let input = format!("{}{}", w.prompt(), w.editor.text());
    let input_row = visible.len() as u16;
    visible.push(Line::raw(input));

    f.render_widget(Paragraph::new(visible), inner);

    if focused {
        let col = inner.x + (w.prompt().chars().count() + w.editor.cursor()) as u16;
        let row = inner.y + input_row;
        f.set_cursor_position((col, row));
    }
}

fn draw_status(f: &mut Frame, area: Rect, w: &Workbench) {
    let (text, style) = match &w.busy {
        Some(what) => (format!(" {what}…"), Style::new().fg(Color::Yellow)),
        None => (format!(" {}", w.status), Style::new().fg(DIM)),
    };
    let keys = match w.focus {
        Focus::Console => "Tab focus · ^L zoom · ^Q quit",
        Focus::Browser => "↑↓ move · enter open · s sniff · e edit · Tab focus · ^Q quit",
        Focus::Main => "↑↓ scroll · Tab focus · ^Q quit",
    };
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(keys.len() as u16 + 2)])
            .areas(area);
    f.render_widget(Paragraph::new(Span::styled(text, style)), left);
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::new().fg(DIM))).alignment(Alignment::Right),
        right,
    );
}

fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        s.to_string()
    } else {
        s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }
}
