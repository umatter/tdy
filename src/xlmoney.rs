//! Reading currency cell formats directly from the xlsx zip.
//!
//! calamine 0.36 parses `numFmts`/`cellXfs` into a stack-local map while opening a
//! workbook, but its public API keeps only a 3-variant `CellFormat`, and `Cell<T>` carries
//! no style id at all — so "is this cell formatted as currency" is unreachable through
//! calamine once a workbook is open. Read it ourselves instead, exactly as `xlguard` reads
//! workbook geometry straight out of the zip: `xl/styles.xml` gives the numFmtId->currency
//! table (builtin ids and any custom `numFmt`) plus the `cellXfs` style-index->numFmtId
//! table, and the sheet XML's `<c r="D10" s="4">` gives each cell's column and style
//! index. Tally per column; this is evidence for the sniffer, never a census — bounded,
//! best-effort, and a parse failure anywhere here must fall back to "no information" rather
//! than fail the caller, since this is an enhancement over the existing typing, not a new
//! way for it to go wrong.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use quick_xml::events::{BytesStart, Event};

/// Bound on decompressed bytes read from the small package parts (styles,
/// workbook, rels) — generous, since all three are tiny in every real
/// workbook, but bounded anyway because a zip entry's claimed size is not to
/// be trusted (see `xlguard`).
const MAX_PART_BYTES: u64 = 16 * 1024 * 1024;
/// Bound on decompressed bytes read from the sheet itself: a backstop
/// against a malformed document with no closing tags, not the real limit.
const MAX_SHEET_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// Bound on the number of `<c>` cells tallied from the sheet: evidence, not
/// a census — a few thousand cells is ample to decide what a column is.
const MAX_CELLS_SCANNED: usize = 5_000;
/// A column counts as money when at least this share of its counted data
/// cells (every row but the first) carry a currency format.
const MONEY_SHARE: f64 = 0.8;

/// Builtin numFmtIds that mean currency (ECMA-376's fixed format table):
/// 5-8 are the "Currency" builtins, 41-44 are "Accounting" (with or without
/// a literal currency symbol, but always money, per the design's own
/// feasibility notes).
const BUILTIN_CURRENCY_IDS: &[u32] = &[5, 6, 7, 8, 41, 42, 43, 44];

/// Which 0-based column indices (`A`=0) of `sheet_name` in the workbook at
/// `path` are money, judged by the share of that column's data cells
/// carrying a currency number format. Empty on any read or parse failure —
/// this only ever adds information, it never blocks a sniff.
pub(crate) fn money_columns(path: &Path, sheet_name: &str) -> HashSet<usize> {
    money_columns_inner(path, sheet_name).unwrap_or_default()
}

fn money_columns_inner(path: &Path, sheet_name: &str) -> Option<HashSet<usize>> {
    let f = File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f)).ok()?;

    let styles_xml = read_part(&mut zip, "xl/styles.xml")?;
    let currency_styles = currency_style_indices(&styles_xml);
    if currency_styles.is_empty() {
        // Nothing in the whole workbook is currency-formatted; no need to
        // touch the (potentially much larger) sheet part at all.
        return Some(HashSet::new());
    }

    let workbook_xml = read_part(&mut zip, "xl/workbook.xml")?;
    let rels_xml = read_part(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let target = resolve_sheet_target(&workbook_xml, &rels_xml, sheet_name)?;
    let part = normalize_sheet_part(&target);

    let entry = zip.by_name(&part).ok()?;
    let tally = tally_from_reader(entry.take(MAX_SHEET_SCAN_BYTES), &currency_styles, MAX_CELLS_SCANNED);
    Some(money_columns_from_tally(&tally))
}

/// Read a small package part fully, bounded — never so large it matters for
/// styles.xml/workbook.xml/rels, but bounded on principle: an untrusted zip
/// entry's claimed size proves nothing (see `xlguard::check_zip_expansion`).
fn read_part<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let entry = zip.by_name(name).ok()?;
    let mut buf = String::new();
    entry.take(MAX_PART_BYTES).read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn money_columns_from_tally(tally: &BTreeMap<usize, (u32, u32)>) -> HashSet<usize> {
    tally
        .iter()
        .filter(|(_, (currency, total))| {
            *total > 0 && f64::from(*currency) / f64::from(*total) >= MONEY_SHARE
        })
        .map(|(col, _)| *col)
        .collect()
}

