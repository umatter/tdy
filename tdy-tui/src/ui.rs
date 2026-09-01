//! Drawing. Reads [`App`], changes nothing.
//!
//! Every screen is a function of the state, which is what makes the whole UI
//! testable against a `TestBackend`: render into a 100x30 buffer, read the
//! text back, assert on what a person would see.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};
use ratatui::Frame;

use tdy::report::MemberStatus;

use crate::app::{App, Screen};
use crate::evidence::Evidence;

const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
            .areas(f.area());

    draw_header(f, header, app);
    match app.screen {
        Screen::Pile => draw_pile(f, body, app),
        Screen::Member => draw_member(f, body, app),
        Screen::Accept => draw_accept(f, body, app),
        Screen::Confirm => draw_confirm(f, body, app),
        Screen::Query => draw_query(f, body, app),
        Screen::Help => draw_help(f, body),
    }
    draw_status(f, status, app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let name = app.report.as_ref().map(|r| r.target.clone()).unwrap_or_default();
    let counts = app
        .report
        .as_ref()
        .map(|r| {
            let (fits, review, refused) = crate::app::counts(r);
            format!("{fits} fit · {review} review · {refused} refused")
        })
        .unwrap_or_default();
    let left = format!(" {} — {}", app.target_path.display(), name);
    let line = Line::from(vec![
        Span::styled(left, Style::new().bold()),
        Span::raw("  "),
        Span::styled(counts, Style::new().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn status_word(s: MemberStatus) -> (&'static str, Color) {
    match s {
        MemberStatus::Fits => ("fits", Color::Green),
        MemberStatus::NeedsReview => ("REVIEW", Color::Yellow),
        MemberStatus::Gaps => ("REFUSED", Color::Red),
        MemberStatus::Contradicts => ("CONTRADICTS", Color::Red),
        MemberStatus::Error => ("ERROR", Color::Red),
    }
}

fn draw_pile(f: &mut Frame, area: Rect, app: &mut App) {
    let members = app.members();
    if members.is_empty() {
        let msg = match &app.busy {
            Some(what) => format!("{what}…"),
            None => "no members".into(),
        };
        f.render_widget(
            Paragraph::new(msg).block(Block::bordered().title(" pile ")),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = members
        .iter()
        .map(|m| {
            let (word, colour) = status_word(m.status);
            // The one-line summary is the *reason*, not a repetition of the
            // status: a REFUSED row that only says REFUSED makes the reader
            // press enter to learn anything.
            let detail = match m.status {
                MemberStatus::Fits | MemberStatus::NeedsReview => m
                    .review
                    .clone()
                    .unwrap_or_else(|| {
                        m.sources
                            .iter()
                            .map(|s| format!("{}←{}", s.column, s.source))
                            .collect::<Vec<_>>()
                            .join("  ")
                    }),
                _ => m
                    .problems
                    .first()
                    .map(|p| first_line(&p.message))
                    .unwrap_or_default(),
            };
            let accepted = if m.accepted { " ✓" } else { "" };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<26}", m.path)),
                Span::styled(format!("{word:<12}"), Style::new().fg(colour)),
                Span::styled(accepted.to_string(), Style::new().fg(Color::Green)),
                Span::styled(truncate(&detail, area.width.saturating_sub(46) as usize), Style::new().fg(DIM)),
            ]))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(app.selected));
    let list = List::new(items)
        .block(Block::bordered().title(" pile "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(&list, area, &mut state);
}

fn draw_member(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(m) = app.selected_member() else { return };
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);

    // Left: what is wrong, and the remedies for it.
    let mut lines: Vec<Line> = Vec::new();
    let (word, colour) = status_word(m.status);
    lines.push(Line::from(Span::styled(word, Style::new().fg(colour).bold())));
    if let Some(r) = &m.review {
        lines.push(Line::raw(""));
        for l in wrap_text(r, left.width.saturating_sub(4) as usize) {
            lines.push(Line::from(Span::styled(l, Style::new().fg(Color::Yellow))));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "[a] read what accepting this would do",
            Style::new().bold(),
        )));
    }
    for p in &m.problems {
        lines.push(Line::raw(""));
        for l in p.message.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }

    let remedies = app.remedies();
    if !remedies.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("remedies", Style::new().bold())));
        for (i, r) in remedies.iter().enumerate() {
            let marker = if i == app.remedy_selected { "▸" } else { " " };
            lines.push(Line::from(Span::raw(format!(
                "{marker} [{}] {}",
                i + 1,
                r.label()
            ))));
        }
    }

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" {} ", m.path))),
        left,
    );

    // Right: the file, as tdy sees it after the frame's transforms — the
    // answer to "no column of this file binds" is usually just to look.
    let preview = app.preview.clone().unwrap_or_default();
    let widget: Paragraph = if preview.rows.is_empty() {
        Paragraph::new(Span::styled("reading…", Style::new().fg(DIM)).to_string())
    } else {
        let header = Row::new(preview.header.clone())
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        let widths: Vec<Constraint> =
            preview.header.iter().map(|_| Constraint::Fill(1)).collect();
        let rows: Vec<Row> = preview.rows.iter().map(|r| Row::new(r.clone())).collect();
        f.render_widget(
            Table::new(rows, widths)
                .header(header)
                .column_spacing(1)
                .block(Block::bordered().title(" what tdy sees ")),
            right,
        );
        return;
    };
    f.render_widget(widget.block(Block::bordered().title(" what tdy sees ")), right);
}

