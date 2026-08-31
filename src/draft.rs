//! `tdy draft` — a target scaffold from a pile of sniffed files.
//!
//! The declaration is the one place a human states intent, and tdy will not
//! write intent. What it *can* write is everything mechanical about the pile:
//! which column names occur, in which spellings, in which files, with which
//! types — laid out so the judgements that remain (are `datum` and `date` one
//! column? is a missing `region` an error or a fact?) are each a one-line
//! edit with the syntax already on screen.
//!
//! The output is a DRAFT by construction, and says so at the top. It is also
//! valid target SQL: `Target::parse` accepts it as emitted, so the loop is
//! draft -> edit -> `tdy fit`, with every wrong guess caught by the gates
//! rather than becoming a wrong dataset.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Limits;
use crate::spec::DType;

/// One declared-column-to-be, merged across the pile.
struct DraftColumn {
    /// Sanitized, SQL-addressable — what the sniffer itself would call it.
    name: String,
    /// Verbatim spellings seen in headers, in first-seen order.
    origins: Vec<String>,
    /// The merged type, plus a caveat when merging had to widen.
    dtype: DType,
    caveat: Option<String>,
    /// Which files carry it.
    files: Vec<String>,
}

pub fn draft_target(files: &[PathBuf], limits: Limits) -> Result<String> {
    if files.is_empty() {
        anyhow::bail!("nothing to draft from: pass the files the dataset should cover");
    }

    let mut columns: Vec<DraftColumn> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut day_first = false;
    let mut month_first = false;
    let mut sniffed = 0usize;

    for f in files {
        let label = short(f);
        let spec = match crate::sample::build(f, 16 * 1024, limits)
            .and_then(|s| crate::sniff::sniff(f, &s, limits))
        {
            Ok(r) => r.spec,
            Err(e) => {
                failures.push((label, format!("{e:#}")));
                continue;
            }
        };
        sniffed += 1;
        for c in &spec.columns {
            match &c.dtype {
                DType::Date { format } | DType::Timestamp { format, .. } => {
                    if format.starts_with("%d") {
                        day_first = true;
                    }
                    if format.starts_with("%m") {
                        month_first = true;
                    }
                }
                _ => {}
            }
            let origin = c.source_name().to_string();
            match columns.iter_mut().find(|d| d.name == c.name) {
                Some(d) => {
                    if !d.origins.contains(&origin) {
                        d.origins.push(origin);
                    }
                    if !d.files.contains(&label) {
                        d.files.push(label.clone());
                    }
                    let (merged, caveat) = merge(&d.dtype, &c.dtype, &label);
                    d.dtype = merged;
                    if d.caveat.is_none() {
                        d.caveat = caveat;
                    }
                }
                None => columns.push(DraftColumn {
                    name: c.name.clone(),
                    origins: vec![origin],
                    dtype: c.dtype.clone(),
                    caveat: None,
                    files: vec![label.clone()],
                }),
            }
        }
    }

    if sniffed == 0 {
        let mut msg = String::from("none of the files could be sniffed:");
        for (f, why) in &failures {
            msg.push_str(&format!("\n  {f}: {why}"));
        }
        anyhow::bail!("{msg}");
    }

    let name = table_name(files);
    let globs = file_globs(files);

    let mut out = String::new();
    out.push_str(&format!(
        "-- Drafted by `tdy draft` from {sniffed} file(s). A DRAFT, not an answer:\n\
         -- everything below is what the sniffer measured; only you know which columns\n\
         -- mean the same thing and which files do not belong. Edit, save as\n\
         -- <name>.tdy.sql beside the data, then:  tdy fit <name>.tdy.sql\n\
         --\n\
         -- Things tdy cannot decide, left for you:\n\
         --   * every column is nullable until you add NOT NULL\n\
         --   * two names below may be one column wearing two spellings (`datum` and\n\
         --     `date`, say): keep one, and move the other's matches= spellings onto it\n\
         --   * a column absent from some files is either a mistake in those files or a\n\
         --     fact about them — declare `if_missing = 'null'` only if it is a fact\n"
    ));
    if !failures.is_empty() {
        out.push_str("--\n-- Files that could not be sniffed (excluded from this draft):\n");
        for (f, why) in &failures {
            out.push_str(&format!("--   {f}: {}\n", first_line(why)));
        }
    }
    out.push_str(&format!("\nCREATE TABLE {name} (\n"));

    let width = columns.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let twidth = columns.iter().map(|c| sql_type(&c.dtype).len()).max().unwrap_or(0);
    let rendered: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut line =
                format!("  {:<width$} {:<twidth$}", c.name, sql_type(&c.dtype));
            let extra_spellings: Vec<&String> =
                c.origins.iter().filter(|o| o.as_str() != c.name).collect();
            if !extra_spellings.is_empty() {
                let m: Vec<String> = extra_spellings.iter().map(|s| s.to_string()).collect();
                line.push_str(&format!(" OPTIONS(matches = '{}')", m.join(", ")));
            }
            line
        })
        .collect();
    for (i, (line, c)) in rendered.iter().zip(&columns).enumerate() {
        out.push_str(line);
        if i + 1 < rendered.len() {
            out.push(',');
        }
        let mut notes: Vec<String> = Vec::new();
        if c.files.len() < sniffed {
            notes.push(format!("in {} of {sniffed} file(s)", c.files.len()));
        }
        if let Some(cv) = &c.caveat {
            notes.push(cv.clone());
        }
        if !notes.is_empty() {
            out.push_str(&format!("  -- {}", notes.join("; ")));
        }
        out.push('\n');
    }
    out.push_str(")\nWITH (\n");
    out.push_str(&format!("  files = '{}'", globs.join(", ")));
    match (day_first, month_first) {
        (true, false) => out.push_str(",\n  date_order = 'dmy'"),
        (false, true) => out.push_str(",\n  date_order = 'mdy'"),
        (true, true) => out.push_str(
            ",\n  date_order = 'dmy'  -- BOTH day-first and month-first formats were seen;\n\
             \x20                     -- check which files use which before trusting this",
        ),
        (false, false) => {}
    }
    out.push_str("\n);\n");
    Ok(out)
}

