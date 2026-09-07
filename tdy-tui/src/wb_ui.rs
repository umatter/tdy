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
use ratatui::text::{Line, Span, Text};
use ratatui::symbols::border;
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table as TableWidget, Wrap,
};
use ratatui::Frame;

use tdy::console::{EntryStatus, RawHead, SpecSummary, Table};
use tdy::report::{MemberReport, MemberStatus, PileReport};

use crate::mark;
use crate::remedy::{Edit, Remedy};
use crate::workbench::{needs_attention, Context, Focus, PileFilter, Workbench};

const DIM: Color = Color::DarkGray;
/// The palette, by meaning rather than by screen: what fits is green, a
/// gap is red, a judgement waiting on a person is yellow, and a state that
/// is not yet the real one (no lock, dry run) is yellow too. Used the same
/// way in the pile, the member view and the browser, so one colour means
/// one thing everywhere.
const OK: Color = Color::Green;
const BAD: Color = Color::Red;
const WARN: Color = Color::Yellow;
/// The selected row in any pane: reversed, as the browser's `List` already
/// draws its selection — one grammar for "this is the one you are on".
const SELECTED: Modifier = Modifier::REVERSED;
/// A raw-head cell that a remedy could bind (`--propose` said its values
/// produce the declared type) is green; one the problem itself implicates
/// (the two `Betrag`s of an ambiguous binding, the column whose values are
/// the declared names) is yellow.
const CANDIDATE: Color = OK;
const IMPLICATED: Color = WARN;
/// Below this many columns the file browser has nowhere to go; the console
/// (where typing still works) keeps the space instead.
const MIN_WIDTH_FOR_BROWSER: u16 = 60;
const BROWSER_WIDTH: u16 = 26;
/// Below this many inner rows the Empty view drops the mark rather than
/// squeeze it against the orientation text beneath it: mark = 9 rows incl.
/// spacing + 3 orientation lines; at 10 the Paragraph clipped the tail.
const MARK_MIN_HEIGHT: u16 = 13;

/// Where a key applies: everywhere, in one focused pane, or in one main
/// pane context. The help popup leads with the scope the user is in.
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Everywhere,
    Browser,
    Main,
    File,
    Pile,
    Member,
    Evidence,
    Confirm,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::Everywhere => "everywhere",
            Scope::Browser => "in the file browser",
            Scope::Main => "in the main pane",
            Scope::File => "on a file",
            Scope::Pile => "on a pile",
            Scope::Member => "on a member",
            Scope::Evidence => "on the evidence",
            Scope::Confirm => "confirming an edit",
        }
    }
}

/// The current key vocabulary — scope, key, meaning — in one slice so a
/// later task appends a row here rather than hunting across the module for
/// every place a key is explained. `draw_help` renders it, this context's
/// scope first.
const HELP_KEYS: &[(Scope, &str, &str)] = &[
    (Scope::Everywhere, "Tab", "cycle focus"),
    (Scope::Everywhere, "Esc", "focus the console"),
    (Scope::Everywhere, "^Q", "quit"),
    (Scope::Everywhere, "^L", "zoom the console"),
    (Scope::Everywhere, "^Up / ^Down", "resize the console"),
    (Scope::Everywhere, "?", "show this help"),
    (Scope::Browser, "↑ / ↓", "move the selection (previews the file)"),
    (Scope::Browser, "Enter", "open file or directory"),
    (Scope::Browser, "Backspace", "go up a directory"),
    (Scope::Browser, "s", "sniff the selected file"),
    (Scope::Browser, "e", "edit the selected file"),
    (Scope::Browser, "f", "fit the selected target"),
    (Scope::Browser, "d", "mark/unmark the selected file"),
    (Scope::Browser, "D", "draft the marked files"),
    (Scope::Main, "↑ / ↓, PgUp / PgDn", "scroll"),
    (Scope::File, "[ / ]", "previous / next sheet of a workbook"),
    (Scope::File, "s", "sniff this file"),
    (Scope::Pile, "↑ / ↓", "move the selected member"),
    (Scope::Pile, "Enter", "open the selected member"),
    (Scope::Pile, "g / G", "next / previous member that needs attention"),
    (Scope::Pile, "/", "show problems only / show all"),
    (Scope::Pile, "f", "re-fit the pile (for real)"),
    (Scope::Pile, "t", "edit the target"),
    (Scope::Pile, "Esc", "close the pile"),
    (Scope::Member, "↑ / ↓", "pick a remedy"),
    (Scope::Member, "1-9, Enter", "stage a remedy (shows the diff first)"),
    (Scope::Member, "a", "accept — show the evidence, then accept"),
    (Scope::Member, "e", "edit the file"),
    (Scope::Member, "t", "edit the target"),
    (Scope::Member, "[ / ]", "previous / next sheet of a workbook"),
    (Scope::Member, "Esc", "back to the pile"),
    (Scope::Evidence, "a", "accept"),
    (Scope::Evidence, "Esc", "close (f re-opens the pile)"),
    (Scope::Confirm, "y", "write the edit"),
    (Scope::Confirm, "Esc / n", "cancel"),
];

/// The scope the user is in: the confirm overlay if one is up, else the
/// focused pane, with the main pane refined by its context.
fn current_scope(w: &Workbench) -> Scope {
    if w.pending_edit.is_some() {
        return Scope::Confirm;
    }
    match w.focus {
        Focus::Console => Scope::Everywhere,
        Focus::Browser => Scope::Browser,
        Focus::Main => match &w.context {
            Context::File { .. } => Scope::File,
            Context::Pile { .. } => Scope::Pile,
            Context::Member { .. } => Scope::Member,
            Context::Evidence { .. } => Scope::Evidence,
            Context::Empty | Context::Query(_) => Scope::Main,
        },
    }
}

/// Spinner frames for the status line while a command runs.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn draw(f: &mut Frame, w: &mut Workbench) {
    let [header, body, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
            .areas(f.area());

    draw_header(f, header, w);
    draw_body(f, body, w);
    draw_status(f, status, w);
}

/// How many rows of content the main pane can show at `height` terminal
/// rows — the same arithmetic `draw`/`draw_right` perform with Layout:
/// 1 header row + 1 status row around the body, `console_rows + 2` for the
/// console pane, 1 for the main block's own top border (its bottom edge is
/// the console's top border — the two share the line). 0 when the console
/// is zoomed (no main pane on screen) — `set_main_view_rows` ignores 0.
pub fn main_inner_rows(height: u16, w: &Workbench) -> usize {
    if w.zoom {
        return 0;
    }
    let body = height.saturating_sub(2);
    let main = body.saturating_sub(w.console_rows + 2);
    main.saturating_sub(1) as usize
}

