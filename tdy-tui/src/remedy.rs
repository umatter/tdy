//! Turning a gap into an edit of the target — textually, and proved.
//!
//! Two rules shape everything here.
//!
//! **The edit is textual, not a re-serialisation.** A target is written by a
//! human, reviewed in git, and full of comments explaining *why* a column is
//! declared the way it is. Parsing it and printing the AST back would land a
//! diff that deletes all of that, which is the fastest way to teach someone
//! never to let a tool touch their declaration. So each remedy finds the line
//! it means and changes that line.
//!
//! **The edit is proved before it is offered.** The result is re-parsed with
//! `Target::parse` and checked for the effect it claimed — an edit that
//! produced an unparseable target, or that silently did nothing, is refused
//! rather than written. The TUI shows the diff and the user confirms; this
//! module guarantees there is something real to confirm.

use anyhow::{anyhow, bail, Result};
use tdy::target::Target;

/// A one-line change to a target declaration, named the way the gap names it.
#[derive(Debug, Clone, PartialEq)]
pub enum Remedy {
    /// The file spells this column differently: teach the declaration that
    /// spelling. (`OPTIONS(matches = '…')`, appended or created.)
    AddMatch { column: String, spelling: String },
    /// This file genuinely lacks the column, and that is a fact about the
    /// file rather than a mistake: null-fill it. Implies dropping NOT NULL,
    /// since `if_missing` on a NOT NULL column is a contradiction the target
    /// parser refuses.
    IfMissingNull { column: String },
    /// This file does not belong in the dataset.
    ExcludeFile { rel: String },
}

impl Remedy {
    /// A short label for a menu.
    pub fn label(&self) -> String {
        match self {
            Remedy::AddMatch { column, spelling } => {
                format!("teach `{column}` the spelling {spelling:?}")
            }
            Remedy::IfMissingNull { column } => {
                format!("declare `{column}` absent-and-null in files that lack it")
            }
            Remedy::ExcludeFile { rel } => format!("exclude {rel:?} from the dataset"),
        }
    }
}

/// A proposed edit: the whole new text, and the lines that changed.
#[derive(Debug, Clone)]
pub struct Edit {
    pub new_text: String,
    /// (line number, before, after) — `before` empty means an inserted line.
    pub changed: Vec<(usize, String, String)>,
}

impl Edit {
    /// A unified-ish diff for display. Deliberately tiny: these edits are one
    /// or two lines, and a real diff library would be a dependency for the
    /// sake of a feature nobody asked for.
    pub fn diff(&self) -> String {
        let mut out = String::new();
        for (n, before, after) in &self.changed {
            if !before.is_empty() {
                out.push_str(&format!("{:>4} - {}\n", n + 1, before));
            }
            out.push_str(&format!("{:>4} + {}\n", n + 1, after));
        }
        out
    }
}

/// Apply a remedy to a target's source text.
///
/// Returns the new text and the changed lines, or an error naming why the
/// edit could not be made — a column line that could not be found, an edit
/// that did not parse, or one that parsed but did not take effect.
pub fn apply(sql: &str, remedy: &Remedy) -> Result<Edit> {
    let edit = match remedy {
        Remedy::AddMatch { column, spelling } => add_match(sql, column, spelling)?,
        Remedy::IfMissingNull { column } => if_missing_null(sql, column)?,
        Remedy::ExcludeFile { rel } => exclude_file(sql, rel)?,
    };

    // Proved, not hoped, and in both directions: the edit must still be a
    // target, must have done what it said, AND must have done nothing else.
    //
    // The second half is the one that matters. Every bug this module has had
    // was an edit that took effect *and* changed something else on its way —
    // a `NOT NULL` deleted out of a quoted alias, spaces collapsed inside a
    // string, a second OPTIONS clause inverting the alias order somebody
    // wrote. Checking only the intended effect passes all of those. So the
    // whole declaration is compared, field by field, and anything unexplained
    // refuses the edit rather than offering it for confirmation.
    let before = Target::parse(sql)
        .map_err(|e| anyhow!("this target does not parse to begin with: {e:#}"))?;
    let after = Target::parse(&edit.new_text)
        .map_err(|e| anyhow!("that edit does not leave a valid target: {e:#}"))?;
    took_effect(&after, remedy)?;
    nothing_else_changed(&before, &after, remedy)?;
    Ok(edit)
}

