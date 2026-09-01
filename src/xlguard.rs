//! Bounding spreadsheet containers *before* a reader allocates their grid.
//!
//! Every other limit in tdy is applied to a table that already exists. That
//! works for delimited text, where the file has to be as big as the data.
//! Spreadsheets are different: the geometry is *declared*, so a few hundred
//! bytes can ask for tens of gigabytes.
//!
//! ```text
//! <table:table-row table:number-rows-repeated="2000000"> ...20 cells... </table:table-row>
//! ```
//!
//! That is 898 bytes on disk and 40 million cells in memory — measured at
//! 4.8 GB peak RSS, a 5,300,000x amplification. `max_cells` never fired,
//! because it is checked against the table calamine hands back, which cannot
//! happen until the allocation already has. On a machine without the memory
//! the process aborts with `memory allocation of N bytes failed`, which is
//! the one failure mode tdy is not allowed to have: not a loud error naming
//! the problem, but SIGABRT.
//!
//! So the geometry is read first, from the container itself, and a file that
//! declares more than `max_cells` is refused with a sentence.
//!
//! What each format allows us to know, and when:
//!
//! * **ods** — `Ods::new` parses all of content.xml eagerly, so by the time
//!   calamine returns a reader the allocation has happened. The scan here
//!   therefore runs *before* the workbook is opened at all.
//! * **xlsx / xlsm** — lazy per sheet, and `XlsxCellReader::dimensions()`
//!   reports the declared extent before the grid is built, so the check
//!   lives at the call site in `engine`, where the target sheet is known.
//! * **xlsb** — zip-based, so the expansion check below applies, but its
//!   geometry is not exposed before materialising. Bounded only by that.
//! * **xls** — BIFF8 row and column indices are 16-bit, capping a sheet at
//!   65536 x 256 cells whatever the file claims, and the format is not
//!   compressed. Bounded by construction rather than by us.
//!
//! The zip expansion check is separate and complementary: it catches an
//! ordinary compression bomb (a huge `sharedStrings.xml`, say), which the
//! geometry scan would not, while the geometry scan catches semantic
//! expansion, which compression ratios would not — the bomb above has a
//! *tiny* content.xml.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::Limits;

/// How much decompressed XML the geometry scan will read before giving up.
/// The expansion check runs first and uses the real limit; this is only a
/// backstop so a malformed container cannot spin forever.
const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// True for the container formats that are zip archives underneath.
fn is_zip_container(path: &Path) -> bool {
    matches!(
        ext(path).as_deref(),
        Some("xlsx") | Some("xlsm") | Some("xlsb") | Some("ods")
    )
}

fn ext(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// Refuse a workbook that would not fit in memory, before anything reads it.
///
/// Call this before `open_workbook_auto` — for ods, opening *is* the
/// allocation.
pub fn preflight(path: &Path, limits: &Limits) -> Result<()> {
    if !is_zip_container(path) {
        return Ok(());
    }
    check_zip_expansion(path, limits)?;
    if ext(path).as_deref() == Some("ods") {
        check_ods_geometry(path, limits)?;
    }
    Ok(())
}

/// Sum the *uncompressed* sizes in the zip central directory.
///
/// Reading the directory decompresses nothing, so this is cheap however
/// large the claim is. It is the classic zip-bomb guard: the ratio is not
/// what matters, the absolute size we would have to hold is.
fn check_zip_expansion(path: &Path, limits: &Limits) -> Result<()> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut zip = match zip::ZipArchive::new(BufReader::new(f)) {
        Ok(z) => z,
        // Not a readable zip: let calamine produce the format-specific error
        // rather than inventing a worse one here.
        Err(_) => return Ok(()),
    };
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(i) else { continue };
        total = total.saturating_add(entry.size());
        if total > limits.max_file_bytes {
            bail!(
                "{} expands to at least {} bytes, above the limit of {} \
                 (raise [limits].max_file_bytes if this is intended)",
                path.display(),
                total,
                limits.max_file_bytes
            );
        }
    }
    Ok(())
}

/// The declared extent of the largest table in an .ods, in cells.
///
/// Mirrors what calamine will actually allocate, which is the *used* area:
/// runs of cells carrying no value are padding, not data. That distinction
/// is what keeps this from refusing ordinary documents — LibreOffice pads
/// every sheet it writes out to the full grid, so a real file routinely ends
/// with `number-rows-repeated="1048570"` over valueless cells. Counting
/// those would reject almost every .ods in existence.
fn check_ods_geometry(path: &Path, limits: &Limits) -> Result<()> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut zip = match zip::ZipArchive::new(BufReader::new(f)) {
        Ok(z) => z,
        Err(_) => return Ok(()),
    };
    let Ok(entry) = zip.by_name("content.xml") else {
        // No content.xml: calamine will say so more precisely.
        return Ok(());
    };

    let cells = scan_ods_cells(entry.take(MAX_SCAN_BYTES))?;
    if cells > limits.max_cells {
        bail!(
            "{} declares a sheet of {} cells, above the limit of {} \
             (raise [limits].max_cells if this is intended)",
            path.display(),
            cells,
            limits.max_cells
        );
    }
    Ok(())
}