/// Widen two sniffed types to one a target could declare over both files.
fn merge(a: &DType, b: &DType, file: &str) -> (DType, Option<String>) {
    use DType::*;
    if kind(a) == kind(b) {
        let merged = match (a, b) {
            (Decimal { precision: p1, scale: s1 }, Decimal { precision: p2, scale: s2 }) => {
                Decimal { precision: (*p1).max(*p2), scale: (*s1).max(*s2) }
            }
            // Formats are per-file facts; the target's job is only the kind.
            _ => a.clone(),
        };
        return (merged, None);
    }
    let widened = match (kind(a), kind(b)) {
        ("int", "float") | ("float", "int") => Some(Float64),
        ("int", "decimal") | ("decimal", "int") => {
            let (p, s) = match (a, b) {
                (Decimal { precision, scale }, _) | (_, Decimal { precision, scale }) => {
                    (*precision, *scale)
                }
                _ => (18, 2),
            };
            Some(Decimal { precision: p.max(19), scale: s })
        }
        ("float", "decimal") | ("decimal", "float") => Some(Float64),
        _ => None,
    };
    match widened {
        Some(w) => (
            w.clone(),
            Some(format!("widened to {} ({file} disagrees with earlier files)", sql_type(&w))),
        ),
        None => (
            Utf8,
            Some(format!(
                "kept TEXT: {file} types this as {}, earlier files as {} — settle it and \
                 narrow the type",
                sql_type(b),
                sql_type(a)
            )),
        ),
    }
}

fn kind(d: &DType) -> &'static str {
    match d {
        DType::Utf8 => "text",
        DType::Bool => "bool",
        DType::Int64 => "int",
        DType::Float64 => "float",
        DType::Decimal { .. } => "decimal",
        DType::Date { .. } => "date",
        DType::Timestamp { .. } => "timestamp",
    }
}

fn sql_type(d: &DType) -> String {
    match d {
        DType::Utf8 => "TEXT".into(),
        DType::Bool => "BOOLEAN".into(),
        DType::Int64 => "BIGINT".into(),
        DType::Float64 => "DOUBLE".into(),
        DType::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
        DType::Date { .. } => "DATE".into(),
        DType::Timestamp { .. } => "TIMESTAMP".into(),
    }
}

/// `exports/*.csv, exports/*.xlsx` from the actual paths, deduplicated.
fn file_globs(files: &[PathBuf]) -> Vec<String> {
    let mut globs: BTreeSet<String> = BTreeSet::new();
    for f in files {
        let dir = f.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let ext = f
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| "csv".into());
        if dir.is_empty() || dir == "." {
            globs.insert(format!("*.{ext}"));
        } else {
            globs.insert(format!("{dir}/*.{ext}"));
        }
    }
    globs.into_iter().collect()
}

fn table_name(files: &[PathBuf]) -> String {
    let dir = files[0]
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| crate::sniff::sanitize(&n.to_string_lossy()))
        .filter(|n| !n.is_empty() && !n.starts_with("col_"));
    dir.unwrap_or_else(|| "dataset".into())
}

fn short(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