fn took_effect(after: &Target, remedy: &Remedy) -> Result<()> {
    match remedy {
        Remedy::AddMatch { column, spelling } => {
            let c = column_of(after, column)?;
            if !c.matches.iter().any(|m| m == spelling) {
                bail!("the edit parsed but `{column}` still does not match {spelling:?}");
            }
        }
        Remedy::IfMissingNull { column } => {
            if !column_of(after, column)?.if_missing_null {
                bail!("the edit parsed but `{column}` is still required");
            }
        }
        Remedy::ExcludeFile { rel } => {
            if !after.exclude.iter().any(|e| e == rel) {
                bail!("the edit parsed but {rel:?} is still not excluded");
            }
        }
    }
    Ok(())
}

fn column_of<'a>(t: &'a Target, name: &str) -> Result<&'a crate::TargetColumn> {
    t.columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("column `{name}` is not declared in this target"))
}

/// Everything the declaration says, as comparable text — one string per
/// column plus one for the table's options.
fn shape(t: &Target) -> Vec<String> {
    let mut out = vec![format!(
        "table {} files={:?} exclude={:?} match={:?} date_order={:?} verify={:?} \
         tz={:?} dec={:?}",
        t.name, t.files, t.exclude, t.match_mode, t.date_order, t.verify, t.timezone,
        t.decimal_separator
    )];
    for c in &t.columns {
        out.push(format!(
            "column {} {:?} nullable={} if_missing_null={} matches={:?}",
            c.name, c.dtype, c.nullable, c.if_missing_null, c.matches
        ));
    }
    out
}

fn nothing_else_changed(before: &Target, after: &Target, remedy: &Remedy) -> Result<()> {
    // Apply the remedy to a *copy of the meaning* and require the result to
    // equal what the text edit produced. Anything else the edit did shows up
    // here as a mismatched line.
    let mut expect = before.clone();
    match remedy {
        Remedy::AddMatch { column, spelling } => {
            column_of_mut(&mut expect, column)?.matches.push(spelling.clone());
        }
        Remedy::IfMissingNull { column } => {
            let c = column_of_mut(&mut expect, column)?;
            c.if_missing_null = true;
            // The remedy drops NOT NULL in the same edit, deliberately and
            // visibly — the two cannot coexist.
            c.nullable = true;
        }
        Remedy::ExcludeFile { rel } => expect.exclude.push(rel.clone()),
    }

    let (want, got) = (shape(&expect), shape(after));
    if want == got {
        return Ok(());
    }
    let mismatch = want
        .iter()
        .zip(got.iter())
        .find(|(a, b)| a != b)
        .map(|(a, b)| format!("\n  expected: {a}\n  but got:  {b}"))
        .unwrap_or_else(|| format!("\n  the target went from {} to {} part(s)", want.len(), got.len()));
    bail!(
        "refusing this edit: it would change more than it says.{mismatch}\n  \
         Edit {:?} by hand instead.",
        "the target"
    )
}

fn column_of_mut<'a>(t: &'a mut Target, name: &str) -> Result<&'a mut crate::TargetColumn> {
    t.columns
        .iter_mut()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("column `{name}` is not declared in this target"))
}

/// Where a line's *code* is — everything that is not inside a single-quoted
/// string and not after a `--` that itself sits outside one.
///
/// Every search in this module goes through here. Searching the raw line
/// finds `NOT NULL` inside a comment and `matches` inside somebody's alias,
/// and an edit made on either is a silent corruption of a file the user
/// wrote by hand.
fn code_spans(line: &str) -> Vec<std::ops::Range<usize>> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_str = false;
    while i < b.len() {
        if in_str {
            if b[i] == b'\'' {
                in_str = false;
                start = i + 1;
            }
            i += 1;
            continue;
        }
        if b[i] == b'\'' {
            out.push(start..i);
            in_str = true;
            i += 1;
            continue;
        }
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            out.push(start..i);
            return out;
        }
        i += 1;
    }
    if !in_str {
        out.push(start..b.len());
    }
    out
}

/// Case-insensitive search restricted to the line's code.
fn find_in_code(line: &str, needle: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    code_spans(line).into_iter().find_map(|r| {
        lower.get(r.clone())?.find(&needle).map(|off| r.start + off)
    })
}

/// Where the trailing comment starts, if the line has one.
fn comment_at(line: &str) -> Option<usize> {
    let spans = code_spans(line);
    let end = spans.last().map(|r| r.end)?;
    (end < line.len() && line[end..].starts_with("--")).then_some(end)
}

