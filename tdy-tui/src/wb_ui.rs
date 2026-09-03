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
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use tdy::console::{EntryStatus, RawHead, SpecSummary, Table};
use tdy::report::{MemberReport, MemberStatus, PileReport};

use crate::mark;
use crate::remedy::{Edit, Remedy};
use crate::workbench::{Context, Focus, Workbench};

const DIM: Color = Color::DarkGray;
/// Below this many columns the file browser has nowhere to go; the console
/// (where typing still works) keeps the space instead.
const MIN_WIDTH_FOR_BROWSER: u16 = 60;
const BROWSER_WIDTH: u16 = 26;
/// Below this many inner rows the Empty view drops the mark rather than
/// squeeze it against the orientation text beneath it: mark = 9 rows incl.
/// spacing + 3 orientation lines; at 10 the Paragraph clipped the tail.
const MARK_MIN_HEIGHT: u16 = 13;

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
    ("PgUp / PgDn (main)", "scroll the main pane"),
    ("[ / ] (file / member)", "previous / next sheet of a workbook"),
    ("Enter", "open file or directory"),
    ("Backspace", "go up a directory"),
    ("s", "sniff the selected file"),
    ("e", "edit the selected file"),
    ("f", "fit the selected target / re-fit the shown pile"),
    ("t (pile / member)", "edit the target"),
    ("d", "mark/unmark the selected file"),
    ("D", "draft the marked files"),
    ("↑ / ↓ (pile)", "move the selected member"),
    ("Enter (pile)", "open the selected member"),
    ("Esc (pile)", "close the pile"),
    ("↑ / ↓ (member)", "pick a remedy"),
    ("e (member)", "edit the file"),
    ("1-9 (member)", "apply a remedy"),
    ("Enter (member)", "apply the marked (▸) remedy"),
    ("a (member / evidence)", "accept — show evidence, then accept"),
    ("Esc (member)", "back to the pile"),
    ("↑ / ↓ (evidence)", "scroll"),
    ("Esc (evidence)", "close (f re-opens the pile)"),
    ("y / Esc (confirm)", "write the edit / cancel"),
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

