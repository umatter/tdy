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
use crate::sample::FormatGuess;
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
    /// The physical files whose own sniff typed this column at a DECIMAL
    /// scale above `MONEY_PLACES` — what lets the rounding comment say
    /// which file the noisy scale came from when not every file did.
    noisy_files: Vec<String>,
    /// For a leaf of a JSON record: the RFC 6901 pointer into the value of
    /// its top-level key (`origins` holds that key, drafted as `matches`).
    pointer: Option<String>,
}

/// The widest scale the sniffer gives money it recognises by shape alone
/// (`sniff::guess_type`'s non-currency branch caps there). A DECIMAL wider
/// than this came from a currency-formatted cell holding a computed float:
/// its scale is the sample's IEEE-754 noise, and a later row can carry one
/// place more, which the fit refuses unless rounding is declared.
const MONEY_PLACES: i8 = 6;

fn noisy_scale(d: &DType) -> Option<i8> {
    match d {
        DType::Decimal { scale, .. } if *scale > MONEY_PLACES => Some(*scale),
        _ => None,
    }
}

/// [`draft_target_in`] for a target written in the current directory —
/// the CLI's stdout and the console's `--to` alike.
pub fn draft_target(files: &[PathBuf], limits: Limits) -> Result<String> {
    let cwd = std::env::current_dir().ok();
    draft_target_in(files, cwd.as_deref(), limits)
}

