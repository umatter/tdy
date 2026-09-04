//! FileSample: what both the heuristic sniffer and the LLM get to look at.
//!
//! Three properties this has to get right:
//!
//! - **Bounded.** A sample is a fixed number of kilobytes, and reading it
//!   costs a fixed number of kilobytes. Reading a whole 2 GB export to show a
//!   model 16 KB of it is the difference between a tool that feels instant and
//!   one that swaps.
//! - **Rendered, never raw, for binary formats.** An .xlsx is a zip archive;
//!   its head bytes say nothing. Excel is rendered to a grid via calamine, in
//!   exactly the string form the extractor will later produce.
//! - **Cut on line boundaries.** A sample that ends mid-line shows the sniffer
//!   a row with the wrong number of fields, which is precisely the signal it
//!   uses to choose a delimiter.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Limits;
use calamine::{open_workbook_auto, Data, Reader};

use crate::fileio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatGuess {
    Delimited,
    Excel,
    Json,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct FileSample {
    pub file_name: String,
    pub bytes: u64,
    pub format: FormatGuess,
    /// Detected encoding label (text formats only).
    pub encoding: Option<String>,
    /// True when every sampled byte is ASCII. Encoding detection over
    /// ASCII-only bytes is meaningless — every candidate agrees — so a guess
    /// made here must not be frozen into the spec and applied to the parts of
    /// the file we never looked at.
    pub ascii_only: bool,
    /// Rendered sample: decoded head(+tail) for text, a grid for Excel.
    pub body: String,
    /// For Excel: per-sheet name list.
    pub sheets: Vec<String>,
    /// How many raw bytes informed `body` (recorded in provenance).
    pub sampled_bytes: u64,
    /// True when the file is larger than what `body` shows.
    pub partial: bool,
}

pub const CONTINUES_MARKER: &str = "\n[... file continues ...]\n";

pub fn guess_format(path: &Path) -> FormatGuess {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("csv") | Some("tsv") | Some("psv") | Some("txt") | Some("dat") | Some("log")
        | Some("out") | Some("tab") => FormatGuess::Delimited,
        Some("xlsx") | Some("xls") | Some("xlsb") | Some("xlsm") | Some("ods") => {
            FormatGuess::Excel
        }
        Some("json") | Some("ndjson") | Some("jsonl") => FormatGuess::Json,
        _ => FormatGuess::Unknown,
    }
}

pub fn detect_encoding(bytes: &[u8]) -> &'static encoding_rs::Encoding {
    // UTF-16 first: ASCII text encoded as UTF-16 is *valid UTF-8* (a NUL byte
    // is a legal UTF-8 character), so the UTF-8 check below would accept it
    // and hand back a string full of NULs.
    if let Some(enc) = utf16_flavour(bytes) {
        return enc;
    }
    // Valid UTF-8 is UTF-8. chardetng is a statistical guesser and will
    // cheerfully call a short ASCII file windows-1252; freezing that guess
    // then mangles every multi-byte character later in the file.
    //
    // The bytes handed to us are a SAMPLE: `build` concatenates a head with a
    // tail read from an arbitrary offset, so a multi-byte character can be torn
    // at the tail's start and another truncated at the very end. Neither says
    // anything about the file's encoding, and treating a torn sample as
    // "not UTF-8" is how a valid UTF-8 file acquired windows-1252 mojibake.
    if is_confidently_utf8(bytes) {
        return encoding_rs::UTF_8;
    }
    let mut det = chardetng::EncodingDetector::new();
    for w in evidence_windows(bytes) {
        det.feed(w, false);
    }
    det.feed(b"", true);
    det.guess(None, true)
}

/// Is this sample valid UTF-8 once truncation at its edges is discounted?
///
/// Skips leading continuation bytes (0x80..=0xBF, at most 3 — a torn character
/// at the start of a tail sample) and tolerates an incomplete sequence at the
/// very end (`Utf8Error::error_len() == None`, which means "valid so far, ran
/// out of bytes"). An *interior* error is a real signal and returns false.
///
/// A buffer that is *nothing but* those leading continuation bytes is not
/// evidence of anything: trimming them away would leave an empty remainder,
/// which trivially "validates", so a lone stray high byte — exactly what a
/// 1-3 byte tail window can hold — would be waved through as UTF-8 having had
/// nothing actually checked. That buffer is refused instead.
fn is_utf8_apart_from_torn_edges(bytes: &[u8]) -> bool {
    let start = bytes.iter().take(3).take_while(|b| (0x80..=0xBF).contains(*b)).count();
    if start > 0 && bytes[start..].is_empty() {
        return false;
    }
    match std::str::from_utf8(&bytes[start..]) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && e.valid_up_to() > 0,
    }
}