/// The header names what changes what a key does: where you are (root and
/// the directory within it), the target on screen with its lock state, and
/// the backend a refused file would be put to — and a DRY RUN badge at the
/// right edge when the pile on screen is one, because "this is what would
/// happen" and "this is what happened" must never blur.
fn draw_header(f: &mut Frame, area: Rect, w: &Workbench) {
    let dim = Style::new().fg(DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);

    // The facts that change what a key does are fixed; the root gives way,
    // from its left, since its tail is what tells two directories apart.
    let mut fixed: Vec<Span<'static>> = Vec::new();
    if let Some(target) = header_target(w) {
        fixed.push(Span::styled("  ·  ", dim));
        fixed.push(Span::styled(target.name, bold));
        if let Some((state, style)) = target.state {
            fixed.push(Span::raw(" "));
            fixed.push(Span::styled(state, style));
        }
    }
    fixed.push(Span::styled("  ·  backend ", dim));
    fixed.push(Span::styled(
        w.backend.clone(),
        if w.backend == "none" { dim } else { Style::new().fg(WARN) },
    ));

    let badge = match &w.context {
        Context::Pile { report, .. } | Context::Member { report, .. } if report.dry_run => {
            Some(Span::styled(" DRY RUN ", Style::new().fg(Color::Black).bg(WARN).add_modifier(Modifier::BOLD)))
        }
        _ => None,
    };
    let badge_w = badge.as_ref().map(|b| b.content.chars().count() as u16).unwrap_or(0);
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(badge_w)]).areas(area);

    let mut root = w.browser.root().display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = root.strip_prefix(&home) {
            root = format!("~{rest}");
        }
    }
    if w.browser.title() != "." {
        root.push('/');
        root.push_str(&w.browser.title());
    }
    let prefix = " tdy ";
    let fixed_w: usize = fixed.iter().map(|sp| sp.content.chars().count()).sum();
    let room = (left.width as usize).saturating_sub(prefix.chars().count() + fixed_w);
    let root_n = root.chars().count();
    if root_n > room {
        let keep = room.saturating_sub(1);
        root = format!("…{}", root.chars().skip(root_n - keep).collect::<String>());
    }

    let mut spans = vec![Span::styled(prefix, bold), Span::raw(root)];
    spans.extend(fixed);
    f.render_widget(Paragraph::new(clip_line(Line::from(spans), left.width as usize)), left);
    if let Some(b) = badge {
        f.render_widget(Paragraph::new(Line::from(b)), right);
    }
}

struct HeaderTarget {
    name: String,
    state: Option<(String, Style)>,
}

/// The target the header names: the one the main pane is showing, else the
/// one last fitted. Its lock state comes from the browser's listing when the
/// target is in the current directory (the same `EntryStatus` the browser
/// column shows), so the two never disagree.
fn header_target(w: &Workbench) -> Option<HeaderTarget> {
    let path = match &w.context {
        Context::Pile { target, .. } | Context::Member { target, .. } | Context::Evidence { target, .. } => {
            Some(target.clone())
        }
        _ => w.last_target.clone(),
    }?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
    let state = w
        .browser
        .entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| match &e.status {
            EntryStatus::NoLock => Some(("no lock".to_string(), Style::new().fg(WARN))),
            EntryStatus::Locked => Some(("locked".to_string(), Style::new().fg(OK))),
            EntryStatus::Drift(n) => Some((format!("drift ({n})"), Style::new().fg(WARN))),
            _ => None,
        });
    Some(HeaderTarget { name, state })
}

fn draw_body(f: &mut Frame, area: Rect, w: &mut Workbench) {
    if area.width < MIN_WIDTH_FOR_BROWSER {
        draw_right(f, area, w, Seams { browser: false });
        return;
    }
    let [browser, right] =
        Layout::horizontal([Constraint::Length(BROWSER_WIDTH), Constraint::Fill(1)]).areas(area);
    draw_browser(f, browser, w);
    draw_right(f, right, w, Seams { browser: true });
}

fn draw_right(f: &mut Frame, area: Rect, w: &mut Workbench, seams: Seams) {
    // Checked first, ahead of `help` and `zoom`: a staged edit is modal (see
    // `Workbench::key`), and the overlay confirming it must cover the whole
    // right column exactly as the help overlay does, for the same reason —
    // it can be open regardless of whether the console is zoomed.
    if let Some((remedy, edit, ..)) = &w.pending_edit {
        draw_under_popup(f, area, w, seams);
        draw_confirm(f, area, remedy, edit);
        return;
    }
    // Checked before `zoom`: `?` opens help from Browser/Main focus
    // regardless of whether the console is currently zoomed (Tab still
    // moves focus off the console while zoomed), so the overlay must cover
    // the whole right column here rather than a `main` sub-area that may
    // not have been computed — the zoom branch below never runs one.
    if w.help {
        draw_under_popup(f, area, w, seams);
        draw_help(f, area, w);
        return;
    }
    draw_under_popup(f, area, w, seams);
}

/// What lies under a popup: the ordinary right column, drawn first so the
/// popup floats over it rather than replacing it.
fn draw_under_popup(f: &mut Frame, area: Rect, w: &Workbench, seams: Seams) {
    if w.zoom {
        draw_console(f, area, w, seams, false);
        return;
    }
    let [main, console] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(w.console_rows + 2),
    ])
    .areas(area);
    draw_main(f, main, w, seams);
    draw_console(f, console, w, seams, true);
}

/// A rect of up to `want_w` x `want_h` centred in `area`, inset by at least
/// one cell on each side so the pane's own border stays visible around it.
fn popup_rect(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let w = want_w.min(area.width.saturating_sub(2)).max(1);
    let h = want_h.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn popup_block(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
        .border_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

/// The `?` popup: the mark beside the key vocabulary, this context's keys
/// first, then the ones that work everywhere. `Workbench::key` owns when
/// this is shown and how it closes; this only draws it.
fn draw_help(f: &mut Frame, area: Rect, w: &Workbench) {
    let here = current_scope(w);
    let key_w = HELP_KEYS.iter().map(|&(_, k, _)| k.chars().count()).max().unwrap_or(0);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let section = |lines: &mut Vec<Line<'static>>, scope: Scope| {
        let rows: Vec<&(Scope, &str, &str)> = HELP_KEYS.iter().filter(|(s, ..)| *s == scope).collect();
        if rows.is_empty() {
            return;
        }
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(scope.label().to_string(), Style::new().fg(DIM).add_modifier(Modifier::BOLD)));
        for &(_, k, desc) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("{k:key_w$}"), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {desc}")),
            ]));
        }
    };
    if here != Scope::Everywhere {
        section(&mut lines, here);
    }
    section(&mut lines, Scope::Everywhere);

    let text_w = lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16;
    let mark_w = mark::WIDTH as u16 + 2;
    let rect = popup_rect(area, mark_w + text_w + 4, lines.len() as u16 + 2);
    let block = popup_block("keys");
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let [mark_area, keys_area] =
        Layout::horizontal([Constraint::Length(mark_w), Constraint::Fill(1)]).areas(inner);
    f.render_widget(Paragraph::new(mark_lines()), mark_area);
    f.render_widget(Paragraph::new(lines), keys_area);
}