/// How many rows of content the main pane can show at `height` terminal
/// rows — the same arithmetic `draw`/`draw_right` perform with Layout:
/// 1 header row + 1 status row around the body, `console_rows + 2` for the
/// console pane, 2 for the main block's own borders. 0 when the console is
/// zoomed (no main pane on screen) — `set_main_view_rows` ignores 0.
pub fn main_inner_rows(height: u16, w: &Workbench) -> usize {
    if w.zoom {
        return 0;
    }
    let body = height.saturating_sub(2);
    let main = body.saturating_sub(w.console_rows + 2);
    main.saturating_sub(2) as usize
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
    // Checked first, ahead of `help` and `zoom`: a staged edit is modal (see
    // `Workbench::key`), and the overlay confirming it must cover the whole
    // right column exactly as the help overlay does, for the same reason —
    // it can be open regardless of whether the console is zoomed.
    if let Some((remedy, edit, ..)) = &w.pending_edit {
        draw_confirm(f, area, remedy, edit);
        return;
    }
    // Checked before `zoom`: `?` opens help from Browser/Main focus
    // regardless of whether the console is currently zoomed (Tab still
    // moves focus off the console while zoomed), so the overlay must cover
    // the whole right column here rather than a `main` sub-area that may
    // not have been computed — the zoom branch below never runs one.
    if w.help {
        draw_help(f, area);
        return;
    }
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

/// The staged-edit confirm overlay: a bordered ` confirm edit ` block over
/// the main pane, mirroring `draw_help`'s mechanism — the remedy's label,
/// then `Edit::diff()`'s lines (`-` red-ish, `+` green-ish), then a footer
/// naming the two keys `Workbench::key`'s modal branch actually honours.
/// `Workbench` owns when this is shown and what `y`/`Esc` do; this only
/// draws it.
fn draw_confirm(f: &mut Frame, area: Rect, remedy: &Remedy, edit: &Edit) {
    let block = Block::bordered()
        .title(" confirm edit ")
        .border_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [content, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);

    let mut lines = vec![Line::raw(remedy.label()), Line::raw(String::new())];
    for l in edit.diff().lines() {
        // The diff line is `"{:>4} - {before}"` or `"{:>4} + {after}"` (see
        // `Edit::diff`): the marker is always the second whitespace-split
        // token, regardless of how wide the line-number field happened to
        // print.
        let style = match l.split_whitespace().nth(1) {
            Some("-") => Style::new().fg(Color::Red),
            Some("+") => Style::new().fg(Color::Green),
            _ => Style::new(),
        };
        lines.push(Line::styled(l.to_string(), style));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);
    f.render_widget(
        Paragraph::new(Line::styled("y writes the target · Esc cancels", Style::new().fg(DIM))),
        footer,
    );
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
            let status_style = match &e.status {
                EntryStatus::Sniffed { confidence: Some(c), .. } if *c < w.confidence_threshold => {
                    Style::new().fg(Color::Red)
                }
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
        Context::File { path, .. } => path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
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
            f.render_widget(
                Paragraph::new(table_lines(t)).scroll((w.main_scroll as u16, 0)),
                inner,
            );
        }
        Context::Pile { report, selected, .. } => {
            draw_pile(f, area, block, report, *selected, w.main_scroll);
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

/// The Member view: the gap beside the file's own rows. Left column is the
/// member's raw head, verbatim — the file's own header spelling, which is
/// exactly what a `matches` clause needs to be written against, and "loading…"
/// until the runtime's `PreviewFile` result lands, scrolled by `main_scroll`
/// the same way `draw_file_no_spec` scrolls a `File` context's raw head —
/// the right column (status/review/remedy menu) is not scrolled, mirroring
/// `draw_file_with_spec`'s own left-only scroll. Right column is the
/// member's status, its review reason (if a judgement is what it is waiting
/// on), each problem's message, then a blank line and the numbered remedy
/// menu — `▸` marks `remedy_selected`. An accepted member's status word
/// already covers "accepted" (see `status_word`), and naturally has no
/// remedies to list.
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
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);

    let left_lines = match raw {
        Some(r) => raw_head_lines(r),
        None => vec![Line::styled("loading…", Style::new().fg(DIM))],
    };
    f.render_widget(
        Paragraph::new(left_lines).scroll((w.main_scroll as u16, 0)),
        left,
    );

    let mut lines = Vec::new();
    let status_style = if m.accepted {
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    lines.push(Line::styled(status_word(m), status_style));
    if let Some(review) = &m.review {
        lines.push(Line::raw(""));
        for l in review.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    for p in &m.problems {
        lines.push(Line::raw(""));
        for l in p.message.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    if !remedies.is_empty() {
        lines.push(Line::raw(""));
        for (i, r) in remedies.iter().enumerate() {
            let marker = if i == remedy_selected { "▸ " } else { "  " };
            lines.push(Line::raw(format!("{marker}{}. {}", i + 1, r.label())));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), right);
}

/// Header (target name, counts, lock state) then one row per member —
/// `▸ path  status  detail`, truncated to the pane's width. `scroll` offsets
/// the whole block (header included) the way `draw_evidence` offsets
/// its own lines — a long pile scrolls the header out of view along with
/// early members, which is the same trade `draw_file_no_spec` already makes
/// for its raw head.
fn draw_pile(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    report: &PileReport,
    selected: usize,
    scroll: usize,
) {
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut header = format!(
        "{} fitted · {} failed · {} need review",
        report.fitted, report.failed, report.needs_review
    );
    header.push_str(if report.lock_written.is_some() { " · lock written" } else { " · no lock" });
    if report.dry_run {
        header.push_str(" · dry run");
    }

    let mut lines = vec![Line::styled(header, Style::new().add_modifier(Modifier::BOLD)), Line::raw("")];
    for (i, m) in report.members.iter().enumerate() {
        let marker = if i == selected { "▸ " } else { "  " };
        let detail = member_detail(m);
        let row = format!("{marker}{}  {}  {detail}", m.path, status_word(m));
        lines.push(Line::raw(truncate(&row, inner.width as usize)));
    }
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
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

/// The raw head, verbatim: file lines, or (for a workbook) one
/// `sheet "Name": R row(s) x C col(s)` line per sheet, then the file's own
/// lines if any were also sampled, then the first sheet's grid under a line
/// naming that sheet (its own header spelling and raw values —
/// tab-per-sheet stays future work). A trailing `…` marks a truncated text
/// read; a grid clipped by its own cap carries its markers inside the grid,
/// put there by `engine::sheet_grid`, which is the only place that knows
/// both the cap and the sheet's true extent.
fn raw_head_lines(raw: &RawHead) -> Vec<Line<'static>> {
    if raw.lines.is_empty() && raw.sheets.is_empty() && raw.grid.is_empty() {
        return vec![Line::styled("reading…", Style::new().fg(DIM))];
    }
    let mut lines = Vec::new();
    for (name, rows, cols) in &raw.sheets {
        lines.push(Line::raw(format!("sheet \"{name}\": {rows} row(s) x {cols} col(s)")));
    }
    for l in &raw.lines {
        lines.push(Line::raw(l.clone()));
    }
    // The grid is the FIRST sheet's; a workbook may list a dozen above it,
    // so name the one these rows came from rather than let them read as the
    // whole book.
    if let Some(name) = &raw.grid_sheet {
        lines.push(Line::raw(format!("grid of sheet \"{name}\":")));
    }
    for row in &raw.grid {
        let cells: Vec<String> = row.iter().map(|c| truncate(c, 14)).collect();
        lines.push(Line::raw(cells.join(" | ")));
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
    f.render_widget(
        Paragraph::new(raw_head_lines(raw)).scroll((scroll as u16, 0)),
        content,
    );
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
    f.render_widget(Paragraph::new(spec_lines(spec, w.confidence_threshold)), right);

    if let (Some(bottom), Some(t)) = (bottom, preview) {
        f.render_widget(Paragraph::new(table_lines(t)), bottom);
    }
}

/// The spec summary: method, confidence (red below `threshold` — the
/// configured `confidence_threshold`, the same number the engine escalates
/// to the model below), each column as `name ← "source" : TYPE`, then the
/// notes as a decisions list.
fn spec_lines(spec: &SpecSummary, threshold: f32) -> Vec<Line<'static>> {
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
        // The keys that actually do something in Main depend on what Main
        // is showing — a Pile's own keys (`f`, `t`) mean nothing in a
        // Member's remedy menu and vice versa, so this mirrors `key_main`'s
        // own match on `w.context` rather than giving every context the
        // same generic "↑↓ scroll" hint. `^Q quit` is the one key that
        // works everywhere regardless of context, so every arm advertises
        // it — consistent with `Console`/`Browser`/`File` above, none of
        // which drop it either.
        Focus::Main => match &w.context {
            Context::Pile { .. } => "↑↓ member · enter open · f refit · t edit target · ^Q quit",
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