/// Would `detect_encoding` classify this buffer as UTF-8 without falling
/// back to the statistical guesser? Composes the same two checks
/// `detect_encoding` makes (UTF-16 first, then edge-tolerant UTF-8) into one
/// place, so `build`'s head+tail fast path — which must check each piece
/// against its own edges, never the artificial seam between the two pieces —
/// reuses this exact reasoning per piece instead of restating a shortened
/// version of it that could drift from `detect_encoding` over time.
fn is_confidently_utf8(bytes: &[u8]) -> bool {
    utf16_flavour(bytes).is_none() && is_utf8_apart_from_torn_edges(bytes)
}

/// Bounded slices of `bytes` that carry the encoding evidence.
///
/// chardetng costs real time per byte — measured at ~0.15 s/MB, which turned
/// one 22 MB latin-1 CSV into 3.4 s of detection per read, twice per query on
/// the streaming path — and ASCII bytes tell it nothing. So instead of
/// feeding the whole file, feed windows around the non-ASCII bytes: each
/// window opens a little early (so the multi-byte sequence around its trigger
/// is intact) and the total is capped. A file whose evidence all fits is
/// detected from exactly what the full scan would have used; a pathological
/// one loses only the evidence past the cap, where the full scan's extra
/// bytes were overwhelmingly ASCII anyway.
fn evidence_windows(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    const WINDOW: usize = 4096;
    const MARGIN: usize = 64;
    const BUDGET: usize = 256 * 1024;

    let mut spent = 0usize;
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if spent >= BUDGET || pos >= bytes.len() {
            return None;
        }
        // memchr-style scan: find the next byte that is evidence.
        let off = bytes[pos..].iter().position(|b| !b.is_ascii())?;
        let hit = pos + off;
        let start = hit.saturating_sub(MARGIN).max(pos);
        let end = (start + WINDOW).min(bytes.len());
        pos = end;
        spent += end - start;
        Some(&bytes[start..end])
    })
}

/// Recognise UTF-16 by its BOM, or by the tell-tale run of NUL bytes in
/// alternating positions that ASCII text produces in either byte order.
pub(crate) fn utf16_flavour(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(encoding_rs::UTF_16LE);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(encoding_rs::UTF_16BE);
    }
    let probe = &bytes[..bytes.len().min(4096)];
    if probe.len() < 16 {
        return None;
    }
    let even_nul = probe.iter().step_by(2).filter(|b| **b == 0).count();
    let odd_nul = probe.iter().skip(1).step_by(2).filter(|b| **b == 0).count();
    let half = probe.len() / 2;
    if odd_nul * 4 > half * 3 && even_nul == 0 {
        return Some(encoding_rs::UTF_16LE);
    }
    if even_nul * 4 > half * 3 && odd_nul == 0 {
        return Some(encoding_rs::UTF_16BE);
    }
    None
}

pub fn decode_text(bytes: &[u8], label: Option<&str>) -> (String, String) {
    let (text, name, _) = decode_reporting(bytes, label);
    (text, name)
}

/// As [`decode_text`], but also reporting whether the decoder had to
/// substitute replacement characters. A declared encoding that produces them
/// is a wrong declaration, and the resulting mojibake is exactly the kind of
/// quiet corruption the rest of this tool refuses to commit.
pub fn decode_reporting(bytes: &[u8], label: Option<&str>) -> (String, String, bool) {
    let enc = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or_else(|| detect_encoding(bytes));
    // `decode` also removes a byte order mark when one is present.
    let (text, _, had_errors) = enc.decode(bytes);
    (text.into_owned(), enc.name().to_ascii_lowercase(), had_errors)
}

/// Decode an owned buffer, reusing its allocation when the bytes are already
/// UTF-8 — which they are for most files. Copying a 2 GB export a second time
/// just to change the type that points at it is the difference between one
/// and two gigabytes of peak memory.
pub fn decode_owned(bytes: Vec<u8>, label: Option<&str>) -> (String, String, bool) {
    let declared = label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()));
    let is_utf8 = match declared {
        Some(enc) => enc == encoding_rs::UTF_8,
        None => utf16_flavour(&bytes).is_none(), // not UTF-16 masquerading as UTF-8
    };
    if is_utf8 && !bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        match String::from_utf8(bytes) {
            Ok(text) => return (text, "utf-8".to_string(), false),
            Err(e) => {
                let bytes = e.into_bytes();
                return decode_reporting(&bytes, label);
            }
        }
    }
    decode_reporting(&bytes, label)
}