fn draw_accept(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(m) = app.selected_member() else { return };
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        "This member fits mechanically. What it still needs is a judgement:",
        Style::new().fg(DIM),
    )));
    lines.push(Line::raw(""));
    for l in wrap_text(m.review.as_deref().unwrap_or(""), area.width.saturating_sub(4) as usize) {
        lines.push(Line::from(Span::styled(l, Style::new().fg(Color::Yellow))));
    }
    lines.push(Line::raw(""));

    match &app.evidence {
        None => lines.push(Line::from(Span::styled("reading the file…", Style::new().fg(DIM)))),
        Some(all) => {
            if all.len() > 1 {
                lines.push(Line::from(Span::styled(
                    format!("{} separate judgements, all of them accepted by this key:", all.len()),
                    Style::new().fg(Color::Yellow).bold(),
                )));
                lines.push(Line::raw(""));
            }
            for (i, e) in all.iter().enumerate() {
                if i > 0 {
                    lines.push(Line::raw(""));
                }
                lines.push(Line::from(Span::styled(e.headline(), Style::new().bold())));
                lines.push(Line::raw(""));
                lines.extend(evidence_lines(e));
            }
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Accepting records this judgement against these bytes. Editing the file or its \
         spec retracts it.",
        Style::new().fg(DIM),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("[a]", Style::new().fg(Color::Green).bold()),
        Span::raw(" accept this member    "),
        Span::styled("[esc]", Style::new().bold()),
        Span::raw(" back"),
    ]));

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" accept {} ? ", m.path))),
        area,
    );
}

fn evidence_lines(e: &Evidence) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    match e {
        Evidence::Shift { head, smallest, largest, source, .. } => {
            out.push(Line::from(Span::styled(
                format!("{:<10}  {:>18}  {:>18}", "row", format!("{source} reads"), "becomes"),
                Style::new().fg(Color::Cyan),
            )));
            for p in head {
                out.push(Line::raw(format!(
                    "{:<10}  {:>18}  {:>18}",
                    p.row, p.raw, p.parsed
                )));
            }
            out.push(Line::raw(""));
            out.push(Line::from(Span::styled(
                "the extremes, over every row of the file:",
                Style::new().fg(DIM),
            )));
            for (label, p) in
                [("smallest", smallest.as_ref()), ("largest", largest.as_ref())]
            {
                if let Some(p) = p {
                    out.push(Line::raw(format!(
                        "{label:<10}  {:>18}  {:>18}   (row {})",
                        p.raw, p.parsed, p.row
                    )));
                }
            }
        }
        Evidence::Constant { column, value, rows } => {
            out.push(Line::raw(format!("every one of the {rows} row(s) gets:")));
            out.push(Line::raw(""));
            out.push(Line::from(Span::styled(
                format!("    {column} = {value:?}"),
                Style::new().fg(Color::Yellow).bold(),
            )));
            out.push(Line::raw(""));
            out.push(Line::from(Span::styled(
                "The file does not contain this. You are asserting it.",
                Style::new().fg(DIM),
            )));
        }
        Evidence::Frame { header, head, .. } => {
            out.push(Line::from(Span::styled(header.join("   "), Style::new().fg(Color::Cyan))));
            for r in head {
                out.push(Line::raw(r.join("   ")));
            }
        }
        Evidence::Unillustrated { reason } => {
            out.push(Line::raw(reason.clone()));
        }
    }
    out
}