/// The staged-edit confirm popup: the remedy's label, then `Edit::diff()`'s
/// lines with a dim line-number gutter, `-` lines red and `+` lines green,
/// then a footer naming the two keys `Workbench::key`'s modal branch
/// actually honours. `Workbench` owns when this is shown and what `y`/`Esc`
/// do; this only draws it.
fn draw_confirm(f: &mut Frame, area: Rect, remedy: &Remedy, edit: &Edit) {
    let mut lines = vec![
        Line::styled(remedy.label(), Style::new().add_modifier(Modifier::BOLD)),
        Line::raw(String::new()),
    ];
    for l in edit.diff().lines() {
        // The diff line is `"{:>4} - {before}"` or `"{:>4} + {after}"` (see
        // `Edit::diff`): a four-wide number, a space, the marker, a space,
        // the text — fixed positions, so parse by position rather than by
        // splitting on characters the text itself may contain.
        let n = l.get(..4).unwrap_or("").trim();
        let marker = l.get(5..6).unwrap_or("");
        let text = l.get(7..).unwrap_or("");
        let style = match marker {
            "-" => Style::new().fg(BAD),
            "+" => Style::new().fg(OK),
            _ => Style::new(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{n:>4} "), Style::new().fg(DIM)),
            Span::styled(format!("{marker} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(text.to_string(), style),
        ]));
    }
    lines.push(Line::raw(String::new()));
    lines.push(Line::styled("y writes the target · Esc cancels", Style::new().fg(DIM)));

    let text_w = lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16;
    let rect = popup_rect(area, text_w + 4, lines.len() as u16 + 2);
    let block = popup_block("confirm edit");
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
    Block::bordered().border_type(BorderType::Rounded).title(title).border_style(style)
}

/// Which edges of the right column are shared with a neighbour, so the
/// blocks there draw junctions rather than corners: the browser's seam on
/// the left (the browser draws no right border of its own), and the
/// main/console boundary, which is the console's top border alone.
#[derive(Clone, Copy)]
struct Seams {
    /// A browser pane sits to the left.
    browser: bool,
}

/// The rounded set with the left corners turned into junctions where the
/// block meets the browser's seam. `top` and `bottom` say which of this
/// block's corners are joins (`┬`/`├`/`┴`) rather than rounded.
fn seam_set(seams: Seams, top: &'static str, bottom: &'static str) -> border::Set<'static> {
    let mut set = border::ROUNDED;
    if seams.browser {
        set.top_left = top;
        set.bottom_left = bottom;
    }
    set
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
    let block = pane_block(" files ".to_string(), focused)
        .borders(Borders::TOP | Borders::LEFT | Borders::BOTTOM);
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
        .map(|e| {
            // A mark's rel path is the entry's own name with any trailing
            // `/` stripped — the same spelling `toggle_mark` stores, and
            // directories/targets never appear in `marked` in the first
            // place (see `toggle_mark`'s doc comment).
            let rel = e.name.strip_suffix('/').unwrap_or(&e.name);
            let marked = w.marked.iter().any(|m| m == rel);
            let name = if marked { format!("*{}", e.name) } else { e.name.clone() };
            // Confidence below the configured threshold reads red here too
            // (reviewer's §6 note) — the same rule the File view's own
            // confidence line applies, just against the compact glyph.
            // The same palette the pile speaks: a sniff below the
            // threshold and a stale sidecar are red, a target with no lock
            // or with drift is yellow, a current lock green.
            let status_style = match &e.status {
                EntryStatus::Sniffed { confidence: Some(c), .. } if *c < w.confidence_threshold => {
                    Style::new().fg(BAD)
                }
                EntryStatus::Stale => Style::new().fg(BAD),
                EntryStatus::NoLock | EntryStatus::Drift(_) => Style::new().fg(WARN),
                EntryStatus::Locked => Style::new().fg(OK),
                _ => Style::new(),
            };
            ListItem::new(browser_row(&name, entry_status_text(&e.status), status_style, inner_width))
        })
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
fn browser_row(name: &str, status: String, status_style: Style, width: usize) -> Line<'static> {
    if status.is_empty() {
        return Line::raw(truncate(name, width));
    }
    let status_w = status.chars().count();
    let sep = if width > status_w { 1 } else { 0 };
    let avail_for_name = width.saturating_sub(status_w + sep);
    let name = truncate(name, avail_for_name);
    let used = name.chars().count() + sep + status_w;
    let pad = width.saturating_sub(used);
    Line::from(vec![
        Span::raw(format!("{name}{}", " ".repeat(pad))),
        Span::styled(status, status_style),
    ])
}

fn context_title(ctx: &Context) -> String {
    match ctx {
        Context::Empty => "main".to_string(),
        Context::File { path, raw, .. } => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            // A workbook with several sheets says which one is on show:
            // `[`/`]` page the grid, and a title that does not move with
            // them would caption every sheet as the first.
            match (&raw.grid_sheet, raw.sheets.len()) {
                (Some(sheet), n) if n > 1 => {
                    let i = raw.sheets.iter().position(|(s, ..)| s == sheet).map(|i| i + 1).unwrap_or(1);
                    format!("{name} · sheet {i}/{n} \"{sheet}\"")
                }
                _ => name,
            }
        }
        Context::Query(_) => "result".to_string(),
        Context::Pile { target, .. } => target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| target.display().to_string()),
        Context::Member { report, member, .. } => report
            .members
            .get(*member)
            .map(|m| m.path.clone())
            .unwrap_or_else(|| "member".to_string()),
        Context::Evidence { member, .. } => format!("accept {member} ?"),
    }
}

fn draw_main(f: &mut Frame, area: Rect, w: &Workbench, seams: Seams) {
    let focused = w.focus == Focus::Main;
    let title = format!(" {} ", context_title(&w.context));
    // No bottom border: the console's top border is that line, and draws
    // the joins. The top-left corner is a `┬` on the browser's seam.
    let block = pane_block(title, focused)
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_set(seam_set(seams, "┬", border::ROUNDED.bottom_left));

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
            // The classic screens are gone (slice 3 Task 7): a target on
            // the command line opens this same workbench, already fitted —
            // as a DRY RUN, because opening a review tool to look must not
            // write. Say that, and say which key writes.
            lines.push(Line::raw("`tdy ui <target>` opens it fitted — a dry run until you press f"));
            f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        }
        Context::File { raw, spec, preview, stale, .. } => match spec {
            // No sidecar yet: the raw head as-is, and nothing that looks
            // like an opinion — no columns, no types, no arrows.
            None => draw_file_no_spec(f, area, block, raw, w.main_scroll, *stale),
            // A sidecar exists: raw beside the spec's own decisions. `w`
            // itself (rather than unpacking `main_scroll`/
            // `confidence_threshold` as separate parameters) is what keeps
            // this under the too-many-arguments threshold.
            Some(spec) => draw_file_with_spec(f, area, block, raw, spec, preview.as_ref(), w),
        },
        Context::Query(t) => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            draw_table(f, inner, t, w.main_scroll);
        }
        Context::Pile { report, selected, .. } => {
            draw_pile(f, area, block, report, *selected, w.main_scroll, w.pile_filter);
        }
        Context::Member { report, member, .. } => {
            match report.members.get(*member) {
                // `w` itself, rather than unpacking `raw`/`remedy_selected`/
                // `main_scroll` as separate parameters — the same reason
                // `draw_file_with_spec` takes it, and for the same
                // too-many-arguments threshold.
                Some(m) => draw_member(f, area, block, m, w),
                None => {
                    let inner = block.inner(area);
                    f.render_widget(block, area);
                    f.render_widget(Paragraph::new(Line::styled("?", Style::new().fg(DIM))), inner);
                }
            }
        }
        Context::Evidence { rows, .. } => draw_evidence(f, area, block, rows, w.main_scroll),
    }
}