/// The draft, its `files` globs relative to `base`, the directory the
/// target is to be written in. A relative path is taken to be relative to
/// it already; an absolute one is rewritten relative to it, because a lock
/// names its members relative to the target and an absolute glob made them
/// absolute paths `--accept` could not name.
pub fn draft_target_in(files: &[PathBuf], base: Option<&Path>, limits: Limits) -> Result<String> {
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

    // Root-object JSON documents, held until the whole pile has been seen:
    // which reading drafts them is a question about the pile (below).
    let mut json_docs: Vec<JsonDoc> = Vec::new();

    for f in files {
        let label = short(f);
        // A JSON document is never split at blank lines (a blank line inside
        // pretty-printed JSON is not a table boundary), and a root object is
        // set aside for the pile's decision.
        if crate::sample::guess_format(f) == FormatGuess::Json {
            let spec = match crate::sample::build(f, 16 * 1024, limits).and_then(|s| crate::sniff::sniff(f, &s, limits)) {
                Ok(r) => r.spec,
                Err(e) => {
                    failures.push((label, format!("{e:#}")));
                    continue;
                }
            };
            match &spec.extraction {
                crate::spec::Extraction::Json { lines: false, record: true, .. } => {
                    json_docs.push(JsonDoc { label, path: f.clone(), array: None });
                }
                crate::spec::Extraction::Json { lines: false, pointer: Some(p), .. } => {
                    json_docs.push(JsonDoc { label, path: f.clone(), array: Some((p.clone(), spec.clone())) });
                }
                _ => {
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
            }
            continue;
        }
        // A workbook is split on the sheet the whole-file sniff reads (the
        // one `sniff::pick_sheet` ranks first), with the same split and
        // block framing `fit` uses for it; a text file on its raw lines.
        // `regions_of(_, None, _)` over a workbook's bytes would answer a
        // question about the wrong bytes, not "no regions".
        let sample = crate::sample::build(f, 16 * 1024, limits);
        let is_excel = crate::sample::guess_format(f) == FormatGuess::Excel;
        let sheet = match (is_excel, &sample) {
            (true, Ok(s)) => crate::sniff::pick_sheet(f, s, limits)
                .and_then(|name| crate::sniff::OpenSheet::open(f, &name, limits).ok()),
            _ => None,
        };
        // A block's label is the member name `fit` gives it: `file#i`, and
        // `book.xlsx#i` for a workbook with one sheet, whose member stays
        // plain. A sheet of several is named, `book.xlsx#Sheet`; it is split
        // only when one table is left (below), so no `#Sheet#i` is printed.
        let several_sheets = matches!(&sample, Ok(s) if s.sheets.len() > 1);
        let prefix = match &sheet {
            Some(open) if several_sheets => format!("{label}#{}", open.name),
            _ => label.clone(),
        };
        let found = match (&sheet, is_excel) {
            (Some(open), _) => crate::engine::regions_of_range(&open.range),
            (None, true) => Default::default(),
            (None, false) => crate::engine::regions_of(f, None, limits).unwrap_or_default(),
        };
        // A block with no header of its own takes the header run a blank
        // row cut off above it, as `fit` frames it; never over a header the
        // block's own frame promoted (`fit::Adoption::Draft`).
        let framed = crate::fit::frame_blocks(
            f,
            sheet.as_ref(),
            &found,
            crate::sniff::SniffOpts::default(),
            limits,
            crate::fit::Adoption::Draft,
        );
        let regions = &framed.regions;
        let frame_block = |w: crate::spec::RowWindow| -> Result<crate::spec::ParseSpec> {
            match &sheet {
                Some(open) => {
                    crate::fit::region_frame(f, Some(open), w, crate::sniff::SniffOpts::default(), limits)
                        .map_err(|e| anyhow::anyhow!("{e}"))
                }
                None => crate::sniff::sniff_text_block(f, w, limits, crate::sniff::SniffOpts::default()),
            }
        };
        // A block none of whose rows holds two fields is a banner or a
        // footnote block, not a table: drafting it declared a column no
        // table has, and the unedited draft then fit nothing. It is the
        // same criterion `Regions::table_shaped` uses to decide what is not
        // data-like, and the note says which block was skipped and why —
        // unless every block is one field wide, which is a one-column file
        // with blank lines in it, drafted whole. A block with no header of
        // its own (`FramedBlocks::draft_kind`) is skipped the same way, as it
        // is no `fit` candidate: a two-cell footnote block drafted `col_N`
        // columns no table has; and when no block is a table the file is
        // drafted whole, as `fit` then reads it. A body whose first row
        // reads like data under a run of title lines and a header makes the
        // whole file drafted whole: a draft does not adopt over it, and its
        // own first row is no header.
        let mut windows = Vec::new();
        let mut banners = Vec::new();
        let mut headless = Vec::new();
        let mut under_run = None;
        for (i, (w, widest)) in regions.windows.iter().zip(&regions.window_widest).enumerate() {
            let at = |why: &str| format!("{prefix}: block {} (lines {}–{}) skipped: {why}", w.ordinal, w.start + 1, w.end);
            if *widest < 2 {
                banners.push(at("one field per line"));
                continue;
            }
            match framed.draft_kind(i, sheet.is_some()) {
                crate::fit::DraftKind::Table => windows.push(*w),
                crate::fit::DraftKind::Headless => headless.push(at("no header of its own")),
                crate::fit::DraftKind::UnderHeaderRun => {
                    under_run.get_or_insert(w.ordinal);
                }
            }
        }
        // A sheet of a workbook with several is a `fit` member through its
        // blocks only when exactly one passes and nothing table-shaped is
        // left over (`fit::discover_sheets`: sheet expansion asks nobody).
        // Drafted from the blocks otherwise, it fit no sheet at all — so it
        // is drafted whole, as `fit` reads it.
        let one_clean_block = windows.len() == 1 && {
            let kept: Vec<bool> = regions.windows.iter().map(|w| windows.contains(w)).collect();
            regions.gated(&kept).table_shaped().next().is_none()
        };
        let under_run = under_run.map(|i| {
            format!(
                "block {i}'s first row reads like data; drafted whole — a by-name target can still \
                 bind it through its header run"
            )
        });
        let whole = if let Some(why) = &under_run {
            Some(why.as_str())
        } else if windows.is_empty() && headless.is_empty() && !banners.is_empty() {
            Some("all blocks one field wide; drafted whole")
        } else if windows.is_empty() && !headless.is_empty() {
            Some("no block has a header of its own; drafted whole")
        } else if several_sheets && !windows.is_empty() && !one_clean_block {
            Some(
                "drafted whole: a sheet of a workbook with several is fitted by its blocks only \
                 when one table is left and nothing table-shaped",
            )
        } else {
            None
        };
        match whole {
            Some(why) => {
                split_files.push(format!("{prefix}: {why}"));
                windows.clear();
            }
            None => {
                split_files.extend(banners);
                split_files.extend(headless);
            }
        }
        // One table left among banners, or one block with lines the split
        // dropped around it: it is the file's table, drafted from its own
        // rows and not commented as "only in" a block. Drafting the whole
        // file instead declared a title line's values as the columns, and
        // the fit then read the real header as a data row, silently.
        let separated = regions.windows.len() > 1 || !regions.dropped.is_empty();
        if let ([w], true) = (windows.as_slice(), separated) {
            match frame_block(*w) {
                Ok(spec) => {
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
                Err(e) => failures.push((format!("{prefix}#{}", w.ordinal), format!("{e:#}"))),
            }
            continue;
        }
        if windows.len() >= 2 {
            split_files.push(format!(
                "{prefix} holds {} stacked tables; each is drafted as {prefix}#i",
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
                match frame_block(*w) {
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
                                block: Some((&prefix, w.ordinal, windows.len())),
                            },
                            &spec,
                        );
                    }
                    Err(e) => failures.push((format!("{prefix}#{}", w.ordinal), format!("{e:#}"))),
                }
            }
            if !file_columns.is_empty() {
                file_sets.push((label.clone(), file_columns));
            }
            continue;
        }
        let spec = match sample.and_then(|s| crate::sniff::sniff(f, &s, limits)) {
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

    // The pile's reading of its root-object documents. Each one can be read
    // as one record; an array reading exists only where the document holds
    // an array. When every one of them is sniffed onto an array at the same
    // pointer — a pile of API dumps, each with its `rows` — the array is the
    // pile's table and the draft is what it always was. Otherwise the
    // document is the record and the directory is the table: the reading
    // every file has (villagerdb's items hold `games.nl.buyPrices` in most
    // documents and no array in others).
    let mut json_notes: Vec<String> = Vec::new();
    let same_array = !json_docs.is_empty()
        && json_docs.iter().all(|d| d.array.as_ref().map(|(p, _)| p) == json_docs[0].array.as_ref().map(|(p, _)| p))
        && json_docs[0].array.is_some();
    if same_array {
        for d in &json_docs {
            let (_, spec) = d.array.as_ref().expect("same_array");
            file_sets.push((d.label.clone(), spec.columns.iter().map(|c| c.name.clone()).collect()));
            record_columns(
                &mut columns,
                &mut day_first,
                &mut month_first,
                &mut sniffed,
                &mut files_ok,
                ColumnSighting { physical_file: &d.label, block: None },
                spec,
            );
        }
    } else if !json_docs.is_empty() {
        let read_as_record = json_docs.iter().filter(|d| d.array.is_some()).count();
        if read_as_record > 0 {
            json_notes.push(format!(
                "{read_as_record} of {} JSON document(s) hold an array, which `tdy sniff` reads on \
                 its own; the pile is drafted as one record per document, and `tdy fit` still \
                 tries every array against the target",
                json_docs.len()
            ));
        }
        let mut leaves = JsonLeaves::default();
        for d in &json_docs {
            match leaves.add(&d.path, &d.label, limits) {
                Ok(()) => {
                    sniffed += 1;
                    files_ok.insert(d.label.clone());
                }
                Err(e) => failures.push((d.label.clone(), format!("{e:#}"))),
            }
        }
        file_sets.extend(leaves.file_sets.drain(..));
        json_notes.extend(leaves.notes());
        leaves.into_columns(&mut columns, &mut day_first, &mut month_first);
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
    let globs = file_globs(files, base);

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
    if !split_files.is_empty() || !json_notes.is_empty() {
        out.push_str("--\n");
        for note in split_files.iter().chain(&json_notes) {
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
            let mut options: Vec<String> = Vec::new();
            if !extra_spellings.is_empty() {
                let m: Vec<String> = extra_spellings.iter().map(|s| s.to_string()).collect();
                options.push(format!("matches = '{}'", m.join(", ")));
            }
            if let Some(p) = &c.pointer {
                options.push(format!("pointer = '{}'", p.replace('\'', "''")));
            }
            // The sniffer's scale is reproduced, not second-guessed; the
            // rounding a longer later value needs is declared beside it, in
            // the reviewed target, which is where a rounding is authorised.
            if noisy_scale(&c.dtype).is_some() {
                options.push("round = 'half_away'".into());
            }
            if !options.is_empty() {
                line.push_str(&format!(" OPTIONS({})", options.join(", ")));
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
        if let Some(scale) = noisy_scale(&c.dtype) {
            let from = if c.noisy_files.len() < c.files.len() {
                format!(" (from {})", c.noisy_files.join(", "))
            } else {
                String::new()
            };
            notes.push(format!(
                "scale {scale}{from} is float noise in a currency-formatted cell, not money's \
                 places; rounding is declared so a longer value does not refuse the file — or \
                 declare DOUBLE"
            ));
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
    /// The block's label prefix (`file` or `book.xlsx#Sheet`), its ordinal
    /// and its file's total block count.
    block: Option<(&'a str, u32, usize)>,
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
                if let Some((prefix, ordinal, total)) = sighting.block {
                    d.block_sightings.push((prefix.to_string(), ordinal, total));
                }
                if noisy_scale(&c.dtype).is_some() && !d.noisy_files.iter().any(|f| f.as_str() == sighting.physical_file) {
                    d.noisy_files.push(sighting.physical_file.to_string());
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
                    Some((prefix, ordinal, total)) => vec![(prefix.to_string(), ordinal, total)],
                    None => Vec::new(),
                },
                noisy_files: match noisy_scale(&c.dtype) {
                    Some(_) => vec![sighting.physical_file.to_string()],
                    None => Vec::new(),
                },
                pointer: None,
            }),
        }
    }
}

/// A root-object JSON document of the pile, with its array reading when the
/// sniffer gave it one: (pointer, that frame's spec).
struct JsonDoc {
    label: String,
    path: PathBuf,
    array: Option<(String, crate::spec::ParseSpec)>,
}

/// How deep a draft descends into a record, counted from the top-level key:
/// `games/nh/sellPrice/value` is four.
const LEAF_DEPTH: usize = 4;

/// Every scalar leaf of a pile of JSON records, keyed by its path, with the
/// values seen and the files that hold it.
#[derive(Default)]
struct JsonLeaves {
    /// In first-seen order across the pile.
    leaves: Vec<JsonLeaf>,
    at: std::collections::HashMap<Vec<String>, usize>,
    /// Paths of arrays under a nested object, and of objects past
    /// [`LEAF_DEPTH`], with how many files hold each.
    nested_arrays: std::collections::BTreeMap<String, usize>,
    too_deep: std::collections::BTreeMap<String, usize>,
    file_sets: Vec<(String, BTreeSet<String>)>,
}

struct JsonLeaf {
    path: Vec<String>,
    values: Vec<String>,
    files: Vec<String>,
    /// A top-level key holding an array in some file: one TEXT column of its
    /// JSON, never descended into.
    array: bool,
}

impl JsonLeaves {
    /// Read one document as one record and collect its leaves.
    fn add(&mut self, path: &Path, label: &str, limits: Limits) -> Result<()> {
        let bytes = crate::fileio::read_all(path, limits.max_file_bytes)?;
        let (text, _) = crate::sample::decode_text(&bytes, None);
        let doc = crate::jsondoc::Node::parse(&text)?;
        let crate::jsondoc::Node::Object(entries) = &doc else {
            anyhow::bail!("expected one JSON object, found {}", doc.kind());
        };
        let mut found: Vec<(Vec<String>, String, bool)> = Vec::new();
        let mut seen_arrays = BTreeSet::new();
        let mut seen_deep = BTreeSet::new();
        for (k, v) in entries {
            walk(v, &mut vec![k.clone()], &mut found, &mut seen_arrays, &mut seen_deep);
        }
        for p in seen_arrays {
            *self.nested_arrays.entry(p).or_default() += 1;
        }
        for p in seen_deep {
            *self.too_deep.entry(p).or_default() += 1;
        }
        let mut names = BTreeSet::new();
        for (p, value, array) in found {
            names.insert(crate::sniff::sanitize(&p.join("_")));
            let i = match self.at.get(&p) {
                Some(i) => *i,
                None => {
                    self.leaves.push(JsonLeaf { path: p.clone(), values: Vec::new(), files: Vec::new(), array: false });
                    self.at.insert(p, self.leaves.len() - 1);
                    self.leaves.len() - 1
                }
            };
            let leaf = &mut self.leaves[i];
            leaf.values.push(value);
            leaf.array |= array;
            if leaf.files.last().map(String::as_str) != Some(label) {
                leaf.files.push(label.to_string());
            }
        }
        self.file_sets.push((label.to_string(), names));
        Ok(())
    }

    /// The header notes: what was not drafted, and why.
    fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        let list = |m: &std::collections::BTreeMap<String, usize>| {
            let mut v: Vec<(&String, &usize)> = m.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let shown: Vec<String> = v.iter().take(8).map(|(p, n)| format!("`{p}` (in {n})")).collect();
            let more = v.len().saturating_sub(shown.len());
            format!("{}{}", shown.join(", "), if more > 0 { format!(", … {more} more") } else { String::new() })
        };
        if !self.nested_arrays.is_empty() {
            out.push(format!(
                "arrays under a nested object are not drafted — a column cannot hold one (a \
                 pointer onto an array is an error) and one row per element is out of scope: {}",
                list(&self.nested_arrays)
            ));
        }
        if !self.too_deep.is_empty() {
            out.push(format!(
                "objects deeper than {LEAF_DEPTH} levels are not descended into: {}",
                list(&self.too_deep)
            ));
        }
        out
    }

    /// Name, type and append the leaves as draft columns. A name already
    /// taken — by another column, or by another path that sanitises to the
    /// same spelling — gets the sniffer's dedupe and a comment naming the
    /// path it reads.
    fn into_columns(self, columns: &mut Vec<DraftColumn>, day_first: &mut bool, month_first: &mut bool) {
        let mut taken: std::collections::HashSet<String> = columns.iter().map(|c| c.name.clone()).collect();
        for leaf in self.leaves {
            let base = crate::sniff::sanitize(&leaf.path.join("_"));
            let mut name = base.clone();
            let mut n = 2;
            while taken.contains(&name) {
                name = format!("{base}_{n}");
                n += 1;
            }
            taken.insert(name.clone());
            let joined = leaf.path.join("/");
            let mut caveats: Vec<String> = Vec::new();
            if name != base {
                caveats.push(format!("reads `{joined}`; `{base}` was already another column"));
            }
            let values: Vec<&str> = leaf.values.iter().map(String::as_str).collect();
            let dtype = if leaf.array {
                caveats.push("an array: drafted as one TEXT column of its JSON, not descended into".into());
                DType::Utf8
            } else {
                let mut conflict: Option<String> = None;
                let d = crate::sniff::guess_dtype_all(&values, &name, |a, b| {
                    let (m, c) = merge(a, b, "documents further into the pile");
                    if conflict.is_none() {
                        conflict = c;
                    }
                    m
                })
                .unwrap_or(DType::Utf8);
                caveats.extend(conflict);
                d
            };
            if let DType::Date { format } | DType::Timestamp { format, .. } = &dtype {
                *day_first |= format.starts_with("%d");
                *month_first |= format.starts_with("%m");
            }
            let pointer = (leaf.path.len() > 1).then(|| {
                leaf.path[1..].iter().map(|t| format!("/{}", crate::sniff::escape_pointer_token(t))).collect::<String>()
            });
            columns.push(DraftColumn {
                name,
                origins: vec![leaf.path[0].clone()],
                dtype,
                caveat: (!caveats.is_empty()).then(|| caveats.join("; ")),
                files: leaf.files,
                block_sightings: Vec::new(),
                noisy_files: Vec::new(),
                pointer,
            });
        }
    }
}

/// Collect the scalar leaves under one value of a record. `path` starts at
/// the top-level key. A top-level array is one leaf of JSON text; an array
/// deeper down, or an object past [`LEAF_DEPTH`], is recorded and skipped.
fn walk(
    v: &crate::jsondoc::Node,
    path: &mut Vec<String>,
    found: &mut Vec<(Vec<String>, String, bool)>,
    arrays: &mut BTreeSet<String>,
    deep: &mut BTreeSet<String>,
) {
    use crate::jsondoc::Node;
    match v {
        Node::Scalar(s) => found.push((path.clone(), crate::engine::json_scalar(s), false)),
        Node::Array(_) if path.len() == 1 => found.push((path.clone(), crate::engine::json_scalar(&v.to_value()), true)),
        Node::Array(_) => {
            arrays.insert(path.join("/"));
        }
        Node::Object(_) if path.len() >= LEAF_DEPTH => {
            deep.insert(path.join("/"));
        }
        Node::Object(entries) => {
            for (k, child) in entries {
                path.push(k.clone());
                walk(child, path, found, arrays, deep);
                path.pop();
            }
        }
    }
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

/// `exports/*.csv, exports/*.xlsx` from the actual paths, deduplicated,
/// with an absolute directory made relative to `base`.
fn file_globs(files: &[PathBuf], base: Option<&Path>) -> Vec<String> {
    let mut globs: BTreeSet<String> = BTreeSet::new();
    for f in files {
        // Relative only at or below `base`; anywhere else absolute, since a
        // `..` ladder is relative to wherever the draft ran, not to the
        // target written beside the data.
        let dir = match (f.parent(), base) {
            (None, _) => String::new(),
            (Some(p), _) if p.as_os_str().is_empty() => String::new(),
            (Some(p), Some(b)) => {
                let abs = b.join(p);
                relative_dir(&abs, b).unwrap_or_else(|| {
                    abs.canonicalize().unwrap_or(abs).to_string_lossy().to_string()
                })
            }
            (Some(p), None) => p.to_string_lossy().to_string(),
        };
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

/// `dir` relative to `base`, both canonicalised, when `dir` is `base` or
/// below it — `None` otherwise. A `..` ladder up to a shared prefix such as
/// `/tmp` is relative to the directory the draft ran in, and from a target
/// written beside the data it named no file.
fn relative_dir(dir: &Path, base: &Path) -> Option<String> {
    let (dir, base) = (dir.canonicalize().ok()?, base.canonicalize().ok()?);
    let rest = dir.strip_prefix(&base).ok()?;
    Some(rest.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect::<Vec<_>>().join("/"))
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