// ---------------------------------------------------------------------------
// styles.xml: cellXfs (style index -> numFmtId) and custom numFmts
// ---------------------------------------------------------------------------

fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// `<cellXfs><xf numFmtId="164" .../>...</cellXfs>` in document order — the
/// vec's position *is* the style index a cell's `s="…"` refers to.
/// `<cellStyleXfs>` has the same element name and must not be counted.
fn parse_cellxfs(xml: &str) -> Vec<u32> {
    let mut reader = quick_xml::Reader::from_reader(xml.as_bytes());
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut in_cell_xfs = false;
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"cellXfs" => in_cell_xfs = true,
            Ok(Event::End(ref e)) if e.name().as_ref() == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if in_cell_xfs && e.name().as_ref() == b"xf" =>
            {
                let id = attr(e, b"numFmtId").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                out.push(id);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// `<numFmts><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/></numFmts>` —
/// the custom formats a workbook defines above id 163. `"$"` survives raw
/// (only `<>&'"` are escaped in XML), so the currency check below never
/// needs to unescape the entity.
fn parse_custom_numfmts(xml: &str) -> HashMap<u32, String> {
    let mut reader = quick_xml::Reader::from_reader(xml.as_bytes());
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.name().as_ref() == b"numFmt" =>
            {
                if let (Some(id), Some(code)) =
                    (attr(e, b"numFmtId").and_then(|v| v.parse::<u32>().ok()), attr(e, b"formatCode"))
                {
                    out.insert(id, code);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// A format code means currency if it is one of the builtin ids, or a
/// custom one carrying a currency symbol or the `[$…]` locale-currency
/// construct — but never a percent or date/time format, which can
/// legitimately contain digits-and-punctuation patterns that would
/// otherwise look numeric-money-ish.
fn is_currency_format_code(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    if lower.contains('%') {
        return false;
    }
    const DATE_TIME_TOKENS: &[&str] = &["yy", "mm", "dd", "hh", "ss"];
    if DATE_TIME_TOKENS.iter().any(|t| lower.contains(t)) {
        return false;
    }
    code.contains('$') || code.contains('€') || code.contains('£') || code.contains('¥') || code.contains("[$")
}

fn is_currency_numfmt(id: u32, custom: &HashMap<u32, String>) -> bool {
    BUILTIN_CURRENCY_IDS.contains(&id)
        || custom.get(&id).is_some_and(|code| is_currency_format_code(code))
}

/// The set of `cellXfs` style indices that are currency-formatted.
fn currency_style_indices(styles_xml: &str) -> HashSet<u32> {
    let cellxfs = parse_cellxfs(styles_xml);
    let custom = parse_custom_numfmts(styles_xml);
    cellxfs
        .iter()
        .enumerate()
        .filter(|(_, &fmt_id)| is_currency_numfmt(fmt_id, &custom))
        .map(|(idx, _)| idx as u32)
        .collect()
}

// ---------------------------------------------------------------------------
// workbook.xml + workbook.xml.rels: sheet name -> worksheet zip part
// ---------------------------------------------------------------------------

/// `(sheet name, r:id)` pairs from `<sheets><sheet name="…" r:id="rIdN"/>…</sheets>`,
/// in document order.
fn parse_sheet_entries(workbook_xml: &str) -> Vec<(String, String)> {
    let mut reader = quick_xml::Reader::from_reader(workbook_xml.as_bytes());
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) if e.name().as_ref() == b"sheet" => {
                if let (Some(name), Some(rid)) = (attr(e, b"name"), attr(e, b"r:id")) {
                    out.push((name, rid));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// The `Target` of `<Relationship Id="rIdN" Target="…"/>` matching `rid`.
fn rel_target(rels_xml: &str, rid: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_reader(rels_xml.as_bytes());
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut found = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.name().as_ref() == b"Relationship" =>
            {
                if attr(e, b"Id").as_deref() == Some(rid) {
                    found = attr(e, b"Target");
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    found
}

/// The zip part name (e.g. `xl/worksheets/sheet1.xml`) for `sheet_name`.
/// Falls back to a case-insensitive match — Excel itself treats sheet names
/// case-insensitively — before giving up.
fn resolve_sheet_target(workbook_xml: &str, rels_xml: &str, sheet_name: &str) -> Option<String> {
    let entries = parse_sheet_entries(workbook_xml);
    let rid = entries
        .iter()
        .find(|(n, _)| n == sheet_name)
        .or_else(|| entries.iter().find(|(n, _)| n.eq_ignore_ascii_case(sheet_name)))
        .map(|(_, r)| r.clone())?;
    rel_target(rels_xml, &rid)
}

/// A `Target` is package-relative to `xl/` and sometimes written with a
/// leading `/` as a package-absolute path (both forms are valid OPC).
fn normalize_sheet_part(target: &str) -> String {
    let t = target.trim_start_matches('/');
    if let Some(rest) = t.strip_prefix("xl/") {
        format!("xl/{rest}")
    } else {
        format!("xl/{t}")
    }
}

// ---------------------------------------------------------------------------
// The sheet itself: <c r="D10" s="4"> -> (column, style index)
// ---------------------------------------------------------------------------

/// Split a cell reference like `"D10"` into a 0-based column index and the
/// (1-based) row number.
fn parse_cell_ref(r: &str) -> Option<(usize, u32)> {
    let split_at = r.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = r.split_at(split_at);
    if letters.is_empty() || digits.is_empty() {
        return None;
    }
    let row: u32 = digits.parse().ok()?;
    let mut col: u64 = 0;
    for c in letters.chars() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + u64::from(c.to_ascii_uppercase()) - u64::from(b'A') + 1;
    }
    Some(((col - 1) as usize, row))
}

/// Stream the sheet, tallying `(currency_cells, total_cells)` per column
/// over every `<c>` past row 1 (the header), stopping after `max_cells`
/// regardless of how much of the sheet remains — the tally is evidence, not
/// a census, and this is the bound that keeps it that way.
fn tally_from_reader<R: Read>(
    reader: R,
    currency_styles: &HashSet<u32>,
    max_cells: usize,
) -> BTreeMap<usize, (u32, u32)> {
    let mut xml = quick_xml::Reader::from_reader(BufReader::new(reader));
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut tally: BTreeMap<usize, (u32, u32)> = BTreeMap::new();
    let mut scanned = 0usize;
    loop {
        if scanned >= max_cells {
            break;
        }
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) if e.name().as_ref() == b"c" => {
                let Some(r) = attr(e, b"r") else { buf.clear(); continue };
                let Some((col, row)) = parse_cell_ref(&r) else { buf.clear(); continue };
                if row <= 1 {
                    buf.clear();
                    continue;
                }
                let style: u32 = attr(e, b"s").and_then(|v| v.parse().ok()).unwrap_or(0);
                let entry = tally.entry(col).or_insert((0, 0));
                entry.1 += 1;
                if currency_styles.contains(&style) {
                    entry.0 += 1;
                }
                scanned += 1;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const STYLES: &str = r#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
        <numFmts count="1"><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/></numFmts>
        <cellStyleXfs count="1"><xf numFmtId="9" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
        <cellXfs count="3">
            <xf numFmtId="0" fontId="0" fillId="0" borderId="0"/>
            <xf numFmtId="164" fontId="0" fillId="0" borderId="0"/>
            <xf numFmtId="44" fontId="0" fillId="0" borderId="0"/>
        </cellXfs>
        </styleSheet>"#;

    #[test]
    fn cellstylexfs_is_not_mistaken_for_cellxfs() {
        // cellStyleXfs's numFmtId="9" (percent) must not leak into the
        // cellXfs index space, or style index 1 would mean the wrong thing.
        let xfs = parse_cellxfs(STYLES);
        assert_eq!(xfs, vec![0, 164, 44]);
    }

    #[test]
    fn custom_and_builtin_currency_formats_are_both_found() {
        let styles = currency_style_indices(STYLES);
        // style 1 -> numFmtId 164, a custom "$"#,##0.00" format.
        // style 2 -> numFmtId 44, a builtin accounting format.
        assert_eq!(styles, HashSet::from([1, 2]));
    }

    #[test]
    fn percent_and_date_formats_are_not_currency() {
        assert!(!is_currency_format_code("0.00%"));
        assert!(!is_currency_format_code("$#,##0%")); // absurd, but % wins
        assert!(!is_currency_format_code("yyyy-mm-dd"));
        assert!(!is_currency_format_code("mm/dd/yyyy"));
        assert!(is_currency_format_code("\"$\"#,##0.00"));
        assert!(is_currency_format_code("[$€-407]#,##0.00"));
        assert!(is_currency_format_code("£#,##0.00"));
    }

    #[test]
    fn cell_refs_decode_column_letters_including_double_letters() {
        assert_eq!(parse_cell_ref("A1"), Some((0, 1)));
        assert_eq!(parse_cell_ref("D10"), Some((3, 10)));
        assert_eq!(parse_cell_ref("Z1"), Some((25, 1)));
        assert_eq!(parse_cell_ref("AA1"), Some((26, 1)));
        assert_eq!(parse_cell_ref("not-a-ref"), None);
    }

    #[test]
    fn header_row_is_excluded_from_the_tally() {
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>amount</t></is></c></row>
            <row r="2"><c r="A2" s="1" t="n"><v>1.00</v></c></row>
            <row r="3"><c r="A3" s="1" t="n"><v>2.00</v></c></row>
        </sheetData></worksheet>"#;
        let currency = HashSet::from([1u32]);
        let tally = tally_from_reader(Cursor::new(sheet.as_bytes()), &currency, 5_000);
        assert_eq!(tally.get(&0), Some(&(2, 2)));
    }

    #[test]
    fn a_column_below_the_money_share_is_not_money() {
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="A1"/></row>
            <row r="2"><c r="A2" s="1" t="n"><v>1</v></c></row>
            <row r="3"><c r="A3" s="0" t="n"><v>2</v></c></row>
            <row r="4"><c r="A4" s="0" t="n"><v>3</v></c></row>
            <row r="5"><c r="A5" s="0" t="n"><v>4</v></c></row>
            <row r="6"><c r="A6" s="0" t="n"><v>5</v></c></row>
        </sheetData></worksheet>"#;
        let currency = HashSet::from([1u32]);
        let tally = tally_from_reader(Cursor::new(sheet.as_bytes()), &currency, 5_000);
        assert_eq!(money_columns_from_tally(&tally), HashSet::new());
    }

    #[test]
    fn the_scan_stops_after_max_cells() {
        let mut sheet = String::from("<worksheet><sheetData>");
        for r in 2..2100 {
            sheet.push_str(&format!(r#"<row r="{r}"><c r="A{r}" s="1" t="n"><v>1</v></c></row>"#));
        }
        sheet.push_str("</sheetData></worksheet>");
        let currency = HashSet::from([1u32]);
        let tally = tally_from_reader(Cursor::new(sheet.as_bytes()), &currency, 100);
        assert_eq!(tally.get(&0), Some(&(100, 100)));
    }

    #[test]
    fn sheet_target_resolves_through_workbook_and_rels() {
        let workbook = r#"<workbook><sheets>
            <sheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
                   name="Sheet1" sheetId="1" r:id="rId1"/>
        </sheets></workbook>"#;
        let rels = r#"<Relationships>
            <Relationship Id="rId1" Target="/xl/worksheets/sheet1.xml"/>
            <Relationship Id="rId2" Target="styles.xml"/>
        </Relationships>"#;
        assert_eq!(resolve_sheet_target(workbook, rels, "Sheet1"), Some("/xl/worksheets/sheet1.xml".into()));
        assert_eq!(normalize_sheet_part("/xl/worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        // Excel treats sheet names case-insensitively.
        assert_eq!(resolve_sheet_target(workbook, rels, "sheet1").as_deref(), Some("/xl/worksheets/sheet1.xml"));
        assert_eq!(resolve_sheet_target(workbook, rels, "NoSuchSheet"), None);
    }

    #[test]
    fn a_relative_target_is_joined_under_xl() {
        assert_eq!(normalize_sheet_part("worksheets/sheet2.xml"), "xl/worksheets/sheet2.xml");
    }

    #[test]
    fn end_to_end_against_the_real_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/xl_money_siblings.xlsx");
        let cols = money_columns(&path, "Sheet1");
        // agency_name(0) is text; added(1), amount_a(2), amount_b(3),
        // amount_c(4) all carry the same "$"#,##0.00" format in every data
        // row.
        assert_eq!(cols, HashSet::from([1, 2, 3, 4]), "{cols:?}");
    }

    #[test]
    fn a_non_xlsx_file_yields_no_information() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/umsatz.xlsx");
        // Not zip-broken, just exercising the "never fail the caller" path
        // with a real workbook that has no currency formatting at all.
        let cols = money_columns(&path, "does-not-exist");
        assert_eq!(cols, HashSet::new());
    }
}
