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

use anyhow::{Context, Result};

use crate::config::Limits;
use crate::sample::FormatGuess;
use crate::spec::{DType, RowWindow};

/// One declared-column-to-be, merged across the pile.
struct DraftColumn {
    /// Sanitized, SQL-addressable — what the sniffer itself would call it.
    name: String,
    /// Verbatim spellings seen in headers, in first-seen order.
    origins: Vec<String>,
    /// The merged type, plus a caveat when merging had to widen.
    dtype: DType,
    caveat: Option<String>,
    /// Which *physical files* carry it — a column seen in any block of a
    /// split file counts as present in that file, per the presence note's
    /// own rule ("a column present in block 2 of file A is present in
    /// file A"). Deduplicated, so a column seen in two of a file's blocks
    /// still counts once.
    files: Vec<String>,
    /// `(physical file, this block's ordinal, that file's total block
    /// count)` for every block-level sighting — never populated for a
    /// whole-file sighting. This is what lets a per-column comment say
    /// `regions_summary.csv#2` when the column is confined to one block of
    /// a split file, which `files` above (physical-file presence only)
    /// cannot express by itself.
    block_sightings: Vec<(String, u32, usize)>,
}

pub fn draft_target(files: &[PathBuf], limits: Limits) -> Result<String> {
    if files.is_empty() {
        anyhow::bail!("nothing to draft from: pass the files the dataset should cover");
    }

    let mut columns: Vec<DraftColumn> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    // One entry per *physical* file that yielded at least one column —
    // never one per block, so one file's own stacked blocks can never look
    // like several files disagreeing about a vocabulary. This is what
    // `group_by_vocabulary` sees.
    let mut file_sets: Vec<(String, BTreeSet<String>)> = Vec::new();
    let mut day_first = false;
    let mut month_first = false;
    // Blocks or whole files successfully sniffed — used only to decide
    // whether anything sniffable was found at all; the physical-file counts
    // used for the header and for column presence live in `files_ok` below.
    let mut sniffed = 0usize;
    // Distinct physical files that yielded at least one column — the
    // denominator the header and every "in N of M file(s)" note use.
    let mut files_ok: BTreeSet<String> = BTreeSet::new();
    // One line per file that turned out to hold several stacked tables.
    let mut split_files: Vec<String> = Vec::new();

    for f in files {
        let label = short(f);
        // Excel is out of scope here: `regions_of(_, None, _)` is the
        // text-file path (it streams raw lines looking for blank-row
        // boundaries), and an .xlsx/.xls/.xlsb/.ods is binary — reading it
        // that way answers a question about the wrong bytes, not "no
        // regions". Splitting a *sheet* is `report.rs::expand_units`'s job,
        // which has a sheet name to ask `regions_of` with; a draft never
        // does.
        let windows = if crate::sample::guess_format(f) == FormatGuess::Excel {
            Vec::new()
        } else {
            crate::engine::regions_of(f, None, limits).unwrap_or_default()
        };
        if windows.len() >= 2 {
            split_files.push(format!(
                "{label} holds {} stacked tables; each is drafted as {label}#i",
                windows.len()
            ));
            // One entry in `file_sets` for the whole file — the union of
            // its blocks' columns — not one per block: `group_by_vocabulary`
            // asks whether files disagree with each other, and a file's own
            // blocks disagreeing with each other is a different question
            // (which is what the `only in <file>#<n>` column comments below
            // answer instead).
            let mut file_columns: BTreeSet<String> = BTreeSet::new();
            for w in &windows {
                match sniff_block(f, *w, limits) {
                    Ok(spec) => {
                        file_columns.extend(spec.columns.iter().map(|c| c.name.clone()));
                        record_columns(
                            &mut columns,
                            &mut day_first,
                            &mut month_first,
                            &mut sniffed,
                            &mut files_ok,
                            ColumnSighting {
                                physical_file: &label,
                                block: Some((w.ordinal, windows.len())),
                            },
                            &spec,
                        );
                    }
                    Err(e) => failures.push((format!("{label}#{}", w.ordinal), format!("{e:#}"))),
                }
            }
            if !file_columns.is_empty() {
                file_sets.push((label.clone(), file_columns));
            }
            continue;
        }
        let spec = match crate::sample::build(f, 16 * 1024, limits)
            .and_then(|s| crate::sniff::sniff(f, &s, limits))
        {
            Ok(r) => r.spec,
            Err(e) => {
                failures.push((label, format!("{e:#}")));
                continue;
            }
        };
        file_sets.push((label.clone(), spec.columns.iter().map(|c| c.name.clone()).collect()));
        record_columns(
            &mut columns,
            &mut day_first,
            &mut month_first,
            &mut sniffed,
            &mut files_ok,
            ColumnSighting { physical_file: &label, block: None },
            &spec,
        );
    }

    if sniffed == 0 {
        let mut msg = String::from("none of the files could be sniffed:");
        for (f, why) in &failures {
            msg.push_str(&format!("\n  {f}: {why}"));
        }
        anyhow::bail!("{msg}");
    }
    let files_seen = files_ok.len();

    let name = table_name(files);
    let globs = file_globs(files);

    let mut out = String::new();
    out.push_str(&format!(
        "-- Drafted by `tdy draft` from {files_seen} file(s). A DRAFT, not an answer:\n\
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
    if !split_files.is_empty() {
        out.push_str("--\n");
        for note in &split_files {
            out.push_str(&format!("-- NOTE: {note}\n"));
        }
    }
    // A directory is not a dataset. When the files' column sets barely
    // overlap, the union target below would demand every column of every
    // file and refuse the whole pile — mechanically correct and humanly
    // useless. The draft can see the grouping, so it says it, and leaves
    // the union in place for the case where the sparse overlap really is
    // one drifting dataset.
    let groups = group_by_vocabulary(&file_sets);
    if groups.len() > 1 {
        out.push_str("--\n-- NOTE: these files do not look like ONE dataset. By shared column\n");
        out.push_str(&format!("-- names they group as {} distinct shapes:\n", groups.len()));
        for (i, g) in groups.iter().enumerate() {
            out.push_str(&format!("--   group {}: {}\n", i + 1, g.join(", ")));
        }
        out.push_str("-- A target declares one dataset. If these are really several, draft each\n");
        out.push_str("-- group separately:  tdy draft <group's files> > <its own>.tdy.sql\n");
    }
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
        out.push_str(line.trim_end());
        if i + 1 < rendered.len() {
            out.push(',');
        }
        let mut notes: Vec<String> = Vec::new();
        if c.files.len() < files_seen {
            notes.push(format!("in {} of {files_seen} file(s)", c.files.len()));
        }
        // A column confined to some but not all of one file's own stacked
        // blocks: physical-file presence alone says nothing about this (the
        // file as a whole still has the column), so name the block(s)
        // directly — `regions_summary.csv#2` — which is where a human
        // learns which block a column came from.
        let mut by_file: std::collections::BTreeMap<&str, (usize, Vec<u32>)> = std::collections::BTreeMap::new();
        for (file, ordinal, total) in &c.block_sightings {
            by_file.entry(file.as_str()).or_insert((*total, Vec::new())).1.push(*ordinal);
        }
        for (file, (total, mut ordinals)) in by_file {
            if ordinals.len() < total {
                ordinals.sort_unstable();
                let labels: Vec<String> = ordinals.iter().map(|o| format!("{file}#{o}")).collect();
                notes.push(format!("only in {}", labels.join(", ")));
            }
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

/// Where one sniffed spec's columns came from: always a physical file
/// (`physical_file`), and — only when this sighting is one block of a
/// split file, never for a whole-file sighting — that block's own ordinal
/// and its file's total block count.
struct ColumnSighting<'a> {
    physical_file: &'a str,
    block: Option<(u32, usize)>,
}

/// Fold one sniffed spec's columns into the merged `columns` tally, under
/// `sighting` (a whole file, or one block of a split file). Shared by the
/// whole-file path and the split-file path so the two cannot tally
/// differently. `files_ok` collects the distinct physical files this run
/// has seen at least one column from — the denominator every presence note
/// (and the header) uses, since a column present in block 2 of file A is
/// present in file A, not in "half" of it.
fn record_columns(
    columns: &mut Vec<DraftColumn>,
    day_first: &mut bool,
    month_first: &mut bool,
    sniffed: &mut usize,
    files_ok: &mut BTreeSet<String>,
    sighting: ColumnSighting,
    spec: &crate::spec::ParseSpec,
) {
    *sniffed += 1;
    files_ok.insert(sighting.physical_file.to_string());
    for c in &spec.columns {
        match &c.dtype {
            DType::Date { format } | DType::Timestamp { format, .. } => {
                if format.starts_with("%d") {
                    *day_first = true;
                }
                if format.starts_with("%m") {
                    *month_first = true;
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
                if !d.files.iter().any(|f| f == sighting.physical_file) {
                    d.files.push(sighting.physical_file.to_string());
                }
                if let Some((ordinal, total)) = sighting.block {
                    d.block_sightings.push((sighting.physical_file.to_string(), ordinal, total));
                }
                let (merged, caveat) = merge(&d.dtype, &c.dtype, sighting.physical_file);
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
                files: vec![sighting.physical_file.to_string()],
                block_sightings: match sighting.block {
                    Some((ordinal, total)) => vec![(sighting.physical_file.to_string(), ordinal, total)],
                    None => Vec::new(),
                },
            }),
        }
    }
}

/// Sniff one stacked block of `path` as if it were its own file: copy the
/// block's own raw lines out to a scratch file with the same extension (so
/// format guessing — which reads the extension, not the bytes — sees a
/// `.csv` for a `.csv`), then run the ordinary sniffer over that. This is
/// the whole reason a block gets the sniffer's full machinery — title rows,
/// separator/date inference, type widening — rather than a cut-down pass of
/// its own that could disagree with what a plain file gets.
fn sniff_block(path: &Path, window: RowWindow, limits: Limits) -> Result<crate::spec::ParseSpec> {
    let bytes = block_bytes(path, window, limits)
        .with_context(|| format!("reading block {} of {}", window.ordinal, path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("csv");
    let mut tmp = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .context("creating a scratch file for the block")?;
    std::io::Write::write_all(&mut tmp, &bytes)
        .with_context(|| format!("writing block {} of {} to a scratch file", window.ordinal, path.display()))?;
    let sample = crate::sample::build(tmp.path(), 16 * 1024, limits)
        .with_context(|| format!("sampling block {} of {}", window.ordinal, path.display()))?;
    crate::sniff::sniff(tmp.path(), &sample, limits)
        .map(|r| r.spec)
        .with_context(|| format!("sniffing block {} of {}", window.ordinal, path.display()))
}

/// The raw bytes of one stacked block, by physical line number — the same
/// indexing `regions_of` counted `window` against, so a window it returned
/// names exactly these lines and no others.
fn block_bytes(path: &Path, window: RowWindow, limits: Limits) -> Result<Vec<u8>> {
    let real = crate::fileio::materialize(path, limits.max_decompressed_bytes)?;
    let file = std::fs::File::open(real.as_ref())
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    let mut index: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if index >= window.end {
            break;
        }
        buf.clear();
        let n = std::io::BufRead::read_until(&mut reader, b'\n', &mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        if index >= window.start {
            out.extend_from_slice(&buf);
        }
        index += 1;
    }
    Ok(out)
}

/// Files clustered by column-name overlap (Jaccard >= 0.5, greedy, in input
/// order). One group = one plausible dataset; several = several.
fn group_by_vocabulary(file_sets: &[(String, BTreeSet<String>)]) -> Vec<Vec<String>> {
    let mut groups: Vec<(BTreeSet<String>, Vec<String>)> = Vec::new();
    for (file, set) in file_sets {
        let found = groups.iter_mut().find(|(u, _)| {
            let inter = u.intersection(set).count();
            let union = u.union(set).count();
            union > 0 && (inter as f64) / (union as f64) >= 0.5
        });
        match found {
            Some((u, files)) => {
                u.extend(set.iter().cloned());
                files.push(file.clone());
            }
            None => groups.push((set.clone(), vec![file.clone()])),
        }
    }
    groups.into_iter().map(|(_, files)| files).collect()
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