/// Walk content.xml counting the used area of the widest/tallest table.
///
/// Kept separate from the zip plumbing so it can be tested on a string.
fn scan_ods_cells<R: Read>(reader: R) -> Result<u64> {
    use quick_xml::events::Event;

    let mut xml = quick_xml::Reader::from_reader(BufReader::new(reader));
    // quick-xml's default; stated because this scan counts *cells*, not text,
    // and must not depend on how whitespace between tags is handled.
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();

    let mut worst: u64 = 0;
    // Per table.
    let mut row_cursor: u64 = 0;
    let mut last_content_row: u64 = 0;
    let mut max_col: u64 = 0;
    // Per row.
    let mut col_cursor: u64 = 0;
    let mut row_repeats: u64 = 1;
    let mut row_has_content = false;

    loop {
        let ev = xml.read_event_into(&mut buf);
        match ev {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let empty = matches!(ev, Ok(Event::Empty(_)));
                match e.name().as_ref() {
                    b"table:table" => {
                        row_cursor = 0;
                        last_content_row = 0;
                        max_col = 0;
                    }
                    b"table:table-row" => {
                        row_repeats = repeat_of(e, b"table:number-rows-repeated");
                        col_cursor = 0;
                        row_has_content = false;
                        if empty {
                            row_cursor = row_cursor.saturating_add(row_repeats);
                        }
                    }
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        let reps = repeat_of(e, b"table:number-columns-repeated");
                        // A cell counts as data only if it carries a value.
                        // Valueless runs are the padding LibreOffice writes
                        // out to the edge of the grid.
                        if has_attr(e, b"office:value-type") {
                            row_has_content = true;
                            max_col = max_col.max(col_cursor.saturating_add(reps));
                        }
                        col_cursor = col_cursor.saturating_add(reps);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"table:table-row" => {
                if row_has_content {
                    last_content_row = row_cursor.saturating_add(row_repeats);
                }
                row_cursor = row_cursor.saturating_add(row_repeats);
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"table:table" => {
                worst = worst.max(last_content_row.saturating_mul(max_col));
            }
            Ok(Event::Eof) => break,
            // A malformed or truncated document is calamine's error to
            // report, not ours: stop scanning and let it through.
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    // A table left unclosed by the read cap still counts what was seen.
    worst = worst.max(last_content_row.saturating_mul(max_col));
    Ok(worst)
}

fn has_attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> bool {
    e.attributes()
        .flatten()
        .any(|a| a.key.as_ref() == key)
}

fn repeat_of(e: &quick_xml::events::BytesStart, key: &[u8]) -> u64 {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        // A repeat count of zero would make a bomb invisible to this scan.
        .map(|n| n.max(1))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(xml: &str) -> u64 {
        scan_ods_cells(xml.as_bytes()).unwrap()
    }

    const HEAD: &str = r#"<?xml version="1.0"?><office:document-content
        xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
        xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
        xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
        <office:body><office:spreadsheet>"#;
    const TAIL: &str = "</office:spreadsheet></office:body></office:document-content>";

    fn doc(body: &str) -> String {
        format!("{HEAD}{body}{TAIL}")
    }

    fn cell(v: &str) -> String {
        format!(r#"<table:table-cell office:value-type="float" office:value="{v}"><text:p>{v}</text:p></table:table-cell>"#)
    }

    #[test]
    fn a_plain_table_measures_its_own_size() {
        let row = format!("<table:table-row>{}{}</table:table-row>", cell("1"), cell("2"));
        let body = format!(r#"<table:table table:name="S">{row}{row}</table:table>"#);
        assert_eq!(cells(&doc(&body)), 4); // 2 rows x 2 cols
    }

    /// The bomb: a tiny document declaring an enormous grid.
    #[test]
    fn a_repeated_row_is_counted_at_its_full_size() {
        let body = format!(
            r#"<table:table table:name="S"><table:table-row table:number-rows-repeated="2000000">{}{}</table:table-row></table:table>"#,
            cell("1"),
            cell("2")
        );
        assert_eq!(cells(&doc(&body)), 4_000_000);
    }

    /// The false-positive that would make this guard unusable: LibreOffice
    /// pads every sheet out to the full grid with valueless cells, so a
    /// scan that counted padding would refuse ordinary documents.
    #[test]
    fn trailing_padding_is_not_counted_as_data() {
        let body = format!(
            r#"<table:table table:name="S">
                 <table:table-row>{}</table:table-row>
                 <table:table-row table:number-rows-repeated="1048575">
                   <table:table-cell table:number-columns-repeated="1024"/>
                 </table:table-row>
               </table:table>"#,
            cell("1")
        );
        assert_eq!(cells(&doc(&body)), 1, "padding was counted as data");
    }

    /// A valueless run *between* two values still occupies its columns.
    #[test]
    fn an_interior_empty_run_still_advances_the_column_cursor() {
        let body = format!(
            r#"<table:table table:name="S"><table:table-row>{}<table:table-cell table:number-columns-repeated="3"/>{}</table:table-row></table:table>"#,
            cell("1"),
            cell("5")
        );
        assert_eq!(cells(&doc(&body)), 5, "the gap did not advance the cursor");
    }

    #[test]
    fn the_largest_table_in_the_document_wins() {
        let small = format!("<table:table table:name=\"a\"><table:table-row>{}</table:table-row></table:table>", cell("1"));
        let big = format!(
            r#"<table:table table:name="b"><table:table-row table:number-rows-repeated="100">{}{}</table:table-row></table:table>"#,
            cell("1"),
            cell("2")
        );
        assert_eq!(cells(&doc(&format!("{small}{big}"))), 200);
        assert_eq!(cells(&doc(&format!("{big}{small}"))), 200);
    }

    /// `number-rows-repeated="0"` must not make a bomb invisible.
    #[test]
    fn a_zero_repeat_count_is_treated_as_one() {
        let body = format!(
            r#"<table:table table:name="S"><table:table-row table:number-rows-repeated="0">{}</table:table-row></table:table>"#,
            cell("1")
        );
        assert_eq!(cells(&doc(&body)), 1);
    }

    #[test]
    fn a_document_with_no_tables_is_zero() {
        assert_eq!(cells(&doc("")), 0);
    }
}
