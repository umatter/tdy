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

use tdy::console::{EntryStatus, RawHead, SpecSummary, Table};

use crate::mark;
use crate::workbench::{Context, Focus, Workbench};

const DIM: Color = Color::DarkGray;
/// Below this many columns the file browser has nowhere to go; the console
/// (where typing still works) keeps the space instead.
const MIN_WIDTH_FOR_BROWSER: u16 = 60;
const BROWSER_WIDTH: u16 = 26;
/// Below this confidence the File view colors the number red. `tdy::config`
/// exports no threshold of its own — the engine escalates to the model
/// below this same number (see `infer.rs`), so it is mirrored here rather
/// than invented.
const ESCALATION: f32 = 0.8;
/// Below this many inner rows the Empty view drops the mark rather than
/// squeeze it against the orientation text beneath it.
const MARK_MIN_HEIGHT: u16 = 10;

/// The current key vocabulary, key then meaning, in one slice so later
/// tasks (d/D/f/a/t) append a row here rather than hunting across the
/// module for every place a key is explained. `draw_help` renders it.
const HELP_KEYS: &[(&str, &str)] = &[
    ("Tab", "cycle focus"),
    ("Esc", "focus the console"),
    ("^Q", "quit"),
    ("^L", "zoom the console"),
    ("^Up / ^Down", "resize the console"),
    ("↑ / ↓", "move selection / scroll"),
    ("Enter", "open file or directory"),
    ("Backspace", "go up a directory"),
    ("s", "sniff the selected file"),
    ("e", "edit the selected file"),
    ("?", "show this help"),
];

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
    // The overlay replaces the main pane's own drawing rather than layering
    // on top of it — ratatui has no z-order, and "over the main pane area"
    // just means "instead of it, for as long as help is up."
    if w.help {
        draw_help(f, main);
    } else {
        draw_main(f, main, w);
    }
    draw_console(f, console, w);
}

/// The `?` overlay: a bordered ` keys ` block over the main pane, the mark
/// beside the key vocabulary. `Workbench::key` owns when this is shown and
/// how it closes; this only draws it.
fn draw_help(f: &mut Frame, area: Rect) {
    let block = Block::bordered()
        .title(" keys ")
        .border_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [mark_area, keys_area] =
        Layout::horizontal([Constraint::Length(mark::WIDTH as u16 + 2), Constraint::Fill(1)])
            .areas(inner);
    f.render_widget(Paragraph::new(mark_lines()), mark_area);

    let key_w = HELP_KEYS.iter().map(|&(k, _)| k.chars().count()).max().unwrap_or(0);
    let lines: Vec<Line<'static>> = HELP_KEYS
        .iter()
        .map(|&(k, desc)| Line::raw(format!("{k:key_w$}  {desc}")))
        .collect();
    f.render_widget(Paragraph::new(lines), keys_area);
}