/// The Evidence view: `.accept`'s step one, rendered — every judgement in
/// `rows` shows its `headline()`, and this is what restores the classic
/// accept screen's load-bearing property (see `evidence::for_spec`'s own
/// doc comment): a `Shift` shows the raw text beside what it parses to, plus
/// the smallest/largest over the *whole* file, because a shift applied the
/// wrong way is invisible in the head of a file and obvious at the ends.
/// Never just the first judgement — accepting the rest unseen is exactly
/// what this screen exists to prevent.
fn draw_evidence(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    rows: &[tdy::evidence::Evidence],
    scroll: usize,
) {
    use tdy::evidence::Evidence;

    let inner = block.inner(area);
    f.render_widget(block, area);
    let [content, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for e in rows {
        lines.push(Line::styled(e.headline(), Style::new().add_modifier(Modifier::BOLD)));
        match e {
            Evidence::Shift { head, smallest, largest, .. } => {
                for p in head.iter().take(5) {
                    lines.push(Line::raw(format!("row {:>5}  {:>14} -> {}", p.row, p.raw, p.parsed)));
                }
                if let Some(p) = smallest {
                    lines.push(Line::raw(format!(
                        "smallest  row {:>5}  {:>14} -> {}",
                        p.row, p.raw, p.parsed
                    )));
                }
                if let Some(p) = largest {
                    lines.push(Line::raw(format!(
                        "largest   row {:>5}  {:>14} -> {}",
                        p.row, p.raw, p.parsed
                    )));
                }
            }
            Evidence::Frame { header, head, .. } => {
                lines.push(Line::raw(header.join(" | ")));
                for r in head.iter().take(5) {
                    lines.push(Line::raw(r.join(" | ")));
                }
            }
            Evidence::Constant { .. } | Evidence::Unillustrated { .. } => {}
        }
        lines.push(Line::raw(""));
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll as u16, 0)),
        content,
    );
    f.render_widget(
        Paragraph::new(Line::styled("a accepts · Esc closes", Style::new().fg(DIM))),
        footer,
    );
}

/// Counts, the declaration, any drift, then the members as a table whose
/// columns are the declared columns — each member's binding sits under the
/// column it supplies, so vocabulary drift across months is a column to read
/// down. `scroll` offsets the whole block: the header lines go first, then
/// the table's rows (`Workbench::follow_pile_selection` relies on
/// `pile_header_rows` being the same arithmetic).
fn draw_pile(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    report: &PileReport,
    selected: usize,
    scroll: usize,
    filter: PileFilter,
) {
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let head = pile_head_lines(report, filter);
    let header_rows = head.len();
    let head_visible: Vec<Line<'static>> = head
        .into_iter()
        .skip(scroll.min(header_rows))
        .map(|l| clip_line(l, inner.width as usize))
        .collect();
    let row_offset = scroll.saturating_sub(header_rows);

    let [head_area, table_area] = Layout::vertical([
        Constraint::Length(head_visible.len() as u16),
        Constraint::Fill(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(head_visible), head_area);
    if table_area.height == 0 {
        return;
    }

    // Column widths from the content: the path, the status word, one column
    // per declared column (its name or its widest binding, capped), and the
    // detail taking what is left.
    let path_w = report.members.iter().map(|m| m.path.chars().count() + 2).max().unwrap_or(8);
    let mut widths = vec![Constraint::Length(path_w as u16), Constraint::Length(8)];
    for c in &report.columns {
        let w = report
            .members
            .iter()
            .filter_map(|m| m.sources.iter().find(|s| s.column == c.name))
            .map(|s| s.source.chars().count())
            .max()
            .unwrap_or(0)
            .max(c.name.chars().count())
            .min(18);
        widths.push(Constraint::Length(w as u16));
    }
    widths.push(Constraint::Fill(1));

    let mut header: Vec<Cell> = vec![Cell::from("member"), Cell::from("status")];
    header.extend(report.columns.iter().map(|c| Cell::from(c.name.clone())));
    header.push(Cell::from("detail"));

    let shown: Vec<(usize, &MemberReport)> = report
        .members
        .iter()
        .enumerate()
        .filter(|(_, m)| filter == PileFilter::All || needs_attention(m))
        .collect();
    let total = shown.len();
    let rows: Vec<Row> = shown
        .into_iter()
        .skip(row_offset)
        .map(|(i, m)| {
            let marker = if i == selected { "▸ " } else { "  " };
            let mut cells: Vec<Cell> = vec![
                Cell::from(format!("{marker}{}", m.path)),
                Cell::from(Span::styled(status_word(m), status_style(m))),
            ];
            for c in &report.columns {
                let bound = m.sources.iter().find(|s| s.column == c.name).map(|s| s.source.clone());
                cells.push(match bound {
                    Some(src) => Cell::from(src),
                    None => Cell::from(Span::styled("·", Style::new().fg(DIM))),
                });
            }
            cells.push(Cell::from(member_detail(m).to_string()));
            let row = Row::new(cells);
            if i == selected {
                row.style(Style::new().add_modifier(SELECTED))
            } else {
                row
            }
        })
        .collect();

    let table = TableWidget::new(rows, widths)
        .header(Row::new(header).style(Style::new().add_modifier(Modifier::BOLD).fg(DIM)))
        .column_spacing(2);
    f.render_widget(table, table_area);

    // A scrollbar only when there is something to scroll: rows beyond the
    // pane, or rows scrolled off its top.
    let visible = table_area.height.saturating_sub(1) as usize;
    if total > visible || row_offset > 0 {
        let mut state =
            ScrollbarState::new(total.saturating_sub(visible).max(1)).position(row_offset);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            table_area,
            &mut state,
        );
    }
}

/// A line broken into as many lines as it needs at `width` cells, each
/// span's style kept across the break. Character-based, not word-based:
/// console output is program text, and a `matches = '…'` remedy broken at a
/// word boundary would still be the same remedy, only harder to paste.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let total: usize = line.spans.iter().map(|sp| sp.content.chars().count()).sum();
    if total <= width || width == 0 {
        return vec![line];
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for sp in line.spans {
        let style = sp.style;
        let mut chars: Vec<char> = sp.content.chars().collect();
        while !chars.is_empty() {
            let room = width - used;
            let take = room.min(chars.len());
            let piece: String = chars.drain(..take).collect();
            cur.push(Span::styled(piece, style));
            used += take;
            if used == width {
                out.push(Line::from(std::mem::take(&mut cur)));
                used = 0;
            }
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

/// A line clipped to `width` cells with an ellipsis, span styles kept: a
/// declaration wider than the pane must read as clipped, not as complete.
fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    let total: usize = line.spans.iter().map(|sp| sp.content.chars().count()).sum();
    if total <= width || width == 0 {
        return line;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for sp in line.spans {
        let n = sp.content.chars().count();
        if used + n < width {
            used += n;
            out.push(sp);
            continue;
        }
        let room = width.saturating_sub(used + 1);
        let text: String = sp.content.chars().take(room).collect::<String>() + "…";
        out.push(Span::styled(text, sp.style));
        break;
    }
    Line::from(out)
}

/// The lines above the pile table: coloured counts and lock state, what the
/// target declares, and any drift against the lock as it stood.
fn pile_head_lines(report: &PileReport, filter: PileFilter) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let sep = || Span::styled(" · ", Style::new().fg(DIM));
    let mut counts: Vec<Span<'static>> = vec![
        Span::styled(format!("{} fitted", report.fitted), bold.fg(if report.fitted > 0 { OK } else { DIM })),
        sep(),
        Span::styled(format!("{} failed", report.failed), bold.fg(if report.failed > 0 { BAD } else { DIM })),
        sep(),
        Span::styled(
            format!("{} need review", report.needs_review),
            bold.fg(if report.needs_review > 0 { WARN } else { DIM }),
        ),
        sep(),
    ];
    counts.push(if report.lock_written.is_some() {
        Span::styled("lock written", bold.fg(OK))
    } else {
        Span::styled("no lock", bold.fg(WARN))
    });
    if report.dry_run {
        counts.push(sep());
        counts.push(Span::styled("dry run", bold.fg(WARN)));
    }
    let mut lines = vec![Line::from(counts)];

    if !report.columns.is_empty() {
        let decl: Vec<String> = report
            .columns
            .iter()
            .map(|c| {
                let mut s = format!("{} {}", c.name, c.dtype);
                if !c.nullable {
                    s.push_str(" NOT NULL");
                }
                if !c.matches.is_empty() {
                    s.push_str(&format!(" ({})", c.matches.join(", ")));
                }
                if c.if_missing_null {
                    s.push_str(" [if missing: null]");
                }
                s
            })
            .collect();
        lines.push(Line::from(vec![
            Span::styled("declares  ", Style::new().fg(DIM)),
            Span::raw(decl.join("  ·  ")),
        ]));
    }
    for d in report.drift.iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled("drift     ", Style::new().fg(WARN).add_modifier(Modifier::BOLD)),
            Span::styled(d.clone(), Style::new().fg(WARN)),
        ]));
    }
    if report.drift.len() > 3 {
        lines.push(Line::styled(
            format!("          … {} more", report.drift.len() - 3),
            Style::new().fg(WARN),
        ));
    }
    if filter == PileFilter::Problems {
        let n = report.members.iter().filter(|m| needs_attention(m)).count();
        lines.push(Line::styled(
            format!("showing problems only ({n} of {}) · / shows all", report.members.len()),
            Style::new().fg(WARN),
        ));
    }
    lines.push(Line::raw(""));
    lines
}