fn draw_confirm(f: &mut Frame, area: Rect, app: &mut App) {
    let Some((remedy, edit)) = &app.pending else { return };
    let mut lines = vec![
        Line::from(Span::styled(remedy.label(), Style::new().bold())),
        Line::raw(""),
        Line::from(Span::styled(
            format!("{}:", app.target_path.display()),
            Style::new().fg(DIM),
        )),
    ];
    for l in edit.diff().lines() {
        let style = if l.contains(" - ") || l.trim_start().starts_with(|c: char| c.is_ascii_digit())
            && l.contains(" - ")
        {
            Style::new().fg(Color::Red)
        } else if l.contains(" + ") {
            Style::new().fg(Color::Green)
        } else {
            Style::new()
        };
        lines.push(Line::from(Span::styled(l.to_string(), style)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("[y]", Style::new().fg(Color::Green).bold()),
        Span::raw(" write it and re-fit    "),
        Span::styled("[esc]", Style::new().bold()),
        Span::raw(" leave the target alone"),
    ]));

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" edit the declaration ")),
        area,
    );
}

fn draw_query(f: &mut Frame, area: Rect, app: &mut App) {
    let [input, result] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);
    f.render_widget(
        Paragraph::new(format!("> {}", app.query_input))
            .block(Block::bordered().title(" SQL ")),
        input,
    );

    match &app.query_result {
        None => f.render_widget(
            Paragraph::new(Span::styled(
                app.default_query(),
                Style::new().fg(DIM),
            ))
            .block(Block::bordered().title(" result ")),
            result,
        ),
        Some(r) => {
            let title = if r.truncated {
                format!(" result — showing {} of {} row(s) ", r.rows.len(), r.total)
            } else {
                format!(" result — {} row(s) ", r.total)
            };
            let widths: Vec<Constraint> =
                r.columns.iter().map(|_| Constraint::Fill(1)).collect();
            let rows: Vec<Row> = r.rows.iter().map(|row| Row::new(row.clone())).collect();
            f.render_widget(
                Table::new(rows, widths)
                    .header(
                        Row::new(r.columns.clone())
                            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    )
                    .column_spacing(1)
                    .block(Block::bordered().title(title)),
                result,
            );
        }
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("pile", Style::new().bold())),
        Line::raw("  ↑ ↓      move        enter  inspect a member"),
        Line::raw("  f        re-fit      t      edit the target in $EDITOR"),
        Line::raw("  tab      query       q      quit"),
        Line::raw(""),
        Line::from(Span::styled("member", Style::new().bold())),
        Line::raw("  1..9     apply a remedy (shows a diff first)"),
        Line::raw("  a        read what accepting this member would do"),
        Line::raw("  e        edit this member's sidecar in $EDITOR"),
        Line::raw("  esc      back"),
        Line::raw(""),
        Line::from(Span::styled("accept", Style::new().bold())),
        Line::raw("  a        accept THIS member (one at a time, by design)"),
        Line::raw(""),
        Line::from(Span::styled(
            "Nothing here writes without showing you what it writes.",
            Style::new().fg(DIM),
        )),
    ];
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(" keys ")),
        area,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let (text, style) = match &app.busy {
        Some(what) => (format!(" {what}…"), Style::new().fg(Color::Yellow)),
        None => (format!(" {}", app.status), Style::new().fg(DIM)),
    };
    let keys = match app.screen {
        Screen::Pile => "enter inspect · f re-fit · t target · tab query · ? keys · q quit",
        Screen::Member => "1..9 remedy · a accept · e sidecar · esc back",
        Screen::Accept => "a accept · esc back",
        Screen::Confirm => "y write · esc cancel",
        Screen::Query => "enter run · ↑ last · esc back",
        Screen::Help => "any key back",
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

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let one = s.replace('\n', " ");
    if one.chars().count() <= width {
        one
    } else {
        one.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Wrap on word boundaries. `Paragraph`'s own wrapping handles the body; this
/// exists for the lines that must be styled individually.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