/// The generated mark (`mark::GRID`) as 8 terminal rows of half-block
/// glyphs: each text row packs two pixel rows into one `▀`/`▄` cell, using
/// both the foreground (upper pixel) and background (lower pixel) colors —
/// the same trick `assets/gen_logo.py`'s `ansi()` uses for `logo.ansi`,
/// through ratatui's `Style` instead of raw escapes.
fn mark_lines() -> Vec<Line<'static>> {
    (0..mark::HEIGHT / 2)
        .map(|r| {
            let spans: Vec<Span<'static>> = (0..mark::WIDTH)
                .map(|c| {
                    let upper = mark::GRID[2 * r][c];
                    let lower = mark::GRID[2 * r + 1][c];
                    match (upper, lower) {
                        (Some(u), Some(l)) => Span::styled("▀", Style::new().fg(rgb(u)).bg(rgb(l))),
                        (Some(u), None) => Span::styled("▀", Style::new().fg(rgb(u))),
                        (None, Some(l)) => Span::styled("▄", Style::new().fg(rgb(l))),
                        (None, None) => Span::raw(" "),
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(DIM)
    };
    Block::bordered().title(title).border_style(style)
}

/// The browser's own compact vocabulary (design doc §6's mock: `✓ 0.95`,
/// `✗ stale`, `locked` / `drift`) — not `render_listing`'s long-form text.
/// The 26-column pane cannot carry `sniffed 0.95 (heuristic)` (24 chars
/// against ~22 usable columns after borders and the highlight-symbol
/// reservation) without clipping the far more common case; the method stays
/// out of the browser entirely, since the File view (Task 4) is where it
/// belongs.
fn entry_status_text(status: &EntryStatus) -> String {
    match status {
        EntryStatus::None => String::new(),
        EntryStatus::Sniffed { confidence: Some(c), .. } => format!("✓ {c:.2}"),
        EntryStatus::Sniffed { confidence: None, .. } => "✓".into(),
        EntryStatus::Stale => "✗ stale".into(),
        EntryStatus::NoLock => "no lock".into(),
        EntryStatus::Locked => "locked".into(),
        EntryStatus::Drift(n) => format!("drift ({n})"),
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
/// width). The status is the fact that matters — whether a file will fit,
/// whether it needs attention — so it never gives way; the name is
/// ellipsized to whatever room is left. With the compact vocabulary above,
/// the longest form is `drift (99)` at 10 chars, well inside a 26-column
/// pane's ~22 usable columns, so this only ever bites the name.
fn browser_row(name: &str, status: String, width: usize) -> Line<'static> {
    if status.is_empty() {
        return Line::raw(truncate(name, width));
    }
    let status_w = status.chars().count();
    let sep = if width > status_w { 1 } else { 0 };
    let avail_for_name = width.saturating_sub(status_w + sep);
    let name = truncate(name, avail_for_name);
    let used = name.chars().count() + sep + status_w;
    let pad = width.saturating_sub(used);
    Line::raw(format!("{name}{}{status}", " ".repeat(pad)))
}

fn context_title(ctx: &Context) -> String {
    match ctx {
        Context::Empty => "main".to_string(),
        Context::File { path, .. } => path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        Context::Query(_) => "result".to_string(),
    }
}

fn draw_main(f: &mut Frame, area: Rect, w: &Workbench) {
    let focused = w.focus == Focus::Main;
    let title = format!(" {} ", context_title(&w.context));
    let block = pane_block(title, focused);

    match &w.context {
        Context::Empty => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            let mut lines = Vec::new();
            // Below MARK_MIN_HEIGHT there is nowhere to put 8 rows of mark
            // plus a blank plus 3 lines of text without crowding all of
            // it — drop the mark rather than draw an illegible sliver.
            if inner.height >= MARK_MIN_HEIGHT {
                lines.extend(mark_lines());
                lines.push(Line::raw(""));
            }
            lines.push(Line::raw("select a file on the left, or type `.help`"));
            lines.push(Line::raw(w.browser.root().display().to_string()));
            lines.push(Line::raw("`tdy ui <target>` opens the classic review flow"));
            f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        }
        Context::File { raw, spec, preview, .. } => match spec {
            // No sidecar yet: the raw head as-is, and nothing that looks
            // like an opinion — no columns, no types, no arrows.
            None => draw_file_no_spec(f, area, block, raw, w.main_scroll),
            // A sidecar exists: raw beside the spec's own decisions.
            Some(spec) => {
                draw_file_with_spec(f, area, block, raw, spec, preview.as_ref(), w.main_scroll)
            }
        },
        Context::Query(t) => {
            f.render_widget(Paragraph::new(table_lines(t)).block(block), area);
        }
    }
}

/// The raw head, verbatim: file lines, or (for a workbook) one
/// `sheet "Name": R row(s) x C col(s)` line per sheet, then the file's own
/// lines if any were also sampled. A trailing `…` marks a truncated read.
fn raw_head_lines(raw: &RawHead) -> Vec<Line<'static>> {
    if raw.lines.is_empty() && raw.sheets.is_empty() {
        return vec![Line::styled("reading…", Style::new().fg(DIM))];
    }
    let mut lines = Vec::new();
    for (name, rows, cols) in &raw.sheets {
        lines.push(Line::raw(format!("sheet \"{name}\": {rows} row(s) x {cols} col(s)")));
    }
    for l in &raw.lines {
        lines.push(Line::raw(l.clone()));
    }
    if raw.truncated {
        lines.push(Line::styled("…", Style::new().fg(DIM)));
    }
    lines
}

/// No sidecar: raw head only, plus a footer naming the fact that it is
/// unopinionated — never a column name, a type, or an arrow.
fn draw_file_no_spec(f: &mut Frame, area: Rect, block: Block<'static>, raw: &RawHead, scroll: usize) {
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [content, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(
        Paragraph::new(raw_head_lines(raw)).scroll((scroll as u16, 0)),
        content,
    );
    f.render_widget(
        Paragraph::new(Line::styled("not sniffed — press s", Style::new().fg(DIM))),
        footer,
    );
}

/// A sidecar exists: raw beside the spec's decisions, two even columns,
/// with the preview table (when there is one) spanning the bottom.
fn draw_file_with_spec(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    raw: &RawHead,
    spec: &SpecSummary,
    preview: Option<&Table>,
    scroll: usize,
) {
    let inner = block.inner(area);
    f.render_widget(block, area);

    // The spec summary (method, confidence, columns, decisions) is this
    // view's primary content; the preview strip is secondary and must
    // never take rows from it. Reserve a floor for the summary first, then
    // size the strip from what's left — and skip the strip entirely rather
    // than draw a sliver too short to read, so a too-short pane degrades to
    // "summary only," never "summary squeezed to nothing."
    const TOP_MIN: u16 = 4;
    let (top, bottom) = match preview {
        Some(t) if inner.height > TOP_MIN => {
            let want = t.rows.len() as u16 + 2;
            let h = want.min(inner.height - TOP_MIN);
            if h >= 2 {
                let [top, bottom] =
                    Layout::vertical([Constraint::Fill(1), Constraint::Length(h)]).areas(inner);
                (top, Some(bottom))
            } else {
                (inner, None)
            }
        }
        _ => (inner, None),
    };

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    f.render_widget(
        Paragraph::new(raw_head_lines(raw)).scroll((scroll as u16, 0)),
        left,
    );
    f.render_widget(Paragraph::new(spec_lines(spec)), right);

    if let (Some(bottom), Some(t)) = (bottom, preview) {
        f.render_widget(Paragraph::new(table_lines(t)), bottom);
    }
}

/// The spec summary: method, confidence (red below `ESCALATION`), each
/// column as `name ← "source" : TYPE`, then the notes as a decisions list.
fn spec_lines(spec: &SpecSummary) -> Vec<Line<'static>> {
    let mut lines = vec![Line::raw(format!("method: {}", spec.method))];
    lines.push(match spec.confidence {
        Some(c) => {
            let style = if c < ESCALATION { Style::new().fg(Color::Red) } else { Style::new() };
            Line::styled(format!("confidence: {c:.2}"), style)
        }
        None => Line::raw("confidence: —".to_string()),
    });
    lines.push(Line::raw(""));
    for (name, source, ty) in &spec.columns {
        lines.push(Line::raw(format!("{name} ← \"{source}\" : {ty}")));
    }
    if !spec.notes.is_empty() {
        lines.push(Line::raw(""));
        for note in &spec.notes {
            lines.push(Line::raw(format!("• {note}")));
        }
    }
    lines
}

/// A `Table`, rendered as a header row, its data rows, then a count line —
/// shared by the File view's preview and a bare query's result.
fn table_lines(t: &Table) -> Vec<Line<'static>> {
    let mut lines =
        vec![Line::styled(t.columns.join("  "), Style::new().add_modifier(Modifier::BOLD))];
    for r in &t.rows {
        lines.push(Line::raw(r.join("  ")));
    }
    let mut count = format!("{} row(s)", t.total);
    if t.truncated {
        count.push_str(" (truncated)");
    }
    lines.push(Line::styled(count, Style::new().fg(DIM)));
    lines
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
        Some(what) => (format!(" {what}"), Style::new().fg(Color::Yellow)),
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