/// How many lines `draw_pile` puts above the first member row: its head
/// lines plus the table's own header row. `Workbench::follow_pile_selection`
/// reads this so the selection it keeps on screen is the one drawn.
pub fn pile_header_rows(report: &PileReport, filter: PileFilter) -> usize {
    pile_head_lines(report, filter).len() + 1
}

/// The palette applied to a member's status word.
fn status_style(m: &MemberReport) -> Style {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    if m.accepted {
        return bold.fg(OK);
    }
    match m.status {
        MemberStatus::Fits => bold.fg(OK),
        MemberStatus::NeedsReview => bold.fg(WARN),
        MemberStatus::Gaps | MemberStatus::Contradicts | MemberStatus::Error => bold.fg(BAD),
    }
}

/// `accepted` wins over `REVIEW` — a reviewed-and-accepted member is no
/// longer waiting on anyone. `Contradicts` and `Error` both read as `GAP`:
/// from this list they are all "does not fit," and the row's detail text
/// (the review note or the first problem's message) is where the
/// distinction actually lives.
fn status_word(m: &MemberReport) -> &'static str {
    if m.accepted {
        return "accepted";
    }
    match m.status {
        MemberStatus::Fits => "fits",
        MemberStatus::NeedsReview => "REVIEW",
        MemberStatus::Gaps | MemberStatus::Contradicts | MemberStatus::Error => "GAP",
    }
}

/// The first line of the member's review note, or (failing that) its first
/// problem's message — whichever explains the status word.
fn member_detail(m: &MemberReport) -> &str {
    m.review
        .as_deref()
        .or_else(|| m.problems.first().map(|p| p.message.as_str()))
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
}

/// The Member view: the gap beside the file's own rows, and the two halves
/// pointing at each other. Left is the member's raw head, verbatim — the
/// file's own header spelling, which is what a `matches` clause is written
/// against — sized to its content rather than to half the pane, with the
/// header cells `--propose` can bind in green and the ones the problem
/// implicates in yellow. Right is the status word, the review reason, each
/// problem rendered from its *structure* (the names tried and the file's
/// header as lists, one per line) and the numbered remedy menu, the
/// selected remedy reversed like every other selection. The raw head
/// scrolls with `main_scroll`; the right column does not.
///
/// Takes `w` itself, rather than unpacking `raw`/`remedy_selected`/
/// `main_scroll` as separate parameters, to stay under the
/// too-many-arguments threshold — the same reason `draw_file_with_spec`
/// does.
fn draw_member(f: &mut Frame, area: Rect, block: Block<'static>, m: &MemberReport, w: &Workbench) {
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Context::Member { raw, remedy_selected, .. } = &w.context else {
        // The caller already matched `w.context` to get here — this is
        // unreachable in practice, but drawing must still be total.
        f.render_widget(Paragraph::new(Line::styled("?", Style::new().fg(DIM))), inner);
        return;
    };
    let remedy_selected = *remedy_selected;
    let remedies = w.member_remedies();
    let marks = Highlights::for_member(m);

    // The raw head takes the width its content needs (plus a gutter), never
    // less than a readable minimum and never more than 60% of the pane: a
    // four-row CSV must not push the problem text into a strip that breaks
    // every spelling across two lines.
    let raw_w = raw
        .as_ref()
        .map(|r| r.max_width() + 2)
        .unwrap_or(0)
        .clamp(24, (inner.width as usize * 3 / 5).max(24)) as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Length(raw_w), Constraint::Fill(1)]).areas(inner);

    match raw {
        Some(r) => draw_raw_head(f, left, r, w.main_scroll, &marks),
        None => f.render_widget(Paragraph::new(Line::styled("loading…", Style::new().fg(DIM))), left),
    }

    let mut lines = vec![Line::styled(status_word(m), status_style(m))];
    if let Some(review) = &m.review {
        lines.push(Line::raw(""));
        for l in review.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    for p in &m.problems {
        lines.push(Line::raw(""));
        lines.extend(problem_lines(p, &marks));
    }
    if !remedies.is_empty() {
        lines.push(Line::raw(""));
        for (i, r) in remedies.iter().enumerate() {
            let marker = if i == remedy_selected { "▸ " } else { "  " };
            let text = format!("{marker}{}. {}", i + 1, r.label());
            lines.push(if i == remedy_selected {
                Line::styled(text, Style::new().add_modifier(SELECTED))
            } else {
                Line::raw(text)
            });
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), right);
}

/// Which of the file's own header cells the member view colours, and how.
#[derive(Default)]
struct Highlights {
    candidates: Vec<String>,
    implicated: Vec<String>,
}