pub fn build(path: &Path, max_bytes: usize, limits: Limits) -> Result<FileSample> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if meta.is_dir() {
        anyhow::bail!(
            "{} is a directory; point tdy at a data file inside it",
            path.display()
        );
    }
    let format = guess_format(path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    if format == FormatGuess::Excel {
        return build_excel_sample(path, file_name, meta.len(), max_bytes, limits);
    }

    let head_budget = (max_bytes * 3 / 4).max(512);
    let tail_budget = max_bytes.saturating_sub(head_budget);
    let ht = fileio::read_head_tail(path, head_budget, tail_budget)?;

    let head_ascii = ht.head.iter().all(|b| b.is_ascii());
    let tail_ascii = ht.tail.as_ref().map(|t| t.iter().all(|b| b.is_ascii())).unwrap_or(true);
    let ascii_only = head_ascii && tail_ascii;
    // Judge the encoding on every byte we looked at. Detecting from the head
    // alone and then recording that label because the *tail* was non-ASCII
    // would freeze a guess made from evidence that contained none.
    let enc_name = match (&ht.tail, ascii_only) {
        (Some(tail), false) => {
            // `read_head_tail` seeks the tail to an arbitrary byte offset, so
            // it can begin mid multi-byte sequence even when the file is
            // valid UTF-8 throughout. Fusing the raw bytes and validating the
            // fused buffer manufactures a false tear right at the seam — the
            // *file* is fine, the concatenation isn't — which skips the
            // "valid UTF-8 is UTF-8" check and freezes a wrong chardetng
            // guess. Check each piece against its own edges instead of the
            // artificial seam between them, through the same predicate
            // `detect_encoding` itself uses (`is_confidently_utf8`), so the
            // two decisions cannot drift apart.
            if is_confidently_utf8(&ht.head) && is_confidently_utf8(tail) {
                encoding_rs::UTF_8.name().to_ascii_lowercase()
            } else {
                let mut both = ht.head.clone();
                both.extend_from_slice(tail);
                decode_text(&both, None).1
            }
        }
        _ => decode_text(&ht.head, None).1,
    };
    let (head_text, _) = decode_text(&ht.head, Some(&enc_name));

    // Drop a torn final line: a half-row would look like a row with the wrong
    // number of fields, which is exactly what delimiter detection counts.
    let more_follows = ht.total > ht.head.len() as u64;
    let mut body = if more_follows {
        match head_text.rfind('\n') {
            Some(i) => head_text[..=i].to_string(),
            None => head_text.clone(),
        }
    } else {
        head_text
    };

    if let Some(tail_bytes) = &ht.tail {
        let (tail, _) = decode_text(tail_bytes, Some(&enc_name));
        // Same at the start of the tail.
        let tail = tail.split_once('\n').map(|(_, rest)| rest).unwrap_or("").to_string();
        if !tail.trim().is_empty() {
            body.push_str(CONTINUES_MARKER);
            body.push_str(&tail);
        }
    }

    Ok(FileSample {
        file_name,
        bytes: meta.len(),
        format,
        encoding: Some(enc_name),
        ascii_only,
        body,
        sheets: vec![],
        sampled_bytes: ht.sampled,
        partial: more_follows,
    })
}

