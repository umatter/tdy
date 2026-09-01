//! The "what tdy sees" panel, against real files.
//!
//! This panel exists to answer one question — *which column of this file
//! supplies my declared column?* — and the answer gets written into a
//! `matches = '…'` clause. That fixes two things it must show: the file's
//! **own** header spelling (not tdy's sanitised one), and the **raw** value
//! (not the value after a type the user has not agreed to yet).
//!
//! It must also work for a member that does not fit, since that is the
//! member whose screen most needs it — and a refused member has no sidecar.

use std::path::{Path, PathBuf};

use tdy::config::Limits;
use tdy::spec::{ColumnSpec, DType, ParseSpec, ValueParsing};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("testdata")
        .join("drifting_exports")
}

/// The same projection `spawn_preview` builds. Kept here rather than exported
/// from the binary because it is two lines of intent, and the property worth
/// pinning is the *output*, not the code path.
fn as_the_file_spells_it(spec: &ParseSpec) -> ParseSpec {
    ParseSpec {
        extraction: spec.extraction.clone(),
        transforms: spec.transforms.clone(),
        columns: spec
            .columns
            .iter()
            .map(|c| ColumnSpec {
                name: c.source_name().to_string(),
                source: Some(c.source_name().to_string()),
                dtype: DType::Utf8,
                nullable: true,
                parse: ValueParsing::default(),
            })
            .collect(),
        confidence: None,
        notes: vec![],
    }
}

fn preview_of(file: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let limits = Limits::default();
    let sample = tdy::sample::build(file, 16 * 1024, limits).unwrap();
    let sniffed = tdy::sniff::sniff_opts(
        file,
        &sample,
        limits,
        tdy::sniff::SniffOpts { verify: false },
    )
    .unwrap()
    .spec;
    let batch = tdy::engine::preview(&as_the_file_spells_it(&sniffed), file, limits, 12).unwrap();
    let header = batch.schema().fields().iter().map(|f| f.name().clone()).collect();
    let rows = (0..batch.num_rows())
        .map(|i| {
            (0..batch.num_columns())
                .map(|c| {
                    let col = batch.column(c);
                    if col.is_null(i) {
                        String::new()
                    } else {
                        datafusion::arrow::util::display::array_value_to_string(col, i).unwrap()
                    }
                })
                .collect()
        })
        .collect();
    (header, rows)
}

/// A member with no sidecar — the refused case — still previews, because it
/// is framed by the sniffer rather than by a plan that does not exist.
#[test]
fn a_file_with_no_sidecar_still_previews() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("2025-11.csv");
    std::fs::copy(corpus().join("2025-11.csv"), &f).unwrap();
    assert!(
        !tdy::sidecar::sidecar_path(&f).exists(),
        "the point of this test is that there is no spec"
    );

    let (header, rows) = preview_of(&f);
    assert_eq!(header, vec!["Datum", "Betrag"]);
    assert_eq!(rows.len(), 4);
}

/// The header is the file's own spelling, so it can be pasted into a
/// `matches` clause. `Datum`, not `datum`.
#[test]
fn the_header_is_the_files_own_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("2025-09.xlsx");
    std::fs::copy(corpus().join("2025-09.xlsx"), &f).unwrap();

    let (header, _) = preview_of(&f);
    // This workbook's real header is `Betrag CHF` — capitals and a space,
    // which is exactly the thing a sanitised name would destroy and exactly
    // what the remedy has to quote.
    assert!(header.iter().any(|h| h == "Betrag CHF"), "{header:?}");
    assert!(header.iter().any(|h| h == "Datum"), "{header:?}");
}

/// The values are raw text. A Swiss amount reads `2'100.00`, not `2100.00`:
/// the reader is being shown the file, not tdy's reading of it.
#[test]
fn the_values_are_the_files_own_text() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("2025-11.csv");
    std::fs::copy(corpus().join("2025-11.csv"), &f).unwrap();

    let (_, rows) = preview_of(&f);
    assert_eq!(rows[0][0], "30.11.2025", "the date is as written: {:?}", rows[0]);
    assert_eq!(rows[0][1], "2'100.00", "the amount keeps its separator: {:?}", rows[0]);
}