impl Highlights {
    fn for_member(m: &MemberReport) -> Self {
        let mut h = Highlights::default();
        for p in &m.proposals {
            h.candidates.extend(p.candidates.iter().map(|(name, _)| name.clone()));
        }
        for p in &m.problems {
            match p.kind.as_str() {
                // `Betrag (column 3)` — the spelling is what the file says.
                "ambiguous" => h.implicated.extend(
                    p.choices.iter().map(|c| c.split(" (column ").next().unwrap_or(c).to_string()),
                ),
                "untypable" | "ambiguous_separator" | "ambiguous_format" | "collides" => {
                    h.implicated.extend(p.choices.iter().cloned())
                }
                _ => {}
            }
            if let Some(holder) = &p.long_form {
                h.implicated.push(holder.clone());
            }
        }
        h
    }

    fn style_of(&self, cell: &str) -> Option<Style> {
        let cell = cell.trim();
        if self.implicated.iter().any(|c| c == cell) {
            Some(Style::new().fg(IMPLICATED).add_modifier(Modifier::BOLD))
        } else if self.candidates.iter().any(|c| c == cell) {
            Some(Style::new().fg(CANDIDATE).add_modifier(Modifier::BOLD))
        } else {
            None
        }
    }

    /// A raw text line with every highlighted spelling styled where it
    /// occurs. Longest spellings first, so `Betrag CHF` is not split by
    /// `Betrag`; an occurrence inside a longer one is left alone.
    fn line(&self, text: &str) -> Line<'static> {
        let mut names: Vec<&String> = self.implicated.iter().chain(self.candidates.iter()).collect();
        names.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
        names.dedup();
        // (start, end, style) byte ranges, non-overlapping.
        let mut marks: Vec<(usize, usize, Style)> = Vec::new();
        for n in names {
            if n.is_empty() {
                continue;
            }
            let Some(style) = self.style_of(n) else { continue };
            let mut from = 0;
            while let Some(i) = text[from..].find(n.as_str()) {
                let (a, b) = (from + i, from + i + n.len());
                if !marks.iter().any(|(x, y, _)| a < *y && b > *x) {
                    marks.push((a, b, style));
                }
                from = b;
            }
        }
        marks.sort_by_key(|m| m.0);
        let mut spans = Vec::new();
        let mut at = 0;
        for (a, b, style) in marks {
            if a > at {
                spans.push(Span::raw(text[at..a].to_string()));
            }
            spans.push(Span::styled(text[a..b].to_string(), style));
            at = b;
        }
        if at < text.len() {
            spans.push(Span::raw(text[at..].to_string()));
        }
        Line::from(spans)
    }
}

/// One problem, from its structure rather than its prose: the first line of
/// the message is the headline; the names tried and the file's own header
/// are lists, one item per line, the header cells carrying the same colours
/// the raw head does; a long-form diagnosis names the holding column; an
/// ambiguous binding lists its choices. The CLI's remedy prose is left out
/// here — the numbered menu beneath is that remedy, made pressable.
fn problem_lines(p: &tdy::report::Problem, marks: &Highlights) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        p.message.lines().next().unwrap_or("").to_string(),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    let dim = Style::new().fg(DIM);
    if !p.tried.is_empty() {
        lines.push(Line::styled("looked for", dim));
        for t in &p.tried {
            lines.push(Line::raw(format!("  {t}")));
        }
    }
    if !p.header.is_empty() {
        lines.push(Line::styled("the file has", dim));
        for h in &p.header {
            let text = format!("  {h}");
            lines.push(match marks.style_of(h) {
                Some(style) => Line::styled(text, style),
                None => Line::raw(text),
            });
        }
    }
    if let Some(holder) = &p.long_form {
        lines.push(Line::styled(
            format!("\"{holder}\" holds those names as values — this file is in long form; tdy has no pivot"),
            Style::new().fg(IMPLICATED),
        ));
    }
    if p.kind == "ambiguous" && !p.choices.is_empty() {
        lines.push(Line::styled("matches", dim));
        for c in &p.choices {
            lines.push(Line::styled(format!("  {c}"), Style::new().fg(IMPLICATED)));
        }
    }
    if p.tried.is_empty() && p.header.is_empty() && p.choices.is_empty() {
        for l in p.message.lines().skip(1) {
            lines.push(Line::raw(l.to_string()));
        }
    }
    lines
}

/// The raw head drawn into `area`: sheet lines and text lines as a
/// paragraph (the header line carrying `marks`), then a workbook's grid as
/// a table whose first row carries them too. `scroll` moves the text
/// lines first and the grid rows after them, so a text file's scroll is
/// exactly the old paragraph scroll.
fn draw_raw_head(f: &mut Frame, area: Rect, raw: &RawHead, scroll: usize, marks: &Highlights) {
    let text_lines = raw_text_lines(raw, marks);
    if raw.grid.is_empty() {
        f.render_widget(Paragraph::new(text_lines).scroll((scroll as u16, 0)), area);
        return;
    }
    let shown = text_lines.len().saturating_sub(scroll) as u16;
    let grid_offset = scroll.saturating_sub(text_lines.len());
    let [top, grid_area] =
        Layout::vertical([Constraint::Length(shown.min(area.height)), Constraint::Fill(1)]).areas(area);
    f.render_widget(Paragraph::new(text_lines).scroll((scroll as u16, 0)), top);
    if grid_area.height == 0 {
        return;
    }
    let ncols = raw.grid.iter().map(|r| r.len()).max().unwrap_or(0);
    let widths: Vec<Constraint> = (0..ncols)
        .map(|c| {
            let w = raw.grid.iter().filter_map(|r| r.get(c)).map(|v| v.chars().count()).max().unwrap_or(1);
            Constraint::Length(w.clamp(1, GRID_CELL_MAX) as u16)
        })
        .collect();
    let rows: Vec<Row> = raw
        .grid
        .iter()
        .enumerate()
        .skip(grid_offset)
        .map(|(i, r)| {
            let cells: Vec<Cell> = r
                .iter()
                .map(|v| {
                    let text = truncate(v, GRID_CELL_MAX);
                    match (i == 0).then(|| marks.style_of(v)).flatten() {
                        Some(style) => Cell::from(Span::styled(text, style)),
                        None => Cell::from(text),
                    }
                })
                .collect();
            let row = Row::new(cells);
            if i == 0 { row.style(Style::new().add_modifier(Modifier::BOLD)) } else { row }
        })
        .collect();
    f.render_widget(TableWidget::new(rows, widths).column_spacing(1), grid_area);
}

/// A workbook grid cell is clipped to this many characters (`…` inside), as
/// it always was — a 300-character title cell must not push every other
/// column off the pane.
const GRID_CELL_MAX: usize = 14;