fn build_excel_sample(
    path: &Path,
    file_name: String,
    bytes: u64,
    max_bytes: usize,
    limits: Limits,
) -> Result<FileSample> {
    // Rendering a sample means opening the workbook, and for .ods that alone
    // allocates the whole grid. Bound it first. See src/xlguard.rs.
    crate::xlguard::preflight(path, &limits)?;
    let mut wb = open_workbook_auto(path)
        .with_context(|| format!("cannot open workbook {}", path.display()))?;
    let sheets: Vec<String> = wb.sheet_names().to_vec();
    let mut body = String::new();
    let mut partial = false;
    // Render the first two sheets (usually enough context to pick one), but
    // never more than the configured sample size: this text goes into an LLM
    // prompt, and `sample_bytes` is the user's statement about how much of
    // their file may leave the machine.
    for name in sheets.iter().take(2) {
        if body.len() >= max_bytes {
            partial = true;
            break;
        }
        // Same check the executor uses: a sheet whose declared extent is
        // over the limit is skipped, not rendered — the workbook may still
        // have a sheet worth showing the model.
        let range = match crate::engine::checked_worksheet_range(&mut wb, name, &limits) {
            Ok(r) => r,
            Err(_) => continue,
        };
        body.push_str(&format!(
            "=== sheet {:?} ({} rows x {} cols) ===\n",
            name,
            range.height(),
            range.width()
        ));
        let mut shown = 0usize;
        for (i, row) in range.rows().take(40).enumerate() {
            if body.len() >= max_bytes {
                partial = true;
                break;
            }
            let cells: Vec<String> = row.iter().take(24).map(render_cell).collect();
            body.push_str(&format!("r{:<3}| {}\n", i, cells.join(" | ")));
            shown = i + 1;
        }
        if range.height() > shown {
            body.push_str(&format!("[... {} more rows ...]\n", range.height() - shown));
            partial = true;
        }
    }
    if body.len() > max_bytes {
        // Cut on a line boundary so the grid stays readable — and on a char
        // boundary, because slicing a String mid-character is a panic and an
        // Excel grid is full of multi-byte characters.
        let mut end = max_bytes;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        let cut = body[..end].rfind('\n').unwrap_or(0);
        body.truncate(cut);
        body.push_str("\n[... sample truncated ...]\n");
        partial = true;
    }
    let sampled = body.len() as u64;
    Ok(FileSample {
        file_name,
        bytes,
        format: FormatGuess::Excel,
        encoding: None,
        ascii_only: body.is_ascii(),
        body,
        sheets,
        sampled_bytes: sampled,
        partial,
    })
}