/// The line declaring `column`, inside the CREATE TABLE column list.
///
/// Matched on the line's first identifier, which is what a column declaration
/// starts with. Deliberately not a regex over the whole file: `region` also
/// appears in `OPTIONS(matches = 'Region')` and in comments, and editing
/// either of those would be a silent corruption.
fn column_line(sql: &str, column: &str) -> Result<usize> {
    let end = column_list_end(sql);
    for (i, line) in lines_with_endings(sql).iter().map(|l| strip_end(l)).enumerate().take(end) {
        let t = line.trim_start();
        if t.starts_with("--") {
            continue;
        }
        let first: String =
            t.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '"').collect();
        if first.trim_matches('"').eq_ignore_ascii_case(column) {
            return Ok(i);
        }
    }
    // The loop above matches a line's FIRST identifier. Before declaring
    // the column absent, check whether it merely shares a line with other
    // declarations (a hand-minified target): remedies edit one column per
    // line by design (see the module doc — splicing inside a shared line
    // risks corrupting its neighbors), so the honest error names the
    // reformat, not a phantom missing column.
    let in_list = lines_with_endings(sql)
        .iter()
        .map(|l| strip_end(l))
        .take(end)
        .any(|line| {
            line.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '"'))
                .any(|w| w.trim_matches('"').eq_ignore_ascii_case(column))
        });
    if in_list {
        bail!(
            "`{column}` shares a line with other declarations — remedies edit one column \
             per line; reformat the target (one column per line, as `tdy draft` writes it) \
             or edit it by hand"
        );
    }
    bail!("no line in this target declares `{column}`")
}

/// Where the column list ends — the `)` that closes CREATE TABLE, which is
/// also where `WITH (` begins. Searching past it would find the option names.
fn column_list_end(sql: &str) -> usize {
    lines_with_endings(sql)
        .iter()
        .position(|l| {
            let t = strip_end(l).trim_start();
            if t.starts_with(')') {
                return true;
            }
            // The WITH *keyword*, not any identifier starting with those
            // four letters: a column called `with_tax` would otherwise end
            // the column list early and make every remedy below it
            // unavailable, with no message saying why.
            let word: String =
                t.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            word.eq_ignore_ascii_case("with")
        })
        .unwrap_or(usize::MAX)
}