/// The text part of a raw head: one `sheet "Name": R row(s) x C col(s)`
/// line per sheet, the file's own lines (the first — the header — carrying
/// `marks`), the caption naming the sheet the grid came from, and a `…`
/// when the text read was truncated. The grid itself is a table, drawn by
/// `draw_raw_head`.
fn raw_text_lines(raw: &RawHead, marks: &Highlights) -> Vec<Line<'static>> {
    if raw.lines.is_empty() && raw.sheets.is_empty() && raw.grid.is_empty() {
        return vec![Line::styled("reading…", Style::new().fg(DIM))];
    }
    let mut lines = Vec::new();
    for (name, rows, cols) in &raw.sheets {
        lines.push(Line::raw(format!("sheet \"{name}\": {rows} row(s) x {cols} col(s)")));
    }
    for (i, l) in raw.lines.iter().enumerate() {
        lines.push(if i == 0 { marks.line(l) } else { Line::raw(l.clone()) });
    }
    // The grid is whichever sheet `grid_sheet` names — the first by
    // default, another when `--sheet`/`[`/`]` picked it. A workbook may
    // list a dozen sheets above it, so name the one these rows came from
    // rather than let them read as the whole book.
    if let Some(name) = &raw.grid_sheet {
        lines.push(Line::raw(format!("grid of sheet \"{name}\":")));
    }
    if raw.truncated {
        lines.push(Line::styled("…", Style::new().fg(DIM)));
    }
    lines
}

/// No sidecar: raw head only, plus a footer naming the fact that it is
/// unopinionated — never a column name, a type, or an arrow. `stale` (a
/// sidecar exists but its fingerprint no longer matches the file — see
/// `Context::File::stale`) points at the fix that actually applies instead
/// of the plain "not sniffed" hint, which would send someone to re-run a
/// command that will just report the same staleness back.
fn draw_file_no_spec(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    raw: &RawHead,
    scroll: usize,
    stale: bool,
) {
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [content, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    draw_raw_head(f, content, raw, scroll, &Highlights::default());
    let footer_text = if stale { "sidecar stale — `.sniff --force`" } else { "not sniffed — press s" };
    f.render_widget(
        Paragraph::new(Line::styled(footer_text, Style::new().fg(DIM))),
        footer,
    );
}

/// A sidecar exists: raw beside the spec's decisions, two even columns,
/// with the preview table (when there is one) spanning the bottom. Takes
/// `w` itself, rather than unpacking `main_scroll`/`confidence_threshold` as
/// separate parameters, to stay under the too-many-arguments threshold.
fn draw_file_with_spec(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    raw: &RawHead,
    spec: &SpecSummary,
    preview: Option<&Table>,
    w: &Workbench,
) {
    let scroll = w.main_scroll;
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
            let want = t.rows.len() as u16 + table_header_rows(t) + 1;
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
    draw_raw_head(f, left, raw, scroll, &Highlights::default());
    f.render_widget(Paragraph::new(spec_lines(spec, w.confidence_threshold, raw)), right);

    if let (Some(bottom), Some(t)) = (bottom, preview) {
        draw_table(f, bottom, t, 0);
    }
}

/// The spec summary: method, confidence (red below `threshold` — the
/// configured `confidence_threshold`, the same number the engine escalates
/// to the model below), each column as `name ← "source" : TYPE`, then the
/// notes as a decisions list.
fn spec_lines(spec: &SpecSummary, threshold: f32, raw: &RawHead) -> Vec<Line<'static>> {
    let mut lines = vec![Line::raw(format!("method: {}", spec.method))];
    lines.push(match spec.confidence {
        Some(c) => {
            let style = if c < threshold { Style::new().fg(Color::Red) } else { Style::new() };
            Line::styled(format!("confidence: {c:.2}"), style)
        }
        None => Line::raw("confidence: —".to_string()),
    });
    lines.push(Line::raw(""));
    // TYPE is the fact that matters here — whether a column round-trips at
    // all — so it should never be the half that clips off the right edge
    // of this pane's 50% split; SOURCE (the file's own header spelling,
    // which can be arbitrarily long — a spreadsheet title row, an XML tag
    // path) is what gives way, the same status-first policy `browser_row`
    // applies to a browser entry's name vs. status. Unlike `browser_row`,
    // this is a fixed cap rather than one computed from the pane's actual
    // width — `spec_lines` is not handed one — so it is a mitigation for
    // any real column header, not a proof for every terminal size.
    // `tests/wb_render.rs::a_long_source_is_ellipsized_so_the_type_never_clips`
    // measures the margin this actually buys at a realistic 132-column
    // frame: a `betrag ← "…" : DECIMAL(38,2)` row comes out to ~51
    // characters against a spec-pane half of ~52 there — about one column
    // of slack, not headroom. A longer column name or a longer TYPE
    // (`TIMESTAMP(3)` with a timezone note, say) at that same width would
    // still clip; the fixed cap is an accepted trade-off against a real
    // width-aware truncation like `browser_row`'s, not a guarantee.
    const SOURCE_MAX: usize = 24;
    for (name, source, ty) in &spec.columns {
        let source = truncate(source, SOURCE_MAX);
        lines.push(Line::raw(format!("{name} ← \"{source}\" : {ty}")));
    }
    if !spec.notes.is_empty() {
        lines.push(Line::raw(""));
        for note in &spec.notes {
            lines.push(Line::raw(format!("• {note}")));
            // A decision about a column, beside the values that drove it:
            // the first few of that column from the head on the left, so
            // "read as decimal(2)" sits next to `1'100.00`.
            let examples = decision_examples(note, spec, raw);
            if !examples.is_empty() {
                lines.push(Line::styled(
                    format!("  e.g. {}", examples.join(" · ")),
                    Style::new().fg(DIM),
                ));
            }
        }
    }
    lines
}

/// The first few raw values of the column a note is about (`column
/// \`name\`: …`), read from the raw head: a workbook's grid by header cell,
/// a text file's lines by the extraction's delimiter. Empty when the note
/// is not about a column, or the column cannot be found in the head —
/// never a guess at which column was meant.
fn decision_examples(note: &str, spec: &SpecSummary, raw: &RawHead) -> Vec<String> {
    let Some(rest) = note.strip_prefix("column `") else { return Vec::new() };
    let Some(end) = rest.find('`') else { return Vec::new() };
    let name = &rest[..end];
    let Some((_, source, _)) = spec.columns.iter().find(|(n, ..)| n == name) else { return Vec::new() };

    let (header, rows): (Vec<String>, Vec<Vec<String>>) = if !raw.grid.is_empty() {
        (raw.grid[0].clone(), raw.grid[1..].to_vec())
    } else {
        let delim: Option<char> = serde_json::from_str::<serde_json::Value>(&spec.extraction)
            .ok()
            .and_then(|v| v.get("delimiter").and_then(|d| d.as_str()).and_then(|d| d.chars().next()));
        let Some(delim) = delim else { return Vec::new() };
        let split = |l: &str| -> Vec<String> {
            l.split(delim).map(|c| c.trim().trim_matches('"').to_string()).collect()
        };
        let mut it = raw.lines.iter();
        let Some(h) = it.next() else { return Vec::new() };
        (split(h), it.map(|l| split(l)).collect())
    };
    let Some(idx) = header.iter().position(|h| h == source) else { return Vec::new() };
    rows.iter()
        .filter_map(|r| r.get(idx))
        .filter(|v| !v.trim().is_empty())
        .take(3)
        .cloned()
        .collect()
}