/// Render a calamine cell to the same string form the extractor will
/// produce, so the model reasons about exactly what the executor sees.
pub fn render_cell(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Int(i) => format!("{i}"),
        Data::Bool(b) => format!("{b}"),
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|d| {
                if d.time() == chrono::NaiveTime::MIN {
                    d.format("%Y-%m-%d").to_string()
                } else {
                    d.format("%Y-%m-%d %H:%M:%S").to_string()
                }
            })
            .unwrap_or_else(|| format!("{}", dt.as_f64())),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR:{e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, name: &str, body: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    #[test]
    fn valid_utf8_is_never_mistaken_for_latin1() {
        assert_eq!(detect_encoding(b"plain ascii").name(), "UTF-8");
        assert_eq!(detect_encoding("Zürich".as_bytes()).name(), "UTF-8");
        // Not valid UTF-8: fall back to the statistical guesser.
        assert_ne!(detect_encoding(b"M\xfcller").name(), "UTF-8");
    }

    #[test]
    fn a_torn_multibyte_char_at_a_sample_boundary_is_still_utf8() {
        // A tail sample that begins in the middle of "à" (0xC3 0xA0): the leading
        // 0xA0 is a continuation byte, so the buffer is not valid UTF-8 on its
        // own — but the FILE is, and freezing windows-1252 here mangles every
        // accented value. `detect_encoding` is checked on the tail alone
        // (what `is_utf8_apart_from_torn_edges` actually tolerates); fusing
        // an unrelated head onto it before validating would manufacture a
        // *different*, un-anchored tear at the seam that no pure function of
        // bytes can safely tell apart from a genuine interior encoding error
        // (see `a_late_non_ascii_byte_is_still_decoded_correctly` in
        // tests/fixtures.rs, which pins exactly that distinction: a single
        // stray byte in the *middle* of an otherwise-ASCII sample must NOT be
        // waved through as "torn"). `build()`'s fix is to check the head and
        // tail pieces independently rather than validate their concatenation.
        let torn_tail = &[0xA0u8, b' ', b'd', b'e', b' ', b'L', 0xC3, 0xB2, b'r', b'i', b'a', b'\n'];
        assert!((0x80..=0xBF).contains(&torn_tail[0]), "precondition: buffer starts torn");
        assert_eq!(detect_encoding(torn_tail).name(), "UTF-8");
    }

    #[test]
    fn a_truncated_trailing_sequence_is_still_utf8() {
        // The head can end mid-character too.
        let mut b = "Zürich, Genève, Basel".as_bytes().to_vec();
        b.truncate(b.len() - 1); // cut the last byte of "è"
        assert_eq!(detect_encoding(&b).name(), "UTF-8");
    }

    /// End-to-end: `build()` samples a file whose tail read (at the seek
    /// offset `read_head_tail` computes) genuinely begins mid multi-byte
    /// character. The file is valid UTF-8 throughout; before the fix,
    /// `build` fused head+tail into one buffer, `detect_encoding` saw an
    /// interior tear at the seam, and the sample was frozen as windows-1252.
    #[test]
    fn a_file_whose_tail_sample_begins_mid_character_still_samples_as_utf8() {
        let d = tempfile::TempDir::new().unwrap();
        // max_bytes = 64 -> head_budget = 48, tail_budget = 16.
        // "é" (0xC3 0xA9) sits at byte offset 100-101; total length 117 makes
        // the tail start (117 - 16 = 101) land exactly on the continuation
        // byte 0xA9, tearing the character in two.
        let mut body = vec![b'x'; 100];
        body.extend_from_slice("é".as_bytes());
        body.extend_from_slice(&[b'y'; 15]);
        assert_eq!(body.len(), 117);
        let p = write(&d, "torn.csv", &body);

        let s = build(&p, 64, Limits::default()).unwrap();
        assert!(!s.ascii_only, "test precondition: tail must be non-ASCII");
        assert_eq!(s.encoding.as_deref(), Some("utf-8"), "torn tail was mistaken for another encoding");
    }

    #[test]
    fn a_buffer_of_only_continuation_bytes_is_not_utf8() {
        // A 1-2 byte tail window can hold a single stray windows-1252 byte.
        // Trimming it away and validating the empty remainder would call that
        // file UTF-8 and mangle every high byte in it.
        assert!(!is_utf8_apart_from_torn_edges(&[0x92]));
        assert!(!is_utf8_apart_from_torn_edges(&[0x92, 0x93]));
        assert!(!is_utf8_apart_from_torn_edges(&[0x92, 0x93, 0x94]));
    }

    /// `build()`-level reproduction: `sample_bytes = 513` (one past the
    /// config-enforced minimum of 512) gives `head_budget =
    /// (513*3/4).max(512) = 512` and `tail_budget = 1`, so the *entire* tail
    /// sample can be a single stray high byte. Before the fix to
    /// `is_utf8_apart_from_torn_edges`, checking that 1-byte tail on its own
    /// trimmed it away as "torn" and validated the empty remainder, so this
    /// windows-1252-only file was recorded `encoding = "utf-8"` — the exact
    /// mirror of the `enc_late_1252_byte.csv` fixture this fix has to keep
    /// passing.
    #[test]
    fn a_tiny_tail_window_holding_only_a_stray_byte_is_not_utf8() {
        let d = tempfile::TempDir::new().unwrap();
        let mut body = vec![b'x'; 599];
        body.push(0x92); // windows-1252 right single quote, alone in the tail
        assert_eq!(body.len(), 600);
        let p = write(&d, "tiny_tail.csv", &body);

        let s = build(&p, 513, Limits::default()).unwrap();
        assert!(!s.ascii_only, "test precondition: tail must be non-ASCII");
        assert_ne!(
            s.encoding.as_deref(),
            Some("utf-8"),
            "a lone stray byte filling a 1-byte tail window was waved through as UTF-8"
        );
    }

    /// The regression behind `evidence_windows`: a 22 MB CSV whose only
    /// non-ASCII byte sits megabytes in fed the whole file to chardetng and
    /// cost 3.4 s per read. Detection must see the evidence — wherever it is —
    /// while feeding the detector a bounded number of bytes.
    #[test]
    fn detection_evidence_is_found_late_and_stays_bounded() {
        // 2 MB of ASCII with one windows-1252 ö near the end.
        let mut bytes = b"taxon,name,weight\n".repeat(120_000);
        let at = bytes.len() - 100;
        bytes[at] = 0xF6;
        assert_eq!(detect_encoding(&bytes).name(), "windows-1252");

        let fed: usize = evidence_windows(&bytes).map(|w| w.len()).sum();
        assert!(fed <= 256 * 1024, "fed {fed} bytes; the budget is the point");
        // …and the window actually contains the evidence.
        assert!(evidence_windows(&bytes).any(|w| w.contains(&0xF6)));

        // Scattered evidence: every window is picked up until the budget.
        let mut scattered = vec![b'a'; 1 << 20];
        for i in (0..scattered.len()).step_by(100_000) {
            scattered[i] = 0xE9; // é in windows-1252
        }
        let hits = evidence_windows(&scattered)
            .map(|w| w.iter().filter(|b| !b.is_ascii()).count())
            .sum::<usize>();
        assert!(hits >= 10, "only {hits} of the 11 evidence bytes were seen");
    }

    #[test]
    fn bom_is_stripped() {
        let (text, _) = decode_text("\u{feff}id,name\n".as_bytes(), None);
        assert_eq!(text, "id,name\n");
        assert!(!text.starts_with('\u{feff}'));
    }

    #[test]
    fn sample_of_a_large_file_reads_only_the_ends() {
        let d = tempfile::TempDir::new().unwrap();
        let mut body = String::from("a,b,c\n");
        for i in 0..200_000 {
            body.push_str(&format!("{i},{i},{i}\n"));
        }
        let p = write(&d, "big.csv", body.as_bytes());
        let total = std::fs::metadata(&p).unwrap().len();
        let s = build(&p, 16 * 1024, Limits::default()).unwrap();
        assert!(s.partial);
        assert!(
            s.sampled_bytes < 20 * 1024,
            "sampled {} bytes of a {total}-byte file",
            s.sampled_bytes
        );
        assert!(s.body.contains(CONTINUES_MARKER));
    }

    #[test]
    fn the_head_of_a_sample_ends_on_a_line_boundary() {
        let d = tempfile::TempDir::new().unwrap();
        let mut body = String::new();
        for i in 0..5000 {
            body.push_str(&format!("{i},aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"));
        }
        let p = write(&d, "lines.csv", body.as_bytes());
        let s = build(&p, 2048, Limits::default()).unwrap();
        let head = s.body.split(CONTINUES_MARKER).next().unwrap();
        assert!(head.ends_with('\n'), "head must not end mid-line: {:?}", &head[head.len().saturating_sub(40)..]);
        for line in head.lines() {
            assert_eq!(line.split(',').count(), 2, "torn line: {line:?}");
        }
    }

    #[test]
    fn a_small_file_is_shown_whole() {
        let d = tempfile::TempDir::new().unwrap();
        let p = write(&d, "small.csv", b"a,b\n1,2\n");
        let s = build(&p, 16 * 1024, Limits::default()).unwrap();
        assert_eq!(s.body, "a,b\n1,2\n");
        assert!(!s.partial);
        assert!(s.ascii_only);
    }

    #[test]
    fn ascii_only_is_reported() {
        let d = tempfile::TempDir::new().unwrap();
        let p = write(&d, "u.csv", "a\nZürich\n".as_bytes());
        assert!(!build(&p, 16 * 1024, Limits::default()).unwrap().ascii_only);
    }

    #[test]
    fn excel_sample_truncation_lands_on_a_char_boundary() {
        // A grid of multi-byte text truncated at an arbitrary byte budget:
        // slicing a String mid-character is a panic, not an error.
        let d = tempfile::TempDir::new().unwrap();
        let script = d.path().join("mk.py");
        std::fs::write(
            &script,
            "import sys, os\n\
             from openpyxl import Workbook\n\
             wb = Workbook(); ws = wb.active\n\
             for i in range(200): ws.append([\"日本語テキスト\" * 4, \"Grüße\" * 4, i])\n\
             wb.save(os.path.join(sys.argv[1], \"cjk.xlsx\"))\n",
        )
        .unwrap();
        let ok = std::process::Command::new("python3")
            .arg(&script)
            .arg(d.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // python3/openpyxl unavailable
        }
        let p = d.path().join("cjk.xlsx");
        for budget in [600usize, 601, 602, 603, 1024, 4096] {
            let s = build(&p, budget, Limits::default()).unwrap_or_else(|e| panic!("budget {budget}: {e:#}"));
            assert!(s.body.is_char_boundary(s.body.len()));
        }
    }

    #[test]
    fn a_directory_is_a_clear_error() {
        let d = tempfile::TempDir::new().unwrap();
        let err = build(d.path(), 1024, Limits::default()).unwrap_err();
        assert!(format!("{err:#}").contains("directory"));
    }

    #[test]
    fn an_empty_file_samples_to_nothing() {
        let d = tempfile::TempDir::new().unwrap();
        let p = write(&d, "e.csv", b"");
        let s = build(&p, 1024, Limits::default()).unwrap();
        assert!(s.body.is_empty());
        assert_eq!(s.bytes, 0);
    }
}