/// Split into lines *keeping* each one's terminator, so an edit cannot
/// silently convert a CRLF target to LF — a whole-file rewrite that the
/// one-line diff would not show.
fn lines_with_endings(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = sql;
    while let Some(i) = rest.find('\n') {
        out.push(rest[..=i].to_string());
        rest = &rest[i + 1..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

fn strip_end(s: &str) -> &str {
    s.strip_suffix('\n').map(|s| s.strip_suffix('\r').unwrap_or(s)).unwrap_or(s)
}

fn ending_of(s: &str) -> &str {
    if s.ends_with("\r\n") {
        "\r\n"
    } else if s.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn replace_line(sql: &str, at: usize, new: String) -> Edit {
    let mut lines = lines_with_endings(sql);
    let before = strip_end(&lines[at]).to_string();
    let ending = ending_of(&lines[at]).to_string();
    lines[at] = format!("{new}{ending}");
    Edit { new_text: lines.concat(), changed: vec![(at, before, new)] }
}

fn add_match(sql: &str, column: &str, spelling: &str) -> Result<Edit> {
    if spelling.contains(',') || spelling.contains('\'') || spelling.contains(['\n', '\r']) {
        // `matches` is a comma-separated list inside a single-quoted string
        // on one line. A spelling carrying a comma or a quote would silently
        // mean something else; one carrying a newline would splice a line
        // into the file that the diff does not show.
        bail!(
            "the header {spelling:?} contains a comma, a quote or a newline, which a \
             `matches` list cannot express — bind it by hand in the sidecar instead"
        );
    }
    let at = column_line(sql, column)?;
    let line = sql.lines().nth(at).expect("line exists");

    let new = match find_matches_option(line) {
        // Extend the existing list, keeping the user's order: an alias list
        // is a statement of preference and the new spelling is the least
        // preferred thing about it.
        Some((start, end, existing)) => {
            let mut merged = existing;
            merged.push_str(", ");
            merged.push_str(spelling);
            format!("{}{}{}", &line[..start], merged, &line[end..])
        }
        None => {
            // No OPTIONS yet: append one before the trailing comma, if any.
            let (body, tail) = split_trailing_comma(line);
            format!("{body} OPTIONS(matches = '{spelling}'){tail}")
        }
    };
    Ok(replace_line(sql, at, new))
}

/// The span of the value inside `matches = '…'` on this line, plus its text.
///
/// `matches` is looked for in the line's *code*: a file whose header is
/// literally `matches` would otherwise have its own alias treated as the
/// option keyword.
fn find_matches_option(line: &str) -> Option<(usize, usize, String)> {
    let key = find_in_code(line, "matches")?;
    let eq = line[key..].find('=')? + key;
    let open = line[eq..].find('\'')? + eq + 1;
    let close = line[open..].find('\'')? + open;
    Some((open, close, line[open..close].to_string()))
}

/// Split a declaration line into its body and any trailing `,` (with the
/// comment that may follow it), so an option can be appended in between.
fn split_trailing_comma(line: &str) -> (&str, &str) {
    let code_end = comment_at(line).unwrap_or(line.len());
    let code = &line[..code_end];
    match code.trim_end().strip_suffix(',') {
        Some(_) => {
            let at = code.rfind(',').expect("just found it");
            (&line[..at], &line[at..])
        }
        None => (code.trim_end(), &line[code_end..]),
    }
}

fn if_missing_null(sql: &str, column: &str) -> Result<Edit> {
    let at = column_line(sql, column)?;
    let line = sql.lines().nth(at).expect("line exists");
    if line.to_ascii_lowercase().contains("if_missing") {
        bail!("`{column}` already declares if_missing");
    }

    // NOT NULL and if_missing = 'null' contradict each other, and the target
    // parser says so. Dropping NOT NULL here is part of the same decision —
    // shown in the diff, so it is not a hidden consequence.
    let code_end = comment_at(line).unwrap_or(line.len());
    let comment = &line[code_end..];
    let code = &line[..code_end];
    // Only a NOT NULL that is really *in the declaration*: one inside a
    // quoted alias (`matches = 'not null'`) or in a comment is somebody's
    // text, and deleting it would silently respell their file.
    let without_not_null = match find_in_code(code, "not null") {
        Some(i) => {
            let mut s = String::from(&code[..i]);
            s.push_str(&code[i + "not null".len()..]);
            // Close only the gap the removal opened — squeezing the whole
            // line would collapse the spaces inside every quoted alias on it.
            while s[..i].ends_with("  ") {
                s.remove(i - 1);
            }
            s
        }
        None => code.to_string(),
    };

    let new_code = match find_matches_option(&without_not_null) {
        Some((_, close, _)) => {
            // Add the option beside the existing one, inside OPTIONS(...).
            let after_quote = close + 1;
            let rest = &without_not_null[after_quote..];
            let paren = rest
                .find(')')
                .ok_or_else(|| anyhow!("malformed OPTIONS on the `{column}` line"))?;
            format!(
                "{}{}, if_missing = 'null'{}",
                &without_not_null[..after_quote],
                &rest[..paren],
                &rest[paren..]
            )
        }
        None => {
            let (body, tail) = split_trailing_comma(&without_not_null);
            format!("{body} OPTIONS(if_missing = 'null'){tail}")
        }
    };
    Ok(replace_line(sql, at, format!("{}{comment}", new_code.trim_end())))
}

fn exclude_file(sql: &str, rel: &str) -> Result<Edit> {
    if rel.contains(',') || rel.contains('\'') {
        bail!("{rel:?} contains a comma or a quote, which an `exclude` list cannot express");
    }
    let owned = lines_with_endings(sql);
    let lines: Vec<&str> = owned.iter().map(|l| strip_end(l)).collect();

    // Already excluded anywhere? A target may carry several `exclude`
    // options (they accumulate), so checking only the first would let the
    // same file be listed twice.
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if lower.trim_start().starts_with("exclude") && lower.contains('=') {
            if let Some((open, close)) = quoted_span(line) {
                if line[open..close].split(',').any(|e| e.trim() == rel) {
                    bail!("{rel:?} is already excluded");
                }
            }
        }
    }

    // Extend an existing exclude= if there is one.
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        if lower.trim_start().starts_with("exclude") && lower.contains('=') {
            let open = line.find('\'').ok_or_else(|| anyhow!("malformed exclude option"))?;
            let close = line[open + 1..]
                .find('\'')
                .ok_or_else(|| anyhow!("malformed exclude option"))?
                + open
                + 1;
            let new = format!("{}, {rel}{}", &line[..close], &line[close..]);
            return Ok(replace_line(sql, i, new));
        }
    }

    // Otherwise add one directly under files=, where a reader looks for it.
    let files_at = lines
        .iter()
        .position(|l| {
            let t = l.trim_start().to_ascii_lowercase();
            t.starts_with("files") && t.contains('=')
        })
        .ok_or_else(|| anyhow!("this target has no `files` option to exclude from"))?;
    let indent: String =
        lines[files_at].chars().take_while(|c| c.is_whitespace()).collect();
    let files_line = lines[files_at];
    // Commas move with the insertion. If `files` already ended with one,
    // options follow it and the NEW line is now the one in the middle, so it
    // takes the comma; if it did not, `files` was last and now is not.
    // Getting this backwards produced a target that no longer parsed — which
    // `apply`'s re-parse caught, but a remedy that can only ever fail is not
    // a remedy.
    let code_end = comment_at(files_line).unwrap_or(files_line.len());
    let files_had_comma = files_line[..code_end].trim_end().ends_with(',');
    let (with_comma, added_comma) = if files_had_comma {
        (files_line.to_string(), false)
    } else {
        let code = &files_line[..code_end];
        (format!("{},{}", code.trim_end(), &files_line[code_end..]), true)
    };
    let inserted = if files_had_comma {
        format!("{indent}exclude = '{rel}',")
    } else {
        format!("{indent}exclude = '{rel}'")
    };

    let ending = {
        let e = ending_of(&owned[files_at]);
        if e.is_empty() { "\n" } else { e }
    };
    let mut out = owned.clone();
    let mut changed = Vec::new();
    if added_comma {
        changed.push((files_at, files_line.to_string(), with_comma.clone()));
        out[files_at] = format!("{with_comma}{}", ending_of(&owned[files_at]));
    }
    out.insert(files_at + 1, format!("{inserted}{ending}"));
    changed.push((files_at + 1, String::new(), inserted));
    Ok(Edit { new_text: out.concat(), changed })
}

/// The span between the first pair of single quotes on a line.
fn quoted_span(line: &str) -> Option<(usize, usize)> {
    let open = line.find('\'')? + 1;
    let close = line[open..].find('\'')? + open;
    Some((open, close))
}

/// The remedies that apply to one structured problem from a `PileReport`.
///
/// Driven by the problem's `kind`, so the menu can never offer a fix for a
/// situation the planner did not report.
pub fn remedies_for(problem: &serde_json::Value, member_path: &str) -> Vec<Remedy> {
    let kind = problem["kind"].as_str().unwrap_or("");
    let column = problem["column"].as_str().unwrap_or("").to_string();
    let mut out = Vec::new();
    if kind == "no_candidate" && !column.is_empty() {
        for h in problem["header"].as_array().into_iter().flatten() {
            if let Some(s) = h.as_str() {
                out.push(Remedy::AddMatch { column: column.clone(), spelling: s.to_string() });
            }
        }
        out.push(Remedy::IfMissingNull { column });
    }
    out.push(Remedy::ExcludeFile { rel: member_path.to_string() });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "-- Monthly sales, reviewed in git.\n\
                     CREATE TABLE sales (\n\
                     \x20 month      DATE          NOT NULL OPTIONS(matches = 'Datum'),\n\
                     \x20 region     TEXT          NOT NULL,\n\
                     \x20 amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')\n\
                     )\n\
                     WITH (\n\
                     \x20 files      = '2025-*.csv',\n\
                     \x20 date_order = 'dmy'\n\
                     );\n";

    /// The comments and the formatting of every untouched line survive: a
    /// tool that reformatted the declaration would be one nobody lets near it.
    #[test]
    fn an_edit_touches_one_line_and_leaves_the_rest_byte_identical() {
        let e = apply(
            T,
            &Remedy::AddMatch { column: "region".into(), spelling: "Kanton".into() },
        )
        .unwrap();
        assert_eq!(e.changed.len(), 1);
        let before: Vec<&str> = T.lines().collect();
        let after: Vec<&str> = e.new_text.lines().collect();
        assert_eq!(before.len(), after.len());
        for (i, (b, a)) in before.iter().zip(&after).enumerate() {
            if i == e.changed[0].0 {
                assert_ne!(b, a);
            } else {
                assert_eq!(b, a, "line {i} was rewritten and should not have been");
            }
        }
        assert!(after[0].starts_with("-- Monthly sales"), "the comment survived");
        assert!(e.new_text.contains("OPTIONS(matches = 'Kanton')"), "{}", e.new_text);
    }

    /// An existing alias list is extended, not replaced — the user's order is
    /// their preference and the new spelling is the least preferred.
    #[test]
    fn add_match_extends_an_existing_list_in_order() {
        let e = apply(
            T,
            &Remedy::AddMatch { column: "month".into(), spelling: "Buchungsdatum".into() },
        )
        .unwrap();
        assert!(e.new_text.contains("matches = 'Datum, Buchungsdatum'"), "{}", e.new_text);
        let t = Target::parse(&e.new_text).unwrap();
        let m = t.columns.iter().find(|c| c.name == "month").unwrap();
        assert_eq!(m.matches, vec!["Datum", "Buchungsdatum"]);
    }

    /// `if_missing = 'null'` contradicts NOT NULL, and the target parser
    /// refuses that pair — so the remedy drops NOT NULL in the same edit,
    /// visibly, rather than producing something that will not parse.
    #[test]
    fn if_missing_null_drops_not_null_in_the_same_visible_edit() {
        let e = apply(T, &Remedy::IfMissingNull { column: "region".into() }).unwrap();
        let line = &e.changed[0].2;
        assert!(!line.to_ascii_lowercase().contains("not null"), "{line}");
        assert!(line.contains("if_missing = 'null'"), "{line}");
        // The user's column alignment survives: only the gap the removal
        // opened is closed. A remedy that reflowed the line would put noise
        // in a diff people read.
        assert!(line.starts_with("  region     TEXT"), "alignment was disturbed: {line}");
        let t = Target::parse(&e.new_text).unwrap();
        let c = t.columns.iter().find(|c| c.name == "region").unwrap();
        assert!(c.if_missing_null && c.nullable);
    }

    /// The same, on a column that already has OPTIONS: the new option joins
    /// the existing ones instead of opening a second OPTIONS(...).
    #[test]
    fn if_missing_null_joins_an_existing_options_list() {
        let e = apply(T, &Remedy::IfMissingNull { column: "amount_chf".into() }).unwrap();
        // Still two OPTIONS( in the file — the new option joined the one that
        // was already on this line rather than opening a second.
        assert_eq!(e.new_text.matches("OPTIONS(").count(), 2, "{}", e.new_text);
        assert!(
            e.new_text.contains("OPTIONS(matches = 'Betrag', if_missing = 'null')"),
            "{}",
            e.new_text
        );
        let t = Target::parse(&e.new_text).unwrap();
        let c = t.columns.iter().find(|c| c.name == "amount_chf").unwrap();
        assert!(c.if_missing_null);
        assert_eq!(c.matches, vec!["Betrag"]);
    }

    /// A target with no exclude gets one, right under `files`, and the comma
    /// that the previous option now needs.
    #[test]
    fn exclude_is_inserted_under_files_with_the_comma_it_needs() {
        let e = apply(T, &Remedy::ExcludeFile { rel: "2025-07.csv".into() }).unwrap();
        let t = Target::parse(&e.new_text).unwrap();
        assert_eq!(t.exclude, vec!["2025-07.csv"]);
        assert_eq!(t.files, vec!["2025-*.csv"]);
        let lines: Vec<&str> = e.new_text.lines().collect();
        let f = lines.iter().position(|l| l.contains("files")).unwrap();
        assert!(lines[f + 1].contains("exclude"), "inserted in the wrong place");
    }

    /// …and an existing exclude list is extended.
    #[test]
    fn exclude_extends_an_existing_list() {
        let first = apply(T, &Remedy::ExcludeFile { rel: "2025-07.csv".into() }).unwrap();
        let second =
            apply(&first.new_text, &Remedy::ExcludeFile { rel: "2025-08.csv".into() }).unwrap();
        let t = Target::parse(&second.new_text).unwrap();
        assert_eq!(t.exclude, vec!["2025-07.csv", "2025-08.csv"]);
        assert_eq!(second.changed.len(), 1, "extending is one line");
    }

    /// Every remedy is proved before it is offered: an edit that would not
    /// parse, or that would not take effect, is an error rather than a diff
    /// the user is invited to accept.
    #[test]
    fn an_edit_that_would_not_take_effect_is_refused() {
        // A column the target does not declare.
        let e = apply(T, &Remedy::AddMatch { column: "gebiet".into(), spelling: "X".into() });
        assert!(e.unwrap_err().to_string().contains("gebiet"));

        // A spelling a matches= list cannot express.
        let e = apply(
            T,
            &Remedy::AddMatch { column: "region".into(), spelling: "Kanton, Ort".into() },
        );
        assert!(e.unwrap_err().to_string().contains("comma"));

        // Excluding twice.
        let once = apply(T, &Remedy::ExcludeFile { rel: "2025-07.csv".into() }).unwrap();
        let twice = apply(&once.new_text, &Remedy::ExcludeFile { rel: "2025-07.csv".into() });
        assert!(twice.unwrap_err().to_string().contains("already"));
    }

    // ---------------------------------------------------------------
    // Regressions. Every one of these was a real edit that took effect
    // *and* changed something else on its way — which is why `apply` now
    // compares the whole declaration before and after, not just the part
    // the remedy claimed.
    // ---------------------------------------------------------------

    /// A `NOT NULL` living inside somebody's alias is their text, not the
    /// declaration's nullability. Deleting it respelled their file.
    #[test]
    fn not_null_inside_a_quoted_alias_is_not_the_declaration() {
        let t = "CREATE TABLE t (\n\
                 \x20 flag TEXT NOT NULL OPTIONS(matches = 'not null flag, ok')\n\
                 )\nWITH (files = '*.csv');\n";
        let e = apply(t, &Remedy::IfMissingNull { column: "flag".into() }).unwrap();
        let after = Target::parse(&e.new_text).unwrap();
        let c = after.columns.iter().find(|c| c.name == "flag").unwrap();
        assert_eq!(c.matches, vec!["not null flag", "ok"], "an alias was respelled");
        assert!(c.if_missing_null && c.nullable);
    }

    /// The same, for spaces: squeezing the whole line collapsed the spaces
    /// inside every quoted alias on it.
    #[test]
    fn spaces_inside_a_quoted_alias_survive_the_edit() {
        let t = "CREATE TABLE t (\n\
                 \x20 amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag  CHF, Total  Sum')\n\
                 )\nWITH (files = '*.csv');\n";
        let e = apply(t, &Remedy::IfMissingNull { column: "amount".into() }).unwrap();
        let after = Target::parse(&e.new_text).unwrap();
        assert_eq!(
            after.columns[0].matches,
            vec!["Betrag  CHF", "Total  Sum"],
            "the double spaces inside the aliases were collapsed"
        );
    }

    /// A comment saying "NOT NULL" is prose. So is a `--` inside a string.
    #[test]
    fn a_comment_is_not_part_of_the_declaration() {
        let t = "CREATE TABLE t (\n\
                 \x20 region TEXT NOT NULL,  -- was NOT NULL before 2024\n\
                 \x20 note   TEXT OPTIONS(matches = 'a--b')\n\
                 )\nWITH (files = '*.csv');\n";
        let e = apply(t, &Remedy::IfMissingNull { column: "region".into() }).unwrap();
        assert!(
            e.changed[0].2.contains("-- was NOT NULL before 2024"),
            "the comment was edited: {}",
            e.changed[0].2
        );
        let after = Target::parse(&e.new_text).unwrap();
        assert_eq!(after.columns[1].matches, vec!["a--b"], "a `--` in a string ended the line");
    }

    /// A CRLF target stays CRLF. Rewriting every line ending is a whole-file
    /// change that the one-line diff would not show.
    #[test]
    fn line_endings_are_preserved() {
        let t = "CREATE TABLE t (\r\n  region TEXT\r\n)\r\nWITH (files = '*.csv');\r\n";
        let e = apply(t, &Remedy::AddMatch { column: "region".into(), spelling: "Kanton".into() })
            .unwrap();
        assert_eq!(
            e.new_text.matches("\r\n").count(),
            t.matches("\r\n").count(),
            "line endings changed:\n{:?}",
            e.new_text
        );
        assert!(!e.new_text.contains("\n\n"));
    }

    /// A declaration spread over two lines cannot be edited safely by
    /// touching one of them — the second OPTIONS would invert the alias
    /// order the user wrote — so the whole-declaration check refuses it
    /// instead of producing a plausible wrong file.
    #[test]
    fn a_multi_line_declaration_is_refused_rather_than_half_edited() {
        let t = "CREATE TABLE t (\n\
                 \x20 region TEXT\n\
                 \x20   OPTIONS(matches = 'Kanton')\n\
                 )\nWITH (files = '*.csv');\n";
        let e = apply(t, &Remedy::AddMatch { column: "region".into(), spelling: "Gebiet".into() });
        match e {
            // Either it refuses…
            Err(err) => {
                let m = format!("{err:#}");
                assert!(
                    m.contains("more than it says") || m.contains("does not leave a valid"),
                    "{m}"
                );
            }
            // …or it genuinely did the right thing, in which case the order
            // the user wrote must survive.
            Ok(edit) => {
                let after = Target::parse(&edit.new_text).unwrap();
                assert_eq!(after.columns[0].matches, vec!["Kanton", "Gebiet"]);
            }
        }
    }

    /// A file already excluded by a *second* exclude option is still already
    /// excluded.
    #[test]
    fn a_second_exclude_option_is_still_an_exclusion() {
        let t = "CREATE TABLE t (region TEXT)\n\
                 WITH (\n\
                 \x20 files   = '*.csv',\n\
                 \x20 exclude = 'a.csv',\n\
                 \x20 exclude = 'b.csv'\n\
                 );\n";
        let e = apply(t, &Remedy::ExcludeFile { rel: "b.csv".into() });
        assert!(e.unwrap_err().to_string().contains("already"), "b.csv was excluded twice");
    }

    /// A header cell carrying a newline cannot go into a single-quoted
    /// option, and must not be spliced into the file.
    #[test]
    fn a_header_with_a_newline_is_refused() {
        let t = "CREATE TABLE t (\n  region TEXT\n)\nWITH (files = '*.csv');\n";
        let e = apply(
            t,
            &Remedy::AddMatch { column: "region".into(), spelling: "Kan\nton".into() },
        );
        assert!(e.is_err(), "a newline was spliced into the target");
    }

    /// A column whose name merely *starts with* `with` does not end the
    /// column list. It used to, silently, making every remedy below it
    /// unavailable with no message saying why.
    #[test]
    fn a_column_named_with_something_does_not_end_the_column_list() {
        let t = "CREATE TABLE t (\n\
                 \x20 with_tax BOOLEAN,\n\
                 \x20 region   TEXT\n\
                 )\nWITH (files = '*.csv');\n";
        let e = apply(t, &Remedy::AddMatch { column: "region".into(), spelling: "Kanton".into() })
            .expect("`region` is declared below `with_tax` and must still be reachable");
        assert_eq!(e.changed[0].0, 2, "{}", e.diff());
    }

    /// A column name appearing in a comment or inside a matches= list is not
    /// a declaration, and must never be the line that gets edited.
    #[test]
    fn a_name_in_a_comment_or_an_alias_is_not_the_declaration() {
        let tricky = "-- region is tricky: some files call it Kanton\n\
                      CREATE TABLE t (\n\
                      \x20 area   TEXT OPTIONS(matches = 'region'),\n\
                      \x20 region TEXT\n\
                      )\n\
                      WITH (files = '*.csv');\n";
        let e = apply(
            tricky,
            &Remedy::AddMatch { column: "region".into(), spelling: "Gebiet".into() },
        )
        .unwrap();
        assert_eq!(e.changed[0].0, 3, "edited the wrong line: {}", e.diff());
        let t = Target::parse(&e.new_text).unwrap();
        assert_eq!(t.columns.iter().find(|c| c.name == "area").unwrap().matches, vec!["region"]);
        assert_eq!(
            t.columns.iter().find(|c| c.name == "region").unwrap().matches,
            vec!["Gebiet"]
        );
    }

    /// A hand-minified target packs several columns onto one line. `column_line`
    /// matches a line's first identifier only, so a column later on the same
    /// line is not found — but it is declared, and the error must say so
    /// rather than claiming it is absent.
    #[test]
    fn single_line_target_gets_the_reformat_hint_not_a_lie() {
        let sql = "CREATE TABLE t (a TEXT, b BIGINT) WITH (format = 'csv');\n";
        let err = column_line(sql, "b").unwrap_err().to_string();
        assert!(err.contains("one column per line"), "{err}");
        // A column that is genuinely absent keeps the plain message.
        let err = column_line(sql, "zzz").unwrap_err().to_string();
        assert!(err.contains("no line in this target declares `zzz`"), "{err}");
        assert!(!err.contains("one column per line"), "{err}");
    }
}