/// A `Table`, drawn as one: a bold header (with the column types beneath
/// the names when the table carries them, as a query result does), the
/// rows with numeric columns right-aligned so amounts read as a column,
/// and a count line. `scroll` skips leading rows.
fn draw_table(f: &mut Frame, area: Rect, t: &Table, scroll: usize) {
    if area.height == 0 {
        return;
    }
    let [table_area, count_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let ncols = t.columns.len();
    let numeric: Vec<bool> = (0..ncols)
        .map(|c| {
            let mut any = false;
            t.rows.iter().filter_map(|r| r.get(c)).all(|v| {
                let v = v.trim();
                if v.is_empty() {
                    return true;
                }
                any = true;
                v.parse::<f64>().is_ok()
            }) && any
        })
        .collect();
    let widths: Vec<Constraint> = (0..ncols)
        .map(|c| {
            let head = t.columns[c].chars().count().max(t.types.get(c).map(|s| s.chars().count()).unwrap_or(0));
            let body = t.rows.iter().filter_map(|r| r.get(c)).map(|v| v.chars().count()).max().unwrap_or(0);
            Constraint::Length(head.max(body).clamp(1, TABLE_CELL_MAX) as u16)
        })
        .collect();
    let align = |c: usize| if numeric.get(c).copied().unwrap_or(false) { Alignment::Right } else { Alignment::Left };

    let header_cells: Vec<Cell> = (0..ncols)
        .map(|c| {
            let name = Line::styled(t.columns[c].clone(), Style::new().add_modifier(Modifier::BOLD)).alignment(align(c));
            match t.types.get(c) {
                Some(ty) if !ty.is_empty() => Cell::from(Text::from(vec![
                    name,
                    Line::styled(ty.clone(), Style::new().fg(DIM)).alignment(align(c)),
                ])),
                _ => Cell::from(name),
            }
        })
        .collect();
    let rows: Vec<Row> = t
        .rows
        .iter()
        .skip(scroll)
        .map(|r| {
            Row::new((0..ncols).map(|c| {
                let v = r.get(c).cloned().unwrap_or_default();
                Cell::from(Line::raw(truncate(&v, TABLE_CELL_MAX)).alignment(align(c)))
            }))
        })
        .collect();
    let table = TableWidget::new(rows, widths)
        .header(Row::new(header_cells).height(table_header_rows(t)))
        .column_spacing(2);
    f.render_widget(table, table_area);

    let mut count = format!("{} row(s)", t.total);
    if t.truncated {
        count.push_str(" (truncated)");
    }
    if scroll > 0 {
        count.push_str(&format!(" · from row {}", scroll + 1));
    }
    f.render_widget(Paragraph::new(Line::styled(count, Style::new().fg(DIM))), count_area);
}

/// Two header rows when the table names its types, one otherwise.
fn table_header_rows(t: &Table) -> u16 {
    if t.types.iter().any(|ty| !ty.is_empty()) { 2 } else { 1 }
}

/// A table cell is clipped to this many characters; a free-text column
/// must not push the numbers off the pane.
const TABLE_CELL_MAX: usize = 40;

/// `below_main`: this console sits under the main pane, so its top border
/// is the line the two share and its top corners are joins (`├`, `┤`)
/// rather than rounded. Zoomed, it stands alone and keeps its own corners.
fn draw_console(f: &mut Frame, area: Rect, w: &Workbench, seams: Seams, below_main: bool) {
    let focused = w.focus == Focus::Console;
    let mut set = seam_set(seams, "┬", "┴");
    if below_main {
        set.top_left = "├";
        set.top_right = "┤";
    }
    let block = pane_block(" console ".to_string(), focused).border_set(set);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for cell in &w.scrollback {
        // A failed command's echo line reads red — the field stays honest
        // about what actually ran, styling only. A multi-line echo (a SQL
        // statement assembled across several `   -> ` continuation prompts)
        // is split the same way the real console showed it as it was
        // typed: `tdy> ` on the first line, `   -> ` on the rest — never a
        // single `tdy>` line embedding a raw newline.
        let echo_style = if cell.ok { Style::new().fg(DIM) } else { Style::new().fg(Color::Red) };
        let mut echo_lines = cell.echo.split('\n');
        if let Some(first) = echo_lines.next() {
            lines.push(Line::styled(format!("tdy> {first}"), echo_style));
        }
        for cont in echo_lines {
            lines.push(Line::styled(format!("   -> {cont}"), echo_style));
        }
        for l in cell.text.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    // A line wider than the pane wraps rather than losing its tail — the
    // tail of an error line is where the remedy is.
    let width = inner.width.max(1) as usize;
    let lines: Vec<Line> = lines.into_iter().flat_map(|l| wrap_line(l, width)).collect();

    let content_rows = inner.height.saturating_sub(1) as usize;
    let total = lines.len();
    let end = total.saturating_sub(w.scroll);
    let start = end.saturating_sub(content_rows);
    let mut visible: Vec<Line> = lines[start..end].to_vec();
    if w.scroll > 0 && !visible.is_empty() {
        // Say so, and how far: a scrolled console that looks like the end
        // of the transcript is how the last command's error goes unread.
        visible[0] = Line::styled(
            format!("⋮ {} more line(s) below · PgDn", total - end),
            Style::new().fg(WARN),
        );
    }

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
        Some(what) => (
            format!(" {} {what}", SPINNER[(w.tick % SPINNER.len() as u64) as usize]),
            Style::new().fg(WARN),
        ),
        None => (format!(" {}", w.status), Style::new().fg(DIM)),
    };
    let keys = match w.focus {
        Focus::Console => "Tab focus · ^L zoom · ^Q quit",
        Focus::Browser => "↑↓ move · enter open · s sniff · e edit · Tab focus · ^Q quit",
        // The keys that actually do something in Main depend on what Main
        // is showing — a Pile's own keys (`f`, `t`) mean nothing in a
        // Member's remedy menu and vice versa, so this mirrors `key_main`'s
        // own match on `w.context` rather than giving every context the
        // same generic "↑↓ scroll" hint. `^Q quit` is the one key that
        // works everywhere regardless of context, so every arm advertises
        // it — consistent with `Console`/`Browser`/`File` above, none of
        // which drop it either.
        Focus::Main => match &w.context {
            Context::Pile { .. } => "↑↓ member · g next problem · / filter · enter open · f refit · t edit target · ^Q quit",
            Context::Member { .. } => "↑↓ remedy · enter/1-9 stage · a accept · e edit · Esc back · ^Q quit",
            Context::Evidence { .. } => "a accept · Esc close · PgUp/Dn scroll · ^Q quit",
            Context::File { raw, .. } if raw.sheets.len() > 1 => {
                "↑↓ scroll · [ ] sheet · Tab focus · ^Q quit"
            }
            Context::File { .. } => "↑↓ scroll · Tab focus · ^Q quit",
            // A result table scrolls now (`key_main`'s fallback arm), so
            // it advertises the keys that move it; `Empty` has nothing to
            // scroll and keeps the bare hint.
            Context::Query(_) => "↑↓ scroll · Tab focus · ^Q quit",
            Context::Empty => "Tab focus · ^Q quit",
        },
    };
    let [left, right] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(keys.chars().count() as u16 + 2),
    ])
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
